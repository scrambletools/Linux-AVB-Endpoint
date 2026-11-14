"""The far end of a veth pair for load.sh: captures what load_talker
sends, and can hold a MAAP block there, announcing it every 300 ms from
the interface's address. On SIGTERM it writes what it saw as JSON.

    python3 -I load-capture.py capture <interface> <json file> [<start> <count> <seconds>]
    python3 -I load-capture.py check <json file> <load_talker output> <streams>
                                     <talker MAC> (maap [<start> <count>] | <MAC>)
    python3 -I load-capture.py first <talker MAC> <count>

A block's start is its first address in hexadecimal, without colons. The
block is announced for its first seconds only, so that load_talker hears
it while it listens and must remember it. `first` gives the block
load_talker's claim tries first, as atdecc's MAAP picks it from the
claim's address, whose last three octets are the interface's.
"""

import json
import signal
import socket
import struct
import sys
import threading
import time

AVTP = 0x22F0
VLAN = 0x8100
AAF = 0x02
MAAP = 0xFE
MAAP_DESTINATION = bytes.fromhex("91e0f000ff00")
POOL_START = 0x91E0F0000000
POOL_SIZE = 0xFE00
ANNOUNCE = 3
SOL_PACKET = 263
PACKET_AUXDATA = 8
TP_STATUS_VLAN_VALID = 0x10
# 8 channels of 6 samples, 32 bits each, after AAF's 24-octet header.
AAF_PDU = 24 + 8 * 6 * 4
FIRST_UNIQUE = 0xFFE0


def mac_of(interface):
    with open(f"/sys/class/net/{interface}/address") as file:
        return bytes.fromhex(file.read().strip().replace(":", ""))


def maap_pdu(message_type, start, count):
    pdu = bytearray(28)
    pdu[0] = MAAP
    pdu[1] = message_type
    pdu[2:4] = ((1 << 11) | 16).to_bytes(2, "big")
    pdu[12:18] = start.to_bytes(6, "big")
    pdu[18:20] = count.to_bytes(2, "big")
    return bytes(pdu)


def capture(interface, path, block):
    mac = mac_of(interface)
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(0x0003))
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
    sock.setsockopt(SOL_PACKET, PACKET_AUXDATA, 1)
    sock.bind((interface, 0))
    sock.settimeout(0.1)
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    if block:
        start, count, seconds = block
        until = time.monotonic() + seconds
        frame = MAAP_DESTINATION + mac + AVTP.to_bytes(2, "big") + maap_pdu(ANNOUNCE, start, count)

        def announce():
            sender = socket.socket(socket.AF_PACKET, socket.SOCK_RAW)
            sender.bind((interface, 0))
            while not stop.is_set() and time.monotonic() < until:
                sender.send(frame)
                time.sleep(0.3)

        threading.Thread(target=announce, daemon=True).start()
    streams = {}
    maap = []
    other = 0
    while not stop.is_set():
        try:
            data, ancillary, _, address = sock.recvmsg(2048, socket.CMSG_SPACE(32))
        except (TimeoutError, InterruptedError):
            continue
        if address[2] == socket.PACKET_OUTGOING:
            continue
        tci = None
        for level, kind, value in ancillary:
            if level == SOL_PACKET and kind == PACKET_AUXDATA and len(value) >= 20:
                status, _, _, _, _, vlan_tci, _ = struct.unpack("IIIHHHH", value[:20])
                if status & TP_STATUS_VLAN_VALID:
                    tci = vlan_tci
        ethertype = int.from_bytes(data[12:14], "big")
        payload = data[14:]
        if ethertype == VLAN:
            tci = int.from_bytes(data[14:16], "big")
            ethertype = int.from_bytes(data[16:18], "big")
            payload = data[18:]
        if ethertype != AVTP or not payload:
            other += 1
            continue
        if payload[0] == MAAP and len(payload) >= 28:
            maap.append(
                {
                    "source": data[6:12].hex(),
                    "type": payload[1] & 0x0F,
                    "start": int.from_bytes(payload[12:18], "big"),
                    "count": int.from_bytes(payload[18:20], "big"),
                }
            )
            continue
        if payload[0] != AAF or len(payload) < 12:
            other += 1
            continue
        key = payload[4:12].hex()
        seen = streams.setdefault(
            key,
            {"frames": 0, "destinations": [], "tags": [], "lengths": [], "flags": []},
        )
        seen["frames"] += 1
        for name, value in (
            ("destinations", data[0:6].hex()),
            ("tags", tci),
            ("lengths", len(payload)),
            ("flags", payload[1] & 0x81),
        ):
            if value not in seen[name]:
                seen[name].append(value)
    with open(path, "w") as file:
        json.dump({"streams": streams, "maap": maap, "other": other}, file)


