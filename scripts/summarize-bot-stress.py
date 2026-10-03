"""Summarize bot-stress.ps1 logs; Python standard library only.

CPU is in single-core percent. UDP bytes exclude IP/UDP headers and TCP.
Network rates use same-generation deltas between 5 and 25 seconds per bot.
"""
import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def read_lines(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8-sig").splitlines() if line.strip()]


def summarize(case):
    count, scenario = case.name.split("-", 1)
    count = int(count)
    rows = read_lines(case / "bots.jsonl")
    resources = read_lines(case / "resources.jsonl")
    status = read_json(case / "status.json")
    events = Counter(row["event"] for row in rows)
    bots = defaultdict(list)
    for row in rows:
        bots[row["bot"]].append(row)
    samples = [r for r in resources if 10 <= r["elapsed_s"] <= 25]
    wall = sum(r["interval_s"] for r in samples)
    result = {"count": count, "scenario": scenario, "exit_code": status["bot_exit_code"],
              "connected": events["connected"], "finished": events["finished"],
              "errors": [r for r in rows if r["event"] == "error"],
              "tcp_reconnections": events["tcp_reconnected"],
              "voice_retries": sum(r.get("action") == "RetryVoice" for r in rows),
              "lost": sum(r.get("action") == "Lost" for r in rows),
              "recovered": sum(r.get("action") == "Recovered" for r in rows)}
    for process in ("server", "bots"):
        result[process + "_cpu_core_pct"] = round(sum(r[process + "_cpu_core_pct"] * r["interval_s"] for r in samples) / wall, 2) if wall else None
        for memory in ("private", "working_set"):
            result[f"{process}_{memory}_peak_mib"] = round(max((r[f"{process}_{memory}_bytes"] for r in resources if r[f"{process}_{memory}_bytes"] is not None), default=0) / 2**20, 2)
    result["bot_threads_peak"] = max((r["bot_threads"] for r in resources), default=0)
    underruns = overflow = send_errors = 0
    final_healthy = final_audible = 0
    opens = []
    recovery_ms = []
    outage_recovery_ms = []
    bandwidth = Counter()
    for bot_rows in bots.values():
        previous = None
        lost_at = None
        byte_sums = Counter()
        byte_wall = 0.0
        for row in bot_rows:
            if row.get("action") == "Lost":
                lost_at = row["elapsed_ms"]
            if row.get("action") == "Recovered" and lost_at is not None:
                recovery_ms.append(row["elapsed_ms"] - lost_at)
                if scenario == "outage" and lost_at < 17000 <= row["elapsed_ms"]:
                    outage_recovery_ms.append(row["elapsed_ms"] - 17000)
                lost_at = None
            if row["event"] not in ("sample", "finished"):
                continue
            same = previous and previous["generation"] == row["generation"]
            underruns += max(0, row["underruns"] - (previous["underruns"] if same else 0))
            for direction in ("up", "down"):
                if row[direction] is not None:
                    prev = previous[direction] if same else None
                    for key in ("overflow", "send_errors"):
                        delta = max(0, row[direction][key] - (prev[key] if prev else 0))
                        if key == "overflow": overflow += delta
                        else: send_errors += delta
            if same and previous["up"] and previous["down"] and row["up"] and row["down"] and 5000 <= previous["elapsed_ms"] < row["elapsed_ms"] <= 25000:
                byte_wall += (row["elapsed_ms"] - previous["elapsed_ms"]) / 1000
                # Upstream delivered reaches the server-facing socket. Downstream
                # accepted is received from the server, before simulated loss.
                byte_sums["ingress"] += max(0, row["up"]["delivered_bytes"] - previous["up"]["delivered_bytes"])
                byte_sums["egress"] += max(0, row["down"]["accepted_bytes"] - previous["down"]["accepted_bytes"])
            previous = row
        if previous and previous["event"] == "finished":
            final_healthy += bool(previous["udp_ok"])
            final_audible += previous["audible_frames"] > 0
            opens.append(previous["audio_opens"])
        if byte_wall:
            for key, value in byte_sums.items(): bandwidth[key] += value * 8 / byte_wall / 1e6
    result.update(underruns=underruns, proxy_overflow=overflow, proxy_send_errors=send_errors,
                  final_healthy=final_healthy, final_audible=final_audible,
                  audio_opens_max=max(opens, default=0), recovery_ms_max=max(recovery_ms, default=None),
                  outage_recovery_after_end_ms_max=max(outage_recovery_ms, default=None),
                  udp_ingress_mbps=round(bandwidth["ingress"], 3), udp_egress_mbps=round(bandwidth["egress"], 3))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    results = [summarize(p) for p in sorted(args.run.iterdir()) if p.is_dir() and (p / "status.json").exists()]
    (args.run / "summary.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
    print("N scenario serverCPU botsCPU serverMiB botsMiB UDPin/outMbps underruns overflow healthy reconnects retries")
    for r in sorted(results, key=lambda r: (r["count"], r["scenario"])):
        print(r["count"], r["scenario"], r["server_cpu_core_pct"], r["bots_cpu_core_pct"],
              r["server_private_peak_mib"], r["bots_private_peak_mib"],
              f'{r["udp_ingress_mbps"]}/{r["udp_egress_mbps"]}', r["underruns"], r["proxy_overflow"],
              f'{r["final_healthy"]}/{r["count"]}', r["tcp_reconnections"], r["voice_retries"])


if __name__ == "__main__":
    main()
