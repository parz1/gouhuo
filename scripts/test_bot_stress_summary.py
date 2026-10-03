"""Guard against reconnect resets and exit races producing misleading totals."""
import json
import tempfile
import unittest
from pathlib import Path

from importlib.util import module_from_spec, spec_from_file_location

spec = spec_from_file_location("summary", Path(__file__).with_name("summarize-bot-stress.py"))
summary = module_from_spec(spec)
spec.loader.exec_module(summary)


class SummaryTests(unittest.TestCase):
    def test_resets_are_accumulated_and_exit_race_memory_is_ignored(self):
        with tempfile.TemporaryDirectory() as tmp:
            case = Path(tmp) / "1-clean"
            case.mkdir()
            rows = [{"bot": 1, "event": "connected"}]
            for generation, elapsed, underruns, amount, event in [
                (1, 0, 0, 0, "sample"), (1, 5000, 2, 100, "sample"),
                (1, 6000, 5, 300, "sample"), (2, 7000, 1, 10, "sample"),
                (2, 8000, 3, 110, "finished")
            ]:
                net = dict(overflow=0, send_errors=0, accepted_bytes=amount, delivered_bytes=amount)
                rows.append(dict(bot=1, event=event, generation=generation, elapsed_ms=elapsed,
                                 underruns=underruns, up=net, down=net, udp_ok=True,
                                 audible_frames=20, audio_opens=generation))
            rows.extend([dict(bot=1, event="recovery", action="Lost", elapsed_ms=6200),
                         dict(bot=1, event="recovery", action="Recovered", elapsed_ms=7200)])
            resource = dict(elapsed_s=10, interval_s=1, server_cpu_core_pct=25, bots_cpu_core_pct=50,
                            server_private_bytes=1048576, server_working_set_bytes=1048576,
                            bots_private_bytes=2097152, bots_working_set_bytes=2097152, bot_threads=10)
            raced = dict(resource, elapsed_s=31, bots_private_bytes=None, bots_working_set_bytes=None)
            for name, values in [("bots.jsonl", rows), ("resources.jsonl", [resource, raced])]:
                (case / name).write_text("\n".join(json.dumps(r) for r in values), encoding="utf-8")
            (case / "status.json").write_text('{"bot_exit_code":0}', encoding="utf-8")
            result = summary.summarize(case)
            self.assertEqual(result["underruns"], 8)
            self.assertEqual(result["udp_ingress_mbps"], 0.001)
            self.assertEqual(result["audio_opens_max"], 2)
            self.assertEqual(result["server_cpu_core_pct"], 25)
            self.assertEqual(result["bots_private_peak_mib"], 2)
            self.assertEqual(result["recovery_ms_max"], 1000)
            self.assertEqual(result["final_healthy"], 1)


if __name__ == "__main__":
    unittest.main()
