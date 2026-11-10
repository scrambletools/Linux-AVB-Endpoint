"""An older program's entity, such as an older plaa's, for held.sh:
advertises the default entity's ID (instance 0x70) from the
interface's address every 10 s, answers ENTITY_DISCOVER for it within
300 ms, and logs the ADP the daemon on this address sends.

    python3 -I older-entity.py <interface> <log file>
"""

import random
import socket
import sys
import time

AVTP = 0x22F0
ADP = 0xFA
ADP_MULTICAST = bytes.fromhex("91e0f0010000")
AVAILABLE, DEPARTING, DISCOVER = 0, 1, 2
NAMES = {AVAILABLE: "AVAILABLE", DEPARTING: "DEPARTING", DISCOVER: "DISCOVER"}


def entity_id(mac, instance):
    return int.from_bytes(mac[0:3] + bytes([0xFF, instance]) + mac[3:6], "big")


def adpdu(message_type, entity, available_index):
    valid_time = 31 if message_type == AVAILABLE else 0
    header = bytes([ADP, message_type & 0x0F]) + ((valid_time << 11) | 56).to_bytes(2, "big")
    body = bytearray(64)
    body[0:8] = entity.to_bytes(8, "big")
    body[8:16] = (0x8C1F6436C0200005).to_bytes(8, "big")
    body[32:36] = available_index.to_bytes(4, "big")
    return header + bytes(body)


def main():
    interface, log_path = sys.argv[1], sys.argv[2]
    with open(f"/sys/class/net/{interface}/address") as file:
        mac = bytes.fromhex(file.read().strip().replace(":", ""))
    ours = entity_id(mac, 0x70)
    # Bound to every protocol: Linux hands what another program here sends
    # only to such sockets, and never a socket its own frames.
    every = 0x0003
    sock = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(every))
    sock.bind((interface, every))
    sock.settimeout(0.05)
    log = open(log_path, "w", buffering=1)
    started = time.monotonic()
    available_index = 0
    next_advertise = started
    answer_at = None

    def send(message_type):
        nonlocal available_index
        frame = ADP_MULTICAST + mac + AVTP.to_bytes(2, "big")
        frame += adpdu(message_type, ours, available_index)
        sock.send(frame)
        if message_type == AVAILABLE:
            available_index += 1

    while True:
        now = time.monotonic()
        if now >= next_advertise:
            send(AVAILABLE)
            next_advertise = now + 10
        if answer_at is not None and now >= answer_at:
            send(AVAILABLE)
            log.write(f"{now - started:7.3f} answered the discover\n")
            answer_at = None
        try:
            frame = sock.recv(2048)
        except socket.timeout:
            continue
        payload = frame[14:]
        ethertype = int.from_bytes(frame[12:14], "big")
        if ethertype != AVTP or len(payload) < 12 or payload[0] != ADP or frame[6:12] != mac:
            continue
        message_type = payload[1] & 0x0F
        entity = int.from_bytes(payload[4:12], "big")
        mine = " (the older entity's ID)" if entity == ours else ""
        name = NAMES.get(message_type, str(message_type))
        log.write(f"{time.monotonic() - started:7.3f} daemon sent {name} {entity:016x}{mine}\n")
        if message_type == DISCOVER and entity in (0, ours) and answer_at is None:
            answer_at = time.monotonic() + random.uniform(0.0, 0.3)


main()
