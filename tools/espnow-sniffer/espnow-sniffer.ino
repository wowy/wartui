// Phase 0 sniffer: prove the fleet is audible, and diagnose it when it is not.
//
// Listens two ways at once, which is the point:
//
//   1. The ESP-NOW receive callback — exactly what the wartui bridge will get.
//      The radio only delivers frames addressed to us or to broadcast, so an
//      encrypted fleet (which unicasts node -> core) is invisible here.
//
//   2. Promiscuous mode — every 802.11 frame on the channel regardless of who
//      it is addressed to. This sees the encrypted fleet's traffic too.
//
// Comparing the two counters tells you which situation you are in without
// having to go and read the node's settings:
//
//   promiscuous > 0, esp-now == 0   -> traffic is unicast: encryption is ON
//   promiscuous == 0                -> nothing on this channel: wrong channel,
//                                      out of range, or nothing transmitting
//   both > 0                        -> plaintext fleet, working as wartui needs
//
// Capture with:
//   pio device monitor -b 115200 | tee /tmp/capture.txt
//
// `capture_*` lines are wartui's own frames and `vendor_*` lines are somebody
// else's, both as `<name> <length> <hex>`. There is no golden-vector fixture to
// paste them into any more -- the wire format has one implementation of each
// end, so `crates/wartui-proto/tests/wire.rs` writes its vectors out by hand --
// but the hex is still the fastest way to see what a fleet is actually saying.

#include <WiFi.h>
#include <esp_now.h>
#include <esp_wifi.h>

// `static constexpr uint8_t ESPNOW_CHANNEL = 6;` — src/WiFiOps.cpp:15.
// Identical on main and feat/node-interference-mitigation.
static const uint8_t MESH_CHANNEL = 6;

// wartui's own frames, and the vendor's. Both are recognised because both are
// worth capturing: one is the fleet under test and the other is whatever else
// is on the channel it has to share.
static const char WARTUI_MAGIC[4] = {'W', 'T', 'U', 'I'};
static const char VENDOR_MAGIC[4] = {'E', 'N', 'O', 'W'};

// Espressif's OUI, which tags an action frame as ESP-NOW.
static const uint8_t ESPRESSIF_OUI[3] = {0x18, 0xFE, 0x34};

// 802.11 management/action frame layout, ahead of the ESP-NOW body:
//   24  MAC header
//    1  category (127, vendor specific)
//    3  OUI
//    4  random values
//    1  element ID (221)   1  length   3  OUI   1  type (4)   1  version
//
// The 4-byte random-values field is easy to miss. Omitting it put this at 35,
// which shifted every promiscuous capture by four bytes so the magic check
// failed on frames that were in fact plaintext -- making the encrypted-versus-
// absent diagnostic report "encrypted" for everything. Caught by comparing the
// two receive paths on real hardware: promiscuous said 216 bytes where the
// ESP-NOW callback said 212.
static const int ESPNOW_BODY_OFFSET = 39;
static const int FCS_LEN = 4;

struct Capture {
  uint8_t src[6];
  uint8_t dst[6];
  int8_t rssi;
  uint8_t channel;
  uint16_t body_len;
  uint8_t body[256];
  // 'w' for one of ours, 'v' for the vendor's, 0 for neither.
  char fleet;
  bool via_espnow;  // false means only promiscuous mode saw it
  // 802.11 retry flag and sequence number, promiscuous captures only. A run of
  // frames sharing one sequence number with the retry bit set is one logical
  // transmission the radio kept resending because nothing acknowledged it --
  // which is very different from the sender having looped.
  bool retry;
  uint16_t seq;
};

// Single producer (the Wi-Fi task) and single consumer (loop), so volatile
// indices are enough. Printing from the radio callback risks tripping the
// watchdog, so captures are queued and drained from loop() instead.
// Deep enough for the burst a node emits in its first sweep after boot, when
// nothing is in its dedup ring yet and every access point on a channel is
// reported back to back. A trickle-sized queue drops exactly the frames worth
// capturing; `dropped` in the alive line says if this is still too small.
static const uint8_t QUEUE_LEN = 48;
static Capture queue[QUEUE_LEN];
static volatile uint8_t q_head = 0;
static volatile uint8_t q_tail = 0;
static volatile uint32_t dropped = 0;

