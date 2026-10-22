#!/usr/bin/env python3
"""Make benchmarks/baselines/stream/gates.json from the baselines and repeat runs.

    python tools/stream-gate/make_gates.py benchmarks/baselines/stream/gates.json \\
        cpu=baseline-2.1-cpu.json,spread-a/results.json,run-b/results.json \\
        hw=baseline-2.1-hw.json,...

The first file of a lane is its baseline; the others are results.json files of
runs on the same commit and machine. A `--repeat N` result gives its N values
from "spread"; a plain result gives its one value per metric. For each gated
metric the bound is the baseline value minus (floor) or plus (ceiling) the
largest of a fixed margin, a share of the value and 1.1 times the spread of all
runs. A metric that is missing in any run (no frame presented) gets no gate:
it would fail at random. The file keeps the values it used in "_spread".
Standard library only.
"""
import json
import math
import sys

# metric, direction (+1: higher is better -> floor; -1: lower is better ->
# ceiling), fixed margin, share of the baseline value
RULES = [
    ("vmaf_mean", +1, 2.0, 0.0),
    ("fps_delivered", +1, 2.0, 0.05),
    ("freeze_ms", -1, 250.0, 0.10),
    ("ttff_ms", -1, 250.0, 0.25),
    ("g2g_p95_ms", -1, 30.0, 0.15),
    ("cap_overshoot_pct", -1, 5.0, 0.0),
]


def metric(doc, profile, name):
    return doc.get("profiles", {}).get(profile, {}).get("metrics", {}).get(name)


def runs_of(doc, profile, name):
    """Values of one metric in a result file: the --repeat list, or the one value."""
    spread = doc.get("spread", {})
    if spread:
        rep = spread.get(profile)
        if rep is None:
            return None
        vals = rep.get(name)
        # A --repeat result lists a metric only when some run has it.
        return vals if vals is not None else [None]
    if profile not in doc.get("profiles", {}):
        return None
    return [metric(doc, profile, name)]


def main():
    out_path = sys.argv[1]
    with open(out_path, encoding="utf-8") as f:
        gates = json.load(f)
    gates["lanes"] = {}
    gates["freeze_ms_max_rise"] = {}
    gates["vmaf_max_drop_profile"] = {}
    used = {}
    for arg in sys.argv[2:]:
        lane, files = arg.split("=", 1)
        paths = files.split(",")
        docs = []
        for p in paths:
            with open(p, encoding="utf-8") as f:
                docs.append(json.load(f))
        base, others = docs[0], docs[1:]
        per = {}
        for profile in base["profiles"]:
            gl = []
            for name, direction, fixed, share in RULES:
                b = metric(base, profile, name)
                if b is None:
                    continue
                values = [b]
                complete = True
                for d in others:
                    r = runs_of(d, profile, name)
                    if r is None:
                        continue
                    if any(v is None for v in r):
                        complete = False
                        break
                    values += r
                if not complete:
                    continue
                span = max(values) - min(values)
                # 10 % over the spread: a few runs do not show the full range.
                margin = max(fixed, share * abs(b), 1.1 * span)
                note = "baseline %.2f %s %.2f (spread %.2f over %d runs; margin = max(fixed %.0f, %.0f %% of baseline, 1.1 x spread))" % (
                    b, "minus" if direction > 0 else "plus", margin, span, len(values), fixed, share * 100)
                g = {"metric": name, "enforced": True, "note": note}
                if direction > 0:
                    if b - margin <= 0:
                        continue
                    g["min"] = math.floor((b - margin) * 100) / 100
                else:
                    g["max"] = math.ceil((b + margin) * 100) / 100
                gl.append(g)
                used.setdefault(lane, {}).setdefault(profile, {})[name] = values
                if name == "vmaf_mean" and 1.1 * span > gates.get("vmaf_max_drop", 1.0):
                    gates["vmaf_max_drop_profile"].setdefault(lane, {})[profile] = math.ceil(1.1 * span * 100) / 100
                if name == "freeze_ms":
                    gates["freeze_ms_max_rise"].setdefault(lane, {})[profile] = math.ceil(max(1.1 * span, fixed))
            per[profile] = gl
        gates["lanes"][lane] = per
    gates["_spread"] = used
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(gates, f, indent=2)
        f.write("\n")


if __name__ == "__main__":
    main()
