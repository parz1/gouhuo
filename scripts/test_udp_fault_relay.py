# SPDX-License-Identifier: GPL-3.0-or-later
import socket
import time
import unittest
from udp_fault_relay import Relay


class RelayTests(unittest.TestCase):
    def setUp(self):
        self.backend = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.backend.bind(("127.0.0.2", 0))
        self.backend.setblocking(False)
        self.relay = Relay(("127.0.0.1", 0), self.backend.getsockname())
        self.clients = [socket.socket(socket.AF_INET, socket.SOCK_DGRAM) for _ in range(3)]
        for client in self.clients:
            client.bind(("127.0.0.1", 0))
            client.setblocking(False)
        self.front = self.relay.front.getsockname()

    def tearDown(self):
        self.relay.close()
        self.backend.close()
        for client in self.clients:
            client.close()

    def roundtrip(self, index, data=b"encrypted-packet-placeholder"):
        """Send one packet and collect what each side receives.

        Finishes when the relay has accounted for every packet in both
        directions (each one is counted exactly once as delivered or
        dropped) and everything delivered has been read, instead of after a
        fixed wall-clock window that a slow runner can overrun.
        """
        start = dict(self.relay.counts)
        delta = lambda key: self.relay.counts[key] - start[key]
        self.clients[index].sendto(data, self.front)
        upstream = []
        downstream = []
        deadline = time.monotonic() + 5.0
        while True:
            up_resolved = delta("up_delivered") + delta("up_dropped") + delta("capacity_dropped") + delta("errors")
            down_resolved = delta("down_delivered") + delta("down_dropped")
            if (
                up_resolved >= 1
                and len(upstream) == delta("up_delivered")
                and down_resolved == len(upstream)
                and len(downstream) == delta("down_delivered")
            ):
                break
            self.assertLess(time.monotonic(), deadline, "relay did not account for the packet")
            self.relay.step(0.001)
            try:
                packet, source = self.backend.recvfrom(4096)
                upstream.append(packet)
                self.backend.sendto(packet, source)
            except BlockingIOError:
                pass
            try:
                packet, source = self.clients[index].recvfrom(4096)
                self.assertEqual(source, self.front, "client must receive from its connected server address")
                downstream.append(packet)
            except BlockingIOError:
                pass
        return upstream, downstream

    def test_bidirectional_exact_bytes_and_no_endpoint_cross_talk(self):
        for index in range(2):
            packet = bytes(range(256)) + bytes([index])
            self.assertEqual(self.roundtrip(index, packet), ([packet], [packet]))
        for client in self.clients:
            with self.assertRaises(BlockingIOError):
                client.recvfrom(4096)

    def test_each_fault_keeps_talker_live_and_blocks_retry_endpoints_and_restores_without_rebinding(self):
        packet = b"opaque"
        self.assertEqual(self.roundtrip(0, packet), ([packet], [packet]))
        self.assertEqual(self.roundtrip(1, packet), ([packet], [packet]))
        for mode, expected in [("uplink", ([], [])), ("downlink", ([packet], [])), ("both", ([], []))]:
            self.relay.set_mode(mode)
            self.assertEqual(self.roundtrip(0, packet), expected)
            self.assertEqual(self.roundtrip(2, packet), expected, "new GUI endpoint must not bypass an outage")
            self.assertEqual(self.roundtrip(1, packet), ([packet], [packet]))
            self.relay.set_mode("pass")
            self.assertEqual(self.roundtrip(0, packet), ([packet], [packet]))
        self.assertGreater(self.relay.counts["up_dropped"], 0)
        self.assertGreater(self.relay.counts["down_dropped"], 0)
        self.assertEqual(self.relay.counts["errors"], 0)

    def test_invalid_mode_does_not_silently_change_direction(self):
        self.relay.set_mode("downlink")
        with self.assertRaises(ValueError):
            self.relay.set_mode("garbage")
        self.assertEqual(self.relay.mode, "downlink")

    def test_non_loopback_is_rejected_before_binding(self):
        with self.assertRaises(ValueError):
            Relay(("0.0.0.0", 0), self.backend.getsockname())


if __name__ == "__main__":
    unittest.main()