// Who is transmitting ESP-NOW, and whether anything acknowledges them.
//
// An 802.11 ACK names only the station being acknowledged, so an ACK whose
// receiver address is the core's MAC means the node did answer the core's
// unicast. That separates "nobody received it" from "it was received but the
// acknowledgement never got back", which look identical from the retry bit
// alone and have very different consequences.
struct Talker {
  uint8_t mac[6];
  uint32_t frames;
  uint32_t acks;
  bool used;
};
static const uint8_t MAX_TALKERS = 8;
static Talker talkers[MAX_TALKERS];

static void noteTalker(const uint8_t *mac) {
  for (uint8_t i = 0; i < MAX_TALKERS; i++) {
    if (talkers[i].used && memcmp(talkers[i].mac, mac, 6) == 0) {
      talkers[i].frames++;
      return;
    }
  }
  for (uint8_t i = 0; i < MAX_TALKERS; i++) {
    if (!talkers[i].used) {
      memcpy(talkers[i].mac, mac, 6);
      talkers[i].frames = 1;
      talkers[i].used = true;
      return;
    }
  }
}

static void noteAck(const uint8_t *receiver) {
  for (uint8_t i = 0; i < MAX_TALKERS; i++) {
    if (talkers[i].used && memcmp(talkers[i].mac, receiver, 6) == 0) {
      talkers[i].acks++;
      return;
    }
  }
}

// Total acknowledgements seen, whoever they were for. This is the control
// that makes a zero per-transmitter count meaningful: without it, "nobody
// acknowledged" and "no acknowledgement ever reached this callback" are the
// same reading, and the second is far more likely to be a mistake of mine.
static volatile uint32_t ack_frames = 0;

static volatile uint32_t espnow_frames = 0;
static volatile uint32_t promisc_frames = 0;
static volatile uint32_t foreign_action = 0;
static uint32_t capture_seq = 0;

// Park the radio the way the firmware does (src/WiFiOps.cpp:586-620).
static void setFixedChannel(uint8_t ch) {
  esp_wifi_set_ps(WIFI_PS_NONE);
  esp_wifi_set_promiscuous(false);
  esp_wifi_set_channel(ch, WIFI_SECOND_CHAN_NONE);
  esp_wifi_set_promiscuous(true);
}

static void enqueue(const uint8_t *src, const uint8_t *dst, int8_t rssi, uint8_t channel,
                    const uint8_t *body, int body_len, bool via_espnow, bool retry,
                    uint16_t seq) {
  uint8_t next = (uint8_t)((q_head + 1) % QUEUE_LEN);
  if (next == q_tail) {
    dropped++;
    return;
  }
  Capture &c = queue[q_head];
  memcpy(c.src, src, 6);
  if (dst) {
    memcpy(c.dst, dst, 6);
  } else {
    memset(c.dst, 0, 6);
  }
  c.rssi = rssi;
  c.channel = channel;
  if (body_len < 0) body_len = 0;
  if (body_len > (int)sizeof(c.body)) body_len = sizeof(c.body);
  c.body_len = (uint16_t)body_len;
  memcpy(c.body, body, c.body_len);
  c.fleet = 0;
  if (c.body_len >= 4) {
    if (memcmp(c.body, WARTUI_MAGIC, 4) == 0) {
      c.fleet = 'w';
    } else if (memcmp(c.body, VENDOR_MAGIC, 4) == 0) {
      c.fleet = 'v';
    }
  }
  c.via_espnow = via_espnow;
  c.retry = retry;
  c.seq = seq;
  q_head = next;
}

