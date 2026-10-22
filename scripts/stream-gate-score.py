#!/usr/bin/env python3
"""Score aligned frame pairs for the streaming quality gate with VMAF.

Scorer: the `vmaf` CLI (libvmaf), model vmaf_v0.6.1, luma, 8 bit. The binary
is pinned by version and SHA-256 in scripts/stream-gate.sh, which passes its
path in STREAM_GATE_VMAF. Called by tools/stream-gate with a manifest:

    {"jobs": [{"id": "clean", "reference": "ref.yuv", "distorted": "dis.yuv",
               "width": 1280, "height": 720}]}

Both files are I420, the same frame count, frame n of one aligned with frame
n of the other. Prints one JSON object on stdout. Without the binary it
prints {"available": false, "reason": ...} and exits 0, so the harness still
reports PSNR, SSIM and every delivery metric. Standard library only.
"""
import json
import os
import platform
import subprocess
import sys
import tempfile

MODEL = "vmaf_v0.6.1"


def unavailable(reason):
    print(json.dumps({"available": False, "reason": reason}))
    sys.exit(0)


def percentile(values, p):
    if not values:
        return None
    s = sorted(values)
    rank = (p / 100.0) * (len(s) - 1)
    lo = int(rank)
    hi = min(lo + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (rank - lo)


def main():
    if len(sys.argv) != 2:
        unavailable("usage: stream-gate-score.py <jobs.json>")
    vmaf = os.environ.get("STREAM_GATE_VMAF", "")
    if not vmaf or not os.path.isfile(vmaf):
        unavailable("STREAM_GATE_VMAF is not a file (run scripts/stream-gate.sh)")
    try:
        out = subprocess.run([vmaf, "--version"], capture_output=True, text=True, check=True)
        # vmaf 3.0.0 prints its version on stderr.
        version = (out.stdout.strip() or out.stderr.strip()).splitlines()[-1].strip()
    except Exception as exc:  # noqa: BLE001 - any failure means "no scorer"
        unavailable("vmaf --version failed: %s" % exc)
    with open(sys.argv[1], "r", encoding="utf-8") as f:
        jobs = json.load(f)["jobs"]
    threads = str(max(1, os.cpu_count() or 1))
    results = []
    for job in jobs:
        out = tempfile.NamedTemporaryFile(suffix=".json", delete=False)
        out.close()
        cmd = [
            vmaf,
            "-r", job["reference"],
            "-d", job["distorted"],
            "-w", str(job["width"]),
            "-h", str(job["height"]),
            "-p", "420",
            "-b", "8",
            "--model", "version=" + MODEL,
            "--threads", threads,
            "--json", "-o", out.name,
            "-q",
        ]
        try:
            subprocess.run(cmd, capture_output=True, text=True, check=True)
            with open(out.name, "r", encoding="utf-8") as f:
                doc = json.load(f)
            frames = [fr["metrics"]["vmaf"] for fr in doc.get("frames", []) if "vmaf" in fr.get("metrics", {})]
            results.append(
                {
                    "id": job["id"],
                    "frames": len(frames),
                    "mean": round(sum(frames) / len(frames), 4) if frames else None,
                    "p5": round(percentile(frames, 5.0), 4) if frames else None,
                    "min": round(min(frames), 4) if frames else None,
                }
            )
        except Exception as exc:  # noqa: BLE001 - report per job, keep going
            results.append({"id": job["id"], "mean": None, "error": str(exc)})
        finally:
            try:
                os.unlink(out.name)
            except OSError:
                pass
    print(
        json.dumps(
            {
                "available": True,
                "scorer": "vmaf %s %s" % (version, MODEL),
                "vmaf_version": version,
                "model": MODEL,
                "python_version": platform.python_version(),
                "results": results,
            }
        )
    )


if __name__ == "__main__":
    main()
