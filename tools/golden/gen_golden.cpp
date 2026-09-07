// Emits ground-truth byte layouts for the ESP-NOW wire structs.
//
// The two typedefs below are copied VERBATIM from
// ESP32DualBandWardriver/src/WiFiOps.h:66-82, and ENOW_TEXT_MAX from
// src/configs.h:53. Letting a real compiler lay them out is what makes the
// output authoritative: we assert the sizes rather than assume them.
//
//   c++ -std=c++17 -Wall -Wextra -o /tmp/gen_golden tools/golden/gen_golden.cpp
//
#include <cstdio>
#include <cstdint>
#include <cstring>
#include <cstddef>

#define ENOW_TEXT_MAX 200

typedef struct __attribute__((packed)) {
  char     magic[4];               // "ENOW"
  uint8_t  type;                   // MSG_TEXT
  uint32_t counter;                // heartbeat counter (valid for MSG_HEARTBEAT)
  uint16_t len;                    // number of bytes in text (not including NUL)
  char     text[ENOW_TEXT_MAX + 1];  // +1 for NUL terminator
} enow_text_msg_t;

typedef struct __attribute__((packed)) {
  char    magic[4];
  uint8_t type;               // MSG_ADMIN
  uint8_t assignment_version;
  uint8_t node_index;
  uint8_t node_count;
  uint8_t start_channel_idx;
  uint8_t end_channel_idx;
} enow_admin_msg_t;

static const char MAGIC[4] = {'E','N','O','W'};

static_assert(sizeof(enow_text_msg_t)  == 212, "text msg must be 212 bytes");
static_assert(sizeof(enow_admin_msg_t) ==  10, "admin msg must be 10 bytes");
static_assert(offsetof(enow_text_msg_t, counter) == 5,  "counter at 5");
static_assert(offsetof(enow_text_msg_t, len)     == 9,  "len at 9");
static_assert(offsetof(enow_text_msg_t, text)    == 11, "text at 11");

static void emit(const char* name, const void* p, size_t n) {
  const uint8_t* b = (const uint8_t*)p;
  printf("%s %zu ", name, n);
  for (size_t i = 0; i < n; i++) printf("%02x", b[i]);
  printf("\n");
}

int main() {
  // Mirrors WiFiOps.cpp:856-880 (sendHeartbeat): counter set, text left empty.
  {
    enow_text_msg_t m = {};
    memcpy(m.magic, MAGIC, 4);
    m.type = 3;              // MSG_HEARTBEAT
    m.counter = 0x12345678;
    emit("heartbeat", &m, sizeof(m));
  }
  // Mirrors WiFiOps.cpp:882-914 (broadcast text): a real WiFi wardrive line.
  {
    enow_text_msg_t m = {};
    memcpy(m.magic, MAGIC, 4);
    m.type = 4;              // MSG_TEXT
    m.counter = 0;
    const char* s = "AA:BB:CC:DD:EE:FF,My_Net,[WPA2_PSK],11,-42,W";
    m.len = (uint16_t)strlen(s);
    memcpy(m.text, s, m.len);
    emit("text_wifi", &m, sizeof(m));
  }
  // Mirrors WiFiOps.cpp:144 (BLE line): empty SSID, [BLE], channel 0, lowercase MAC.
  {
    enow_text_msg_t m = {};
    memcpy(m.magic, MAGIC, 4);
    m.type = 4;
    const char* s = "aa:bb:cc:dd:ee:ff,,[BLE],0,-70,B";
    m.len = (uint16_t)strlen(s);
    memcpy(m.text, s, m.len);
    emit("text_ble", &m, sizeof(m));
  }
  // Mirrors WiFiOps.cpp:683-696 (sendCoreRequest).
  {
    enow_text_msg_t m = {};
    memcpy(m.magic, MAGIC, 4);
    m.type = 1;              // MSG_CORE_REQUEST
    emit("core_request", &m, sizeof(m));
  }
  // Mirrors WiFiOps.cpp:646-653 (sendAdminToNodeSlot). Named `legacy_` because
  // wartui stopped speaking this shape in Phase 2: its own MSG_ADMIN is
  // fourteen bytes and carries a channel mask. The vector stays because the
  // frame is still out there — a stock core in the same room emits it, and
  // `air::is_legacy_admin` has to recognise it off real bytes rather than off
  // a reading of the header.
  {
    enow_admin_msg_t m = {};
    memcpy(m.magic, MAGIC, 4);
    m.type = 5;              // MSG_ADMIN
    m.assignment_version = 7;
    m.node_index = 2;
    m.node_count = 5;
    m.start_channel_idx = 16;
    m.end_channel_idx = 23;
    emit("legacy_admin", &m, sizeof(m));
  }
  return 0;
}
