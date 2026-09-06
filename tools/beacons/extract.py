#!/usr/bin/env python3
"""Turn a monitor-mode capture into beacon vectors for the parser tests.

`crates/wartui-proto/src/beacon.rs` reproduces a classification ladder the
vendor firmware wrote and never ran, so the tests around it are assembled
frames: they check the ladder against my reading of it. That is worth doing and
it is not the same as checking it against the air. This script closes that gap
the way `tools/espnow-sniffer` does for the ESP-NOW structs — by turning real
frames into permanent regression tests.

macOS can capture 802.11 management frames without any extra hardware, at the
cost of dropping off the network while it does:

    sudo tcpdump -I -i en0 -w /tmp/beacons.pcap -c 2000 'type mgt'
    tools/beacons/extract.py /tmp/beacons.pcap >> \\
        crates/wartui-proto/tests/beacon_vectors.txt

One frame per BSSID is kept, named after it, so a busy street does not produce
four hundred copies of the same access point. Radiotap headers are stripped:
`parse_mgmt` is handed the 802.11 frame itself, which is what promiscuous mode
delivers on the node.

Output is `<name> <length> <hex>`, the same format the ESP-NOW vectors use, so
the two files stay readable by the same eyes.

**Frames are scrubbed before they are printed.** A capture of the air around you
is a geolocation fingerprint: BSSIDs are exactly what WiGLE, Google and Apple
run their location databases on — the same data this project exists to collect —
SSIDs routinely carry surnames and house numbers, WPS elements carry device
names and serials, and a probe response is addressed to a real client device
nearby. None of that is what the parser is being tested on, so none of it needs
to be in the repository.

What survives is the shape: every element's tag and length at its original
offset, and the contents of the six the parser actually reads. What is replaced
is addresses, SSID bytes, and the contents of every element the parser ignores.
`--raw` skips all of it, for a fixture you are keeping to yourself.
"""

import struct
import sys

# libpcap link types. macOS gives radiotap; a capture from elsewhere may not.
DLT_IEEE802_11 = 105
DLT_IEEE802_11_RADIO = 127

BEACON, PROBE_RESPONSE = 8, 5

# Elements `crates/wartui-proto/src/beacon.rs` reads. Their contents are cipher
# suites and channel numbers — the substance of the test, and identifying of
# nobody — so they are kept as they arrived.
SSID, DS_PARAM, RSN, HT_OPERATION, WAPI, VENDOR = 0, 3, 48, 61, 68, 221

# Microsoft's OUI and type 1: the WPA element, which is the only vendor-specific
# element the parser looks inside. Every other one — WPS most of all, which
# carries a device name, a model and sometimes a serial — is replaced wholesale,
# OUI included, so the fixture does not name the hardware either.
WPA_OUI = bytes([0x00, 0x50, 0xF2, 0x01])

# Locally administered, and obviously not a real prefix.
FAKE_OUI = bytes([0x02, 0x00, 0x00])


def packets(blob):
    """Yield each packet body from a classic (non-pcapng) capture file."""
    magic = blob[:4]
    if magic in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1"):
        endian = "<"
    elif magic in (b"\xa1\xb2\xc3\xd4", b"\xa1\xb2\x3c\x4d"):
        endian = ">"
    else:
        sys.exit("not a pcap file; pcapng is not supported, use tcpdump -w")

    link_type = struct.unpack_from(endian + "I", blob, 20)[0]
    if link_type not in (DLT_IEEE802_11, DLT_IEEE802_11_RADIO):
        sys.exit(f"link type {link_type} is not 802.11; capture with tcpdump -I")

    at = 24
    while at + 16 <= len(blob):
        captured, _original = struct.unpack_from(endian + "II", blob, at + 8)
        body = blob[at + 16 : at + 16 + captured]
        at += 16 + captured
        if len(body) < captured:
            break
        if link_type == DLT_IEEE802_11_RADIO:
            if len(body) < 4:
                continue
            # Radiotap: version, pad, then its own little-endian length.
            body = body[struct.unpack_from("<H", body, 2)[0] :]
        yield body


def scrub_rsn(body):
    """Keep version, ciphers, AKMs and capabilities; zero anything past them.

    A beacon's RSN element does not normally carry a PMKID list, but the format
    allows one, and a PMKID is an HMAC over the access point and client
    addresses. The parser stops after the AKM suites, so zeroing the tail costs
    the test nothing and removes the one field here derived from a real MAC.
    """
    if len(body) < 8:
        return body
    at = 2 + 4  # version, group cipher
    for _ in range(2):  # pairwise suites, then AKM suites
        if at + 2 > len(body):
            return body
        count = int.from_bytes(body[at : at + 2], "little")
        at += 2 + count * 4
        if at > len(body):
            return body
    at += 2  # RSN capabilities
    return body[:at] + bytes(len(body) - at) if at < len(body) else body


def scrub(frame, index):
    """Replace everything identifying, keeping every offset where it was."""
    out = bytearray(frame)

    # All three addresses. addr1 on a probe response is a client device that
    # happened to be nearby, which is somebody else's business entirely.
    fake = FAKE_OUI + bytes([0x00, index >> 8 & 0xFF, index & 0xFF])
    for at in (4, 10, 16):
        out[at : at + 6] = fake

    at = 36
    while at + 2 <= len(out):
        tag, length = out[at], out[at + 1]
        body = slice(at + 2, at + 2 + length)
        if body.stop > len(out):
            break

        if tag == SSID:
            # Same length, so a hidden network stays hidden and an over-long
            # SSID still exercises the truncation path.
            name = f"ap-{index:03d}".encode()
            out[body] = (name + b"-" * length)[:length]
        elif tag == VENDOR and out[body][:4] != WPA_OUI:
            out[body] = b"\x00" * length
        elif tag == RSN:
            out[body] = scrub_rsn(bytes(out[body]))
        elif tag not in (DS_PARAM, HT_OPERATION, WAPI, VENDOR):
            out[body] = b"\x00" * length

        at = body.stop

    return bytes(out)


def main(path, raw):
    seen = {}
    for frame in packets(open(path, "rb").read()):
        if len(frame) < 36:
            continue
        fc0 = frame[0]
        if (fc0 >> 2) & 0x03 != 0 or (fc0 >> 4) & 0x0F not in (BEACON, PROBE_RESPONSE):
            continue
        bssid = frame[16:22]
        # Keep the longest frame per access point: the elements we care about
        # sit at the tail, so a clipped copy is the least useful one.
        if len(frame) > len(seen.get(bssid, b"")):
            seen[bssid] = frame

    for index, (bssid, frame) in enumerate(sorted(seen.items())):
        if raw:
            # Named after the real BSSID, which is the point of --raw and the
            # reason its output does not belong in the repository.
            print(f"beacon_{bssid.hex()} {len(frame)} {frame.hex()}")
        else:
            frame = scrub(frame, index)
            print(f"beacon_{index:03d} {len(frame)} {frame.hex()}")

    print(f"# {len(seen)} access points from {path}", file=sys.stderr)
    if raw:
        print("# WARNING: --raw output identifies real networks", file=sys.stderr)


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "--raw"]
    if len(args) != 1:
        sys.exit(f"usage: {sys.argv[0]} [--raw] <capture.pcap>")
    main(args[0], raw="--raw" in sys.argv[1:])
