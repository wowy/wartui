// The P4 half of the C6's first flash (#395). It pushes the C6 loader image to the factory C6
// through ESP-Hosted's slave OTA over SDIO, resets the C6 into it, and relays the C6 loader's
// lines from the link UART to the host as `wartui-loader: ` lines on the console. It never
// restarts itself: every reset reruns it, so a restart loop would push the image without end.
#include <inttypes.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <string.h>

#include "driver/gpio.h"
#include "driver/uart.h"
#include "esp_attr.h"
#include "esp_err.h"
#include "esp_event.h"
#include "esp_hosted.h"
#include "esp_hosted_api_types.h"
#include "esp_hosted_ota.h"
#include "esp_system.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "nvs_flash.h"

extern const uint8_t image_start[] asm("_binary_c6_loader_bin_start");
extern const uint8_t image_end[] asm("_binary_c6_loader_bin_end");

#define CHUNK 1500
#define C6_RESET 54
// The link UART. Both pins are SDIO lines too, so UART1 and ESP-Hosted take turns on them.
#define LINK_RX 14
#define LINK_TX 19
#define PRE_LISTEN_MS 1500
#define VERDICT_S 120

// What the C6 loader prefixes each of its lines with.
#define C6_PREFIX "c6-loader: "

// Counts the attempts that reached `esp_hosted_init`, across resets that keep RTC memory. The
// magic sits in the upper 24 bits, so a power-on's random contents almost never read as an
// attempt; the low byte is the count.
#define ATTEMPT_MAGIC 0x57A7C600u
#define ATTEMPT_MASK 0xFFFFFF00u
static RTC_NOINIT_ATTR uint32_t attempts;

// Sends one whole line to the host. A single printf per line keeps another task's log from
// landing inside it.
static void say(const char *fmt, ...) {
    char text[300];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(text, sizeof text, fmt, ap);
    va_end(ap);
    printf("wartui-loader: %s\n", text);
    fflush(stdout);
}

static void idle(void) {
    for (;;) vTaskDelay(pdMS_TO_TICKS(1000));
}

