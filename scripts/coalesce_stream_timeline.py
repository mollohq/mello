#!/usr/bin/env python3
"""
Coalesce stream probe + SFU logs into one wall-clock timeline.

This script is meant for the SFU investigation loop:
1) read local host/viewer probe logs (from stream-host and sfu-stream-viewer-probe),
2) fetch SFU logs from a GCP VM via gcloud compute ssh (or read from a saved file),
3) keep only lines for one session_id,
4) sort everything by wall_ms,
5) write one JSONL timeline for fast correlation.

Example:
  python scripts/coalesce_stream_timeline.py \
    --session stream_abcd_1234 \
    --host-log C:\\temp\\host.log \
    --viewer-log C:\\temp\\viewer.log \
    --vm mello-sfu-eu \
    --zone europe-west3-c \
    --project m3llo-490321 \
    --sfu-since 20m \
    --output C:\\temp\\timeline.jsonl
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import re
import shlex
import subprocess
import sys
from typing import Any, Iterable, Optional


PROBE_MARKERS = (
    "host_probe_start",
    "host_probe_event",
    "host_probe_tick",
    "viewer_probe_start",
    "viewer_probe_event",
    "viewer_probe_tick",
)

TS_PREFIX_RE = re.compile(r"^\[([0-9T:\-\.]+Z)")
WALL_MS_RE = re.compile(r"\bwall_ms=(\d+)\b")
MONO_MS_RE = re.compile(r"\bmono_ms=(\d+)\b")
SESSION_RE = re.compile(r"\bsession=([^\s]+)")
EVENT_RE = re.compile(r"\bevent=([^\s]+)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Merge host/viewer/SFU logs into one wall_ms timeline."
    )
    parser.add_argument(
        "--session",
        required=True,
        help="Session ID to correlate (e.g. stream_ab052de9_1776535990797)",
    )
    parser.add_argument("--host-log", help="Path to stream-host log file")
    parser.add_argument("--viewer-log", help="Path to sfu-stream-viewer-probe log file")

    parser.add_argument(
        "--sfu-log-file",
        help="Path to pre-fetched SFU log file (skips gcloud fetch)",
    )
    parser.add_argument(
        "--vm",
        default="mello-sfu-eu",
        help="GCP VM name for SFU logs (default: mello-sfu-eu)",
    )
    parser.add_argument(
        "--zone",
        default="europe-west3-c",
        help="GCP VM zone (default: europe-west3-c)",
    )
    parser.add_argument(
        "--project",
        default="m3llo-490321",
        help="GCP project ID (default: m3llo-490321)",
    )
    parser.add_argument(
        "--sfu-container",
        default="mello-sfu",
        help="Docker container name on VM (default: mello-sfu)",
    )
    parser.add_argument(
        "--sfu-since",
        default="30m",
        help='docker logs --since value (default: "30m")',
    )
    parser.add_argument(
        "--sfu-tail-lines",
        type=int,
        default=20000,
        help="Tail this many SFU log lines after since filter (default: 20000)",
    )
    parser.add_argument(
        "--no-fetch-sfu",
        action="store_true",
        help="Do not fetch SFU logs from VM (requires --sfu-log-file to include SFU)",
    )
    parser.add_argument(
        "--save-sfu-raw",
        help="Optional path to save fetched raw SFU logs",
    )
    parser.add_argument(
        "--output",
        help="Output timeline JSONL path (default: ./timeline-<session>.jsonl)",
    )
    return parser.parse_args()


def read_lines(path: pathlib.Path) -> list[str]:
    try:
        return path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as exc:
        raise RuntimeError(f"failed to read {path}: {exc}") from exc


def parse_iso_millis(ts: str) -> Optional[int]:
    try:
        parsed = dt.datetime.fromisoformat(ts.replace("Z", "+00:00"))
        return int(parsed.timestamp() * 1000)
    except ValueError:
        return None


def extract_first_int(regex: re.Pattern[str], text: str) -> Optional[int]:
    match = regex.search(text)
    if not match:
        return None
    try:
        return int(match.group(1))
    except ValueError:
        return None


def extract_first(regex: re.Pattern[str], text: str) -> Optional[str]:
    match = regex.search(text)
    return match.group(1) if match else None


def parse_probe_lines(
    lines: Iterable[str], source: str, target_session: str
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for line in lines:
        marker = next((m for m in PROBE_MARKERS if m in line), None)
        if not marker:
            continue

        session = extract_first(SESSION_RE, line)
        if session != target_session:
            continue

        wall_ms = extract_first_int(WALL_MS_RE, line)
        mono_ms = extract_first_int(MONO_MS_RE, line)
        event = extract_first(EVENT_RE, line)

        if wall_ms is None:
            ts_match = TS_PREFIX_RE.match(line)
            if ts_match:
                wall_ms = parse_iso_millis(ts_match.group(1))

        if wall_ms is None:
            continue

        out.append(
            {
                "wall_ms": wall_ms,
                "source": source,
                "kind": marker,
                "event": event,
                "session": session,
                "mono_ms": mono_ms,
                "raw": line,
            }
        )
    return out


def parse_sfu_json_line(line: str) -> Optional[dict[str, Any]]:
    idx = line.find("{")
    if idx < 0:
        return None
    payload = line[idx:]
    try:
        obj = json.loads(payload)
    except json.JSONDecodeError:
        return None
    return obj if isinstance(obj, dict) else None


def parse_sfu_lines(lines: Iterable[str], target_session: str) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for line in lines:
        obj = parse_sfu_json_line(line)
        if not obj:
            continue

        session = obj.get("session")
        if session != target_session:
            continue

        data = obj.get("data")
        if not isinstance(data, dict):
            data = {}

        wall_ms = data.get("wall_ms")
        if isinstance(wall_ms, str) and wall_ms.isdigit():
            wall_ms = int(wall_ms)
        elif not isinstance(wall_ms, int):
            wall_ms = None

        if wall_ms is None:
            ts = obj.get("ts")
            if isinstance(ts, str):
                wall_ms = parse_iso_millis(ts)

        if wall_ms is None:
            continue

        out.append(
            {
                "wall_ms": wall_ms,
                "source": "sfu",
                "kind": obj.get("event"),
                "event": obj.get("event"),
                "session": session,
                "mono_ms": data.get("session_age_ms"),
                "raw": line,
                "sfu_level": obj.get("level"),
            }
        )
    return out


def fetch_sfu_logs(args: argparse.Namespace) -> str:
    remote_cmd = (
        f"sudo docker logs --since {shlex.quote(args.sfu_since)} "
        f"{shlex.quote(args.sfu_container)} 2>&1"
    )
    if args.sfu_tail_lines > 0:
        remote_cmd += f" | tail -n {args.sfu_tail_lines}"

    cmd = [
        "gcloud",
        "compute",
        "ssh",
        args.vm,
        "--zone",
        args.zone,
        "--project",
        args.project,
        "--command",
        remote_cmd,
    ]

    proc = subprocess.run(
        cmd,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if proc.returncode != 0:
        stderr = proc.stderr.strip()
        raise RuntimeError(
            "failed to fetch SFU logs via gcloud compute ssh.\n"
            f"command: {' '.join(cmd)}\n"
            f"stderr: {stderr}"
        )
    return proc.stdout


def write_jsonl(path: pathlib.Path, entries: list[dict[str, Any]]) -> None:
    with path.open("w", encoding="utf-8", newline="\n") as fh:
        for entry in entries:
            fh.write(json.dumps(entry, ensure_ascii=False))
            fh.write("\n")


def main() -> int:
    args = parse_args()
    target_session = args.session
    merged: list[dict[str, Any]] = []

    if args.host_log:
        host_path = pathlib.Path(args.host_log)
        merged.extend(parse_probe_lines(read_lines(host_path), "host", target_session))

    if args.viewer_log:
        viewer_path = pathlib.Path(args.viewer_log)
        merged.extend(parse_probe_lines(read_lines(viewer_path), "viewer", target_session))

    sfu_raw: Optional[str] = None
    if args.sfu_log_file:
        sfu_raw = pathlib.Path(args.sfu_log_file).read_text(
            encoding="utf-8", errors="replace"
        )
    elif not args.no_fetch_sfu:
        sfu_raw = fetch_sfu_logs(args)

    if sfu_raw is not None:
        if args.save_sfu_raw:
            pathlib.Path(args.save_sfu_raw).write_text(
                sfu_raw, encoding="utf-8", newline="\n"
            )
        merged.extend(parse_sfu_lines(sfu_raw.splitlines(), target_session))

    if not merged:
        print(
            "No correlated entries found. Check session ID, log file paths, "
            "or widen --sfu-since.",
            file=sys.stderr,
        )
        return 2

    merged.sort(
        key=lambda e: (
            int(e.get("wall_ms", 0)),
            str(e.get("source", "")),
            str(e.get("kind", "")),
        )
    )

    output_path = pathlib.Path(
        args.output or f"timeline-{target_session.replace('/', '_')}.jsonl"
    )
    write_jsonl(output_path, merged)

    host_count = sum(1 for e in merged if e.get("source") == "host")
    viewer_count = sum(1 for e in merged if e.get("source") == "viewer")
    sfu_count = sum(1 for e in merged if e.get("source") == "sfu")
    min_wall = min(int(e["wall_ms"]) for e in merged)
    max_wall = max(int(e["wall_ms"]) for e in merged)
    span_ms = max_wall - min_wall

    print(f"Wrote {len(merged)} entries to {output_path}")
    print(
        f"Counts: host={host_count} viewer={viewer_count} sfu={sfu_count} "
        f"span_ms={span_ms}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
