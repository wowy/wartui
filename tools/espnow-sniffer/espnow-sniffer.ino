// Phase 0 sniffer: prove the fleet is audible before writing any Rust.
//
// Parks an ESP32 on the mesh's ESP-NOW channel and dumps every frame it hears
// as a golden-vector line. Purely passive — it registers no peers and transmits
// nothing, so it cannot disturb a running fleet.
//
// It answers two questions at once:
//   1. Can a dongle on your desk actually hear nodes transmitting at 2 dBm?
//      (`resolveTxPowerDbm` drops NODE and CORE to 2 dBm while wardriving.)
//   2. Do real frames match the byte layout wartui-proto encodes?
//
// Capture with:
//   pio device monitor -b 115200 | tee /tmp/capture.txt
// then append the `capture_*` lines to
// crates/wartui-proto/tests/golden_vectors.txt and re-run `cargo test`.

#include <WiFi.h>
#include <esp_now.h>
#include <esp_wifi.h>

// `static constexpr uint8_t ESPNOW_CHANNEL = 6;` — src/WiFiOps.cpp:15.
static const uint8_t ESPNOW_CHANNEL = 6;

static const char MAGIC[4] = {'E', 'N', 'O', 'W'};

static uint32_t frame_count = 0;
static uint32_t foreign_count = 0;

// Park the radio the same way the firmware does (src/WiFiOps.cpp:586-620):
// promiscuous on, set channel, promiscuous off, power save disabled.
static void setFixedChannel(uint8_t ch) {
  esp_wifi_set_ps(WIFI_PS_NONE);
  esp_wifi_set_promiscuous(true);
  esp_wifi_set_channel(ch, WIFI_SECOND_CHAN_NONE);
  esp_wifi_set_promiscuous(false);
}

static void onRecv(const esp_now_recv_info_t *info, const uint8_t *data, int len) {
  const int rssi = (info && info->rx_ctrl) ? info->rx_ctrl->rssi : 0;

  char src[18];
  snprintf(src, sizeof(src), "%02X:%02X:%02X:%02X:%02X:%02X", info->src_addr[0],
           info->src_addr[1], info->src_addr[2], info->src_addr[3], info->src_addr[4],
           info->src_addr[5]);

  // Anything without the preamble belongs to some other ESP-NOW user nearby.
  if (len < 5 || memcmp(data, MAGIC, 4) != 0) {
    foreign_count++;
    Serial.printf("# non-ENOW frame from %s rssi=%d len=%d\n", src, rssi, len);
    return;
  }

  frame_count++;
  Serial.printf("# from=%s rssi=%d type=%u len=%d\n", src, rssi, data[4], len);
  Serial.printf("capture_%04lu %d ", (unsigned long)frame_count, len);
  for (int i = 0; i < len; i++) {
    Serial.printf("%02x", data[i]);
  }
  Serial.println();
}

void setup() {
  Serial.begin(115200);
  delay(2000);  // Give the USB CDC host time to attach without blocking on it.

  Serial.println("# wartui Phase 0 ESP-NOW sniffer");
  Serial.printf("# listening on channel %u, transmitting nothing\n", ESPNOW_CHANNEL);

  WiFi.mode(WIFI_STA);
  WiFi.disconnect();
  setFixedChannel(ESPNOW_CHANNEL);

  if (esp_now_init() != ESP_OK) {
    Serial.println("# FATAL: esp_now_init failed");
    return;
  }
  esp_now_register_recv_cb(onRecv);

  uint8_t mac[6] = {0};
  esp_wifi_get_mac(WIFI_IF_STA, mac);
  Serial.printf("# sniffer MAC %02X:%02X:%02X:%02X:%02X:%02X\n", mac[0], mac[1], mac[2], mac[3],
                mac[4], mac[5]);
  Serial.println("# ready");
}

void loop() {
  // A heartbeat of our own, so silence is distinguishable from a hung sketch.
  static uint32_t last = 0;
  if (millis() - last > 10000) {
    last = millis();
    Serial.printf("# alive: %lu ENOW frames, %lu foreign, uptime %lus\n",
                  (unsigned long)frame_count, (unsigned long)foreign_count,
                  (unsigned long)(millis() / 1000));
  }
  delay(10);
}
