# SPDX-License-Identifier: GPL-3.0-or-later
"""Loopback-only UDP fault relay for ui_fault_server; no payloads are recorded.

GUI must connect first and exactly one talker second. Only the second endpoint
is exempt; GUI retries using new endpoints remain impaired. Mode file: pass, uplink, downlink, or both.
The backend and front use the same port on distinct loopback addresses, so the
real TLS Welcome remains unmodified. Restart the relay between independent runs.
"""
import argparse
import datetime
import ipaddress
import json
from pathlib import Path
import selectors
import socket
import time

MODES = {"pass", "uplink", "downlink", "both"}
MAX_PEERS = 32
MAX_DATAGRAM = 2048


class Relay:
    def __init__(self, front, backend):
        if not ipaddress.ip_address(front[0]).is_loopback or not ipaddress.ip_address(backend[0]).is_loopback:
            raise ValueError("relay must remain on loopback")
        self.backend = backend
        self.front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.front.bind(front)
        self.front.setblocking(False)
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.front, selectors.EVENT_READ, None)
        self.peers = {}
        self.ordinal = 0
        self.mode = "pass"
        self.counts = dict(up_delivered=0, down_delivered=0, up_dropped=0, down_dropped=0, capacity_dropped=0, errors=0)

    def set_mode(self, mode):
        if mode not in MODES:
            raise ValueError("invalid mode; expected pass/uplink/downlink/both")
        self.mode = mode

    def blocked(self, ordinal, direction):
        return ordinal != 1 and self.mode in (direction, "both")

    def step(self, timeout=0.01):
        for key, _ in self.selector.select(timeout):
            if key.fileobj is self.front:
                try:
                    data, client = self.front.recvfrom(MAX_DATAGRAM + 1)
                except BlockingIOError:
                    continue
                if len(data) > MAX_DATAGRAM or not ipaddress.ip_address(client[0]).is_loopback:
                    self.counts["errors"] += 1
                    continue
                peer = self.peers.get(client)
                if peer is None:
                    if len(self.peers) >= MAX_PEERS:
                        self.counts["capacity_dropped"] += 1
                        continue
                    upstream = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                    upstream.bind(("127.0.0.1", 0))
                    upstream.connect(self.backend)
                    upstream.setblocking(False)
                    peer = dict(socket=upstream, client=client, ordinal=self.ordinal, seen=time.monotonic())
                    self.ordinal += 1
                    self.peers[client] = peer
                    self.selector.register(upstream, selectors.EVENT_READ, peer)
                peer["seen"] = time.monotonic()
                if self.blocked(peer["ordinal"], "uplink"):
                    self.counts["up_dropped"] += 1
                else:
                    try:
                        peer["socket"].send(data)
                        self.counts["up_delivered"] += 1
                    except OSError:
                        self.counts["errors"] += 1
            else:
                peer = key.data
                try:
                    data = peer["socket"].recv(MAX_DATAGRAM + 1)
                except BlockingIOError:
                    continue
                except OSError:
                    self.counts["errors"] += 1
                    continue
                peer["seen"] = time.monotonic()
                if len(data) > MAX_DATAGRAM:
                    self.counts["errors"] += 1
                    continue
                if self.blocked(peer["ordinal"], "downlink"):
                    self.counts["down_dropped"] += 1
                else:
                    try:
                        self.front.sendto(data, peer["client"])
                        self.counts["down_delivered"] += 1
                    except OSError:
                        self.counts["errors"] += 1
        now = time.monotonic()
        for client, peer in list(self.peers.items()):
            if now - peer["seen"] > 60:
                self.selector.unregister(peer["socket"])
                peer["socket"].close()
                del self.peers[client]

    def close(self):
        for peer in self.peers.values():
            peer["socket"].close()
        self.front.close()
        self.selector.close()


def record(relay, event):
    print(json.dumps(dict(event=event, utc=datetime.datetime.now(datetime.timezone.utc).isoformat(), mode=relay.mode,
                         peers=len(relay.peers), **relay.counts)), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=20898)
    parser.add_argument("--mode-file", type=Path, required=True)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("port must be 1..65535")
    mode = args.mode_file.read_text(encoding="utf-8-sig").strip()
    if mode not in MODES:
        parser.error("mode file must initially contain pass/uplink/downlink/both")
    relay = Relay(("127.0.0.1", args.port), ("127.0.0.2", args.port))
    relay.set_mode(mode)
    record(relay, "started")
    next_mode = next_record = time.monotonic()
    mode_error = False
    try:
        while True:
            relay.step()
            now = time.monotonic()
            if now >= next_mode:
                next_mode = now + 0.1
                try:
                    mode = args.mode_file.read_text(encoding="utf-8-sig").strip()
                    if mode not in MODES:
                        raise ValueError("invalid mode")
                    if mode != relay.mode:
                        relay.set_mode(mode)
                        record(relay, "mode_changed")
                    mode_error = False
                except (OSError, ValueError):
                    if not mode_error:
                        record(relay, "invalid_or_unreadable_mode_retained_previous")
                    mode_error = True
            if now >= next_record:
                next_record = now + 5
                record(relay, "counters")
    except KeyboardInterrupt:
        record(relay, "stopped")
    finally:
        relay.close()


if __name__ == "__main__":
    main()