def fail(message):
    print(f"FAIL: {message}")
    sys.exit(1)


def check(path, output_path, streams, talker, dest, block):
    with open(path) as file:
        seen = json.load(file)
    with open(output_path) as file:
        lines = file.read().splitlines()
    talker = bytes.fromhex(talker.replace(":", ""))
    sent = {}
    for line in lines:
        words = line.replace(",", "").replace(":", "").split()
        if len(words) >= 9 and words[0] == "stream" and words[3] == "sent":
            sent[words[2]] = (int(words[4]), int(words[6]), int(words[8]))
    ids = [
        (int.from_bytes(talker, "big") << 16 | (FIRST_UNIQUE + place)).to_bytes(8, "big").hex()
        for place in range(streams)
    ]
    if sorted(seen["streams"]) != sorted(ids):
        fail(f"streams seen {sorted(seen['streams'])}, not {ids}")
    firsts = []
    for place, key in enumerate(ids):
        stream = seen["streams"][key]
        if key not in sent:
            fail(f"no counts printed for {key}")
        count, late, refused = sent[key]
        if refused:
            fail(f"{key}: {refused} refused")
        if stream["frames"] != count:
            fail(f"{key}: {stream['frames']} frames arrived, {count} sent")
        if count < 8000:
            fail(f"{key}: only {count} sent")
        if stream["tags"] != [0x6002]:
            fail(f"{key}: tags {stream['tags']}, not PCP 3 on VLAN 2")
        if stream["lengths"] != [AAF_PDU]:
            fail(f"{key}: PDUs of {stream['lengths']} octets, not {AAF_PDU}")
        if stream["flags"] != [0x81]:
            fail(f"{key}: sv and tv {stream['flags']}, not both set")
        if len(stream["destinations"]) != 1:
            fail(f"{key}: sent to {stream['destinations']}, not one address")
        to = int(stream["destinations"][0], 16)
        firsts.append(to - place)
        print(f"ok: {key} {count} sent and arrived, {late} late, to {to:012x}")
    claims = [entry for entry in seen["maap"] if entry["source"] == talker.hex()]
    if dest == "maap":
        if len(set(firsts)) != 1:
            fail(f"the streams' addresses are not one block: {firsts}")
        first = firsts[0]
        if not (POOL_START <= first and first + streams <= POOL_START + POOL_SIZE):
            fail(f"{first:012x} is outside MAAP's pool")
        if block and first < block[0] + block[1] and block[0] < first + streams:
            fail(f"{first:012x} is in the block held, {block[0]:012x} for {block[1]}")
        announced = [entry for entry in claims if entry["type"] == ANNOUNCE]
        if not announced or any(
            (entry["start"], entry["count"]) != (first, streams) for entry in announced
        ):
            fail(f"announced {announced}, not {first:012x} for {streams}")
        print(f"ok: MAAP block {first:012x} for {streams}, announced, clear of any held")
    else:
        address = dest.replace(":", "").lower()
        if any(stream["destinations"] != [address] for stream in seen["streams"].values()):
            fail(f"not every stream went to {dest}")
        if claims:
            fail(f"MAAP sent with a fixed destination: {claims}")
        print(f"ok: every stream to {dest}, no MAAP")


def first_block(talker, count):
    seed = int(talker.replace(":", ""), 16) & 0xFFFFFF
    return POOL_START + seed % (POOL_SIZE - count + 1)


def main():
    if sys.argv[1] == "capture":
        block = None
        if len(sys.argv) > 4:
            block = (int(sys.argv[4], 16), int(sys.argv[5]), float(sys.argv[6]))
        capture(sys.argv[2], sys.argv[3], block)
    elif sys.argv[1] == "first":
        print(f"{first_block(sys.argv[2], int(sys.argv[3])):012x}")
    else:
        block = None
        if len(sys.argv) > 7:
            block = (int(sys.argv[7], 16), int(sys.argv[8]))
        check(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6], block)


if __name__ == "__main__":
    main()