// What the wartui bridge will see: broadcast, or addressed to us.
static void onEspNowRecv(const esp_now_recv_info_t *info, const uint8_t *data, int len) {
  espnow_frames++;
  const int8_t rssi = (info && info->rx_ctrl) ? (int8_t)info->rx_ctrl->rssi : 0;
  uint8_t channel = 0;
  wifi_second_chan_t second;
  esp_wifi_get_channel(&channel, &second);
  enqueue(info->src_addr, info->des_addr, rssi, channel, data, len, true, false, 0);
}

// Everything on the air, whoever it is addressed to.
static void onPromiscuous(void *buf, wifi_promiscuous_pkt_type_t type) {
  const wifi_promiscuous_pkt_t *pkt = (const wifi_promiscuous_pkt_t *)buf;
  const uint8_t *p = pkt->payload;
  const int len = pkt->rx_ctrl.sig_len;

  // Acknowledgement: subtype 13 of the control type, carrying only the address
  // of the station being acknowledged. Counted, never queued -- a busy channel
  // produces far too many to print.
  if (type == WIFI_PKT_CTRL) {
    if (len >= 10 && p[0] == 0xD4) {
      ack_frames++;
      noteAck(&p[4]);
    }
    return;
  }
  if (type != WIFI_PKT_MGMT) return;

  // Action frame: protocol version 0, type management, subtype 13.
  if (len < ESPNOW_BODY_OFFSET + FCS_LEN || p[0] != 0xD0) return;

  // Category 127 (vendor specific) followed by Espressif's OUI.
  if (p[24] != 127 || memcmp(&p[25], ESPRESSIF_OUI, 3) != 0) {
    foreign_action++;
    return;
  }

  promisc_frames++;
  noteTalker(&p[10]);
  const int body_len = len - ESPNOW_BODY_OFFSET - FCS_LEN;
  // Frame Control bit 11 is Retry; sequence control sits at bytes 22-23 with
  // the sequence number in the top 12 bits.
  const bool retry = (p[1] & 0x08) != 0;
  const uint16_t seq = (uint16_t)((p[22] | (p[23] << 8)) >> 4);
  enqueue(&p[10], &p[4], (int8_t)pkt->rx_ctrl.rssi, pkt->rx_ctrl.channel,
          &p[ESPNOW_BODY_OFFSET], body_len, false, retry, seq);
}

static void formatMac(const uint8_t *m, char *out) {
  snprintf(out, 18, "%02X:%02X:%02X:%02X:%02X:%02X", m[0], m[1], m[2], m[3], m[4], m[5]);
}

static void drainQueue() {
  while (q_tail != q_head) {
    const Capture &c = queue[q_tail];
    char src[18], dst[18];
    formatMac(c.src, src);
    formatMac(c.dst, dst);

    const bool broadcast = (memcmp(c.dst, "\xFF\xFF\xFF\xFF\xFF\xFF", 6) == 0);
    if (c.via_espnow) {
      Serial.printf("# from=%s to=%s%s rssi=%d ch=%u len=%u via=esp-now\n", src, dst,
                    broadcast ? " (broadcast)" : " (unicast)", c.rssi, c.channel, c.body_len);
    } else {
      Serial.printf("# from=%s to=%s%s rssi=%d ch=%u len=%u via=promiscuous seq=%u%s\n", src,
                    dst, broadcast ? " (broadcast)" : " (unicast)", c.rssi, c.channel,
                    c.body_len, c.seq, c.retry ? " RETRY" : "");
    }

    if (c.fleet == 'w') {
      // wartui: magic, wire version, type, then the body.
      Serial.printf("#   wartui v%u type=0x%02x\n", c.body_len > 4 ? c.body[4] : 0,
                    c.body_len > 5 ? c.body[5] : 0);
      Serial.printf("capture_%04lu %u ", (unsigned long)++capture_seq, c.body_len);
      for (uint16_t i = 0; i < c.body_len; i++) Serial.printf("%02x", c.body[i]);
      Serial.println();
    } else if (c.fleet == 'v') {
      // The vendor's, whose type byte sits where our version byte does.
      Serial.printf("#   vendor type=%u\n", c.body_len > 4 ? c.body[4] : 0);
      Serial.printf("vendor_%04lu %u ", (unsigned long)++capture_seq, c.body_len);
      for (uint16_t i = 0; i < c.body_len; i++) Serial.printf("%02x", c.body[i]);
      Serial.println();
    } else {
      Serial.println("#   body starts with neither \"WTUI\" nor \"ENOW\" -- an encrypted "
                     "payload, or another ESP-NOW application nearby");
    }
    q_tail = (uint8_t)((q_tail + 1) % QUEUE_LEN);
  }
}