static void link_install(void) {
    const uart_config_t cfg = {
        .baud_rate = 115200,
        .data_bits = UART_DATA_8_BITS,
        .parity = UART_PARITY_DISABLE,
        .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    ESP_ERROR_CHECK(uart_driver_install(UART_NUM_1, 4096, 0, 0, NULL, 0));
    ESP_ERROR_CHECK(uart_param_config(UART_NUM_1, &cfg));
    ESP_ERROR_CHECK(
        uart_set_pin(UART_NUM_1, LINK_TX, LINK_RX, UART_PIN_NO_CHANGE, UART_PIN_NO_CHANGE));
}

// Hands the shared pins back before SDIO takes them.
static void link_release(void) {
    uart_driver_delete(UART_NUM_1);
    gpio_reset_pin(LINK_RX);
    gpio_reset_pin(LINK_TX);
}

typedef struct {
    char buf[256];
    size_t len;
    bool overflow;
} line_reader_t;

// Reads the link UART until a whole line arrives, and returns it without its line ending, or
// returns NULL at the deadline. A line too long for the buffer is dropped whole.
static const char *read_line(line_reader_t *r, TickType_t deadline) {
    for (;;) {
        TickType_t now = xTaskGetTickCount();
        if ((int32_t)(deadline - now) <= 0) return NULL;
        TickType_t wait = deadline - now;
        if (wait > pdMS_TO_TICKS(100)) wait = pdMS_TO_TICKS(100);
        uint8_t c;
        if (uart_read_bytes(UART_NUM_1, &c, 1, wait) <= 0) continue;
        if (c == '\n') {
            bool dropped = r->overflow;
            size_t len = r->len;
            r->len = 0;
            r->overflow = false;
            if (dropped) continue;
            if (len > 0 && r->buf[len - 1] == '\r') len--;
            r->buf[len] = '\0';
            return r->buf;
        }
        if (r->len < sizeof r->buf - 1) {
            r->buf[r->len++] = (char)c;
        } else {
            r->overflow = true;
        }
    }
}

// The text after the C6 loader's prefix, or NULL for any other line. The prefix may follow a
// stray byte from the C6's reset, so it is searched for rather than matched at the start.
static char *c6_text(const char *line) {
    char *at = strstr(line, C6_PREFIX);
    if (at == NULL) return NULL;
    char *text = at + strlen(C6_PREFIX);
    for (char *p = text; *p; p++) {
        if (*p < 0x20 || *p > 0x7e) *p = '?';
    }
    return text;
}

// Relays one C6 loader line, and returns true when it was the verdict.
static bool relay_line(const char *text) {
    say("c6 %s", text);
    if (strncmp(text, "done", 4) == 0) {
        say("done");
        return true;
    }
    if (strncmp(text, "failed ", 7) == 0) {
        say("failed c6 loader: %s", text + 7);
        return true;
    }
    return false;
}

static void relay(line_reader_t *r) {
    TickType_t deadline = xTaskGetTickCount() + pdMS_TO_TICKS(VERDICT_S * 1000);
    const char *line;
    while ((line = read_line(r, deadline)) != NULL) {
        char *text = c6_text(line);
        if (text != NULL && relay_line(text)) return;
    }
    say("failed no verdict from the C6 loader in %d s", VERDICT_S);
}

static void reset_c6(void) {
    gpio_reset_pin(C6_RESET);
    gpio_set_direction(C6_RESET, GPIO_MODE_OUTPUT);
    gpio_set_level(C6_RESET, 0);
    vTaskDelay(pdMS_TO_TICKS(100));
    gpio_set_level(C6_RESET, 1);
}

// Pushes the image. Returns false, having said why, when a call fails.
static bool push_image(bool activate) {
    esp_err_t err = esp_hosted_slave_ota_begin();
    if (err != ESP_OK) {
        say("failed ota_begin: %s", esp_err_to_name(err));
        return false;
    }
    size_t len = image_end - image_start;
    int said = 0;
    say("sending 0");
    for (size_t off = 0; off < len; off += CHUNK) {
        size_t n = len - off < CHUNK ? len - off : CHUNK;
        err = esp_hosted_slave_ota_write((uint8_t *)image_start + off, n);
        if (err != ESP_OK) {
            say("failed ota_write at %u: %s", (unsigned)off, esp_err_to_name(err));
            esp_hosted_slave_ota_end();
            return false;
        }
        int pct = (int)((off + n) * 100 / len);
        if (pct / 10 > said / 10) {
            said = pct / 10 * 10;
            say("sending %d", said);
        }
    }
    err = esp_hosted_slave_ota_end();
    if (err != ESP_OK) {
        say("failed ota_end: %s", esp_err_to_name(err));
        return false;
    }
    if (activate) {
        err = esp_hosted_slave_ota_activate();
        if (err != ESP_OK) {
            say("failed ota_activate: %s", esp_err_to_name(err));
            return false;
        }
    }
    say("sent");
    return true;
}

void app_main(void) {
    vTaskDelay(pdMS_TO_TICKS(2000));  // time for a listener to attach
    say("start");

    // A C6 loader that is already talking is mid-run after a P4 reset. `esp_hosted_init` would
    // reset the C6 under it, so listen first.
    line_reader_t reader = {0};
    link_install();
    TickType_t listen_end = xTaskGetTickCount() + pdMS_TO_TICKS(PRE_LISTEN_MS);
    const char *line;
    while ((line = read_line(&reader, listen_end)) != NULL) {
        char *text = c6_text(line);
        if (text == NULL) continue;
        if (!relay_line(text)) relay(&reader);
        idle();
    }
    link_release();

    // ESP-Hosted restarts the P4 when it finds no slave. Without this guard that restart would
    // rerun the attempt forever; a reset the operator makes is a power-on or the reset pin.
    esp_reset_reason_t reason = esp_reset_reason();
    bool host_reset = reason != ESP_RST_POWERON && reason != ESP_RST_EXT;
    bool recorded = (attempts & ATTEMPT_MASK) == ATTEMPT_MAGIC && (attempts & 0xFF) > 0;
    if (host_reset && recorded) {
        say("skipped no ESP-Hosted on the C6 (the host restarted the P4)");
        idle();
    }
    uint32_t count = recorded ? (attempts & 0xFF) : 0;
    attempts = ATTEMPT_MAGIC | (count < 0xFF ? count + 1 : 0xFF);

    esp_err_t err = nvs_flash_init();
    if (err == ESP_ERR_NVS_NO_FREE_PAGES || err == ESP_ERR_NVS_NEW_VERSION_FOUND) {
        nvs_flash_erase();
        nvs_flash_init();
    }
    esp_event_loop_create_default();
    esp_hosted_init();
    if (esp_hosted_connect_to_slave() != ESP_OK) {
        say("skipped no ESP-Hosted on the C6");
        idle();
    }

    // The factory C6 does not answer the version request, so its OTA goes without `activate`,
    // which ESP-Hosted adds in 2.6.
    esp_hosted_coprocessor_fwver_t ver = {0};
    bool known = esp_hosted_get_coprocessor_fwversion(&ver) == 0;
    if (known) {
        say("hosted %" PRIu32 ".%" PRIu32 ".%" PRIu32, ver.major1, ver.minor1, ver.patch1);
    } else {
        say("hosted unknown");
    }
    bool activate = known && (ver.major1 > 2 || (ver.major1 == 2 && ver.minor1 >= 6));
    if (!push_image(activate)) idle();

    vTaskDelay(pdMS_TO_TICKS(500));
    esp_hosted_deinit();
    // Listening before the reset catches the C6 loader's first line.
    reader = (line_reader_t){0};
    link_install();
    reset_c6();
    relay(&reader);
    idle();
}