static void report(uint8_t channel) {
  Serial.printf("# alive: ch=%u  esp-now=%lu  promiscuous=%lu  other-action=%lu  acks=%lu  "
                "dropped=%lu  uptime=%lus\n",
                channel, (unsigned long)espnow_frames, (unsigned long)promisc_frames,
                (unsigned long)foreign_action, (unsigned long)ack_frames,
                (unsigned long)dropped, (unsigned long)(millis() / 1000));

  if (ack_frames == 0) {
    Serial.println("#   NO acknowledgements captured at all, so the per-transmitter counts "
                   "below mean nothing -- control-frame capture is not working");
  }

  for (uint8_t i = 0; i < MAX_TALKERS; i++) {
    if (!talkers[i].used) continue;
    char mac[18];
    formatMac(talkers[i].mac, mac);
    Serial.printf("#   %s sent %lu ESP-NOW frames, acknowledged %lu times\n", mac,
                  (unsigned long)talkers[i].frames, (unsigned long)talkers[i].acks);
  }

  if (promisc_frames == 0) {
    Serial.println("#   nothing on channel 6 yet: nothing is transmitting, or out of "
                   "range (nodes drop to 2 dBm while wardriving)");
  } else if (espnow_frames == 0) {
    Serial.println("#   ESP-NOW traffic IS present but the receive callback never fired, so "
                   "it is unicast: the fleet has encryption ENABLED, which wartui does not "
                   "support. Turn it off in each node's web UI.");
  }
}

void setup() {
  Serial.begin(115200);
  delay(2000);  // Let the USB CDC host attach, without blocking on it.

  Serial.println("# wartui Phase 0 ESP-NOW sniffer");
  Serial.println("# listening only; it registers no peers and transmits nothing");

  WiFi.mode(WIFI_STA);
  WiFi.disconnect();

  if (esp_now_init() != ESP_OK) {
    Serial.println("# FATAL: esp_now_init failed");
    return;
  }
  esp_now_register_recv_cb(onEspNowRecv);

  wifi_promiscuous_filter_t filter = {};
  // Action frames carry ESP-NOW; control frames carry the acknowledgements.
  filter.filter_mask = WIFI_PROMIS_FILTER_MASK_MGMT | WIFI_PROMIS_FILTER_MASK_CTRL;
  esp_wifi_set_promiscuous_filter(&filter);

  // Control frames are gated a second time by their own subtype filter, and
  // setting only the mask above delivers none of them. Missing this made an
  // earlier capture report zero acknowledgements from an instrument that was
  // never switched on.
  wifi_promiscuous_filter_t ctrl_filter = {};
  ctrl_filter.filter_mask = WIFI_PROMIS_CTRL_FILTER_MASK_ACK;
  esp_wifi_set_promiscuous_ctrl_filter(&ctrl_filter);
  esp_wifi_set_promiscuous_rx_cb(onPromiscuous);
  setFixedChannel(MESH_CHANNEL);

  uint8_t mac[6] = {0};
  esp_wifi_get_mac(WIFI_IF_STA, mac);
  char self[18];
  formatMac(mac, self);
  Serial.printf("# sniffer MAC %s on channel %u\n", self, MESH_CHANNEL);
  Serial.println("# ready");
}

void loop() {
  drainQueue();

  static uint32_t last_report = 0;
  if (millis() - last_report > 10000) {
    last_report = millis();
    uint8_t channel = 0;
    wifi_second_chan_t second;
    esp_wifi_get_channel(&channel, &second);
    report(channel);
  }

  delay(1);
}
