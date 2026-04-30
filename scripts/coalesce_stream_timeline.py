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
import os
import pathlib
import re
import shlex
import shutil
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
CORR_START_RE = re.compile(r"corr_start_unix_ms:\s*(\d+)")
HOST_SESSION_START_RE = re.compile(r"Nakama start_stream:\s+mode=sfu\s+session=([^\s]+)")
HOST_SESSION_LINE_RE = re.compile(r"Session:\s*([^\s]+)\s+\(role=")
VIEWER_SESSION_LINE_RE = re.compile(r"session:\s*([^\s]+)")
HOST_TICK_RE = re.compile(
    r"^\[\s*(\d+)s\]\s+viewers=(\d+)\s+media_open=(\w+)\s+control_open=(\w+)\s+rtt_ms=([0-9.]+)\s+disconnect=([^\s]+)"
)
KV_RE = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)=([^\s]+)")
HOST_DIAG_MARKERS = (
    "Stream manager diag:",
    "Stream manager video coalesce:",
    "Stream manager severe coalesce under pressure:",
    "Stream manager keyframe requested:",
    "Stream manager failed to send",
    "Stream pacing:",
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Merge host/viewer/SFU logs into one wall_ms timeline."
    )
    parser.add_argument(
        "--session",
        required=True,
        help=(
            "Session ID to correlate (e.g. stream_ab052de9_1776535990797), "
            'or "auto" to detect from host/viewer logs.'
        ),
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
    parser.add_argument(
        "--gcloud-bin",
        help=(
            "Optional explicit path to gcloud executable/cmd/ps1. "
            "Useful on Windows when gcloud is not on PATH."
        ),
    )
    return parser.parse_args()


def read_lines(path: pathlib.Path) -> list[str]:
    try:
        raw = path.read_bytes()
        if raw.startswith(b"\xff\xfe") or raw.startswith(b"\xfe\xff"):
            return raw.decode("utf-16", errors="replace").splitlines()
        if raw.startswith(b"\xef\xbb\xbf"):
            return raw.decode("utf-8-sig", errors="replace").splitlines()
        return raw.decode("utf-8", errors="replace").splitlines()
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


def parse_scalar(value: str) -> Any:
    if value.lower() == "true":
        return True
    if value.lower() == "false":
        return False
    if re.fullmatch(r"[-+]?\d+", value):
        try:
            return int(value)
        except ValueError:
            return value
    if re.fullmatch(r"[-+]?\d+\.\d+", value):
        try:
            return float(value)
        except ValueError:
            return value
    return value


def parse_kv_fields(text: str) -> dict[str, Any]:
    fields: dict[str, Any] = {}
    for key, raw_value in KV_RE.findall(text):
        fields[key] = parse_scalar(raw_value.rstrip(","))
    return fields


def parse_ts_prefix_wall_ms(line: str) -> Optional[int]:
    ts_match = TS_PREFIX_RE.match(line)
    if not ts_match:
        return None
    return parse_iso_millis(ts_match.group(1))


def maybe_with_mono_ms(
    entry: dict[str, Any], corr_start_ms: Optional[int], wall_ms: Optional[int]
) -> None:
    if corr_start_ms is None or wall_ms is None:
        return
    if wall_ms >= corr_start_ms:
        entry["mono_ms"] = wall_ms - corr_start_ms


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
        fields = parse_kv_fields(line)

        if wall_ms is None:
            wall_ms = parse_ts_prefix_wall_ms(line)

        if wall_ms is None:
            continue

        fields.pop("session", None)
        fields.pop("wall_ms", None)
        fields.pop("mono_ms", None)
        fields.pop("event", None)
        out.append({
            "wall_ms": wall_ms,
            "source": source,
            "kind": marker,
            "event": event,
            "session": session,
            "mono_ms": mono_ms,
            "raw": line,
            "fields": fields,
        })
    return out


def find_corr_start_ms(lines: Iterable[str]) -> Optional[int]:
    for line in lines:
        val = extract_first_int(CORR_START_RE, line)
        if val is not None:
            return val
    return None


def find_last_corr_start_ms(lines: Iterable[str]) -> Optional[int]:
    last: Optional[int] = None
    for line in lines:
        val = extract_first_int(CORR_START_RE, line)
        if val is not None:
            last = val
    return last


def find_session_in_probe_lines(lines: Iterable[str], source: str) -> Optional[str]:
    last_session: Optional[str] = None
    for line in lines:
        if source == "host":
            session = extract_first(HOST_SESSION_START_RE, line) or extract_first(
                HOST_SESSION_LINE_RE, line
            )
        else:
            session = extract_first(VIEWER_SESSION_LINE_RE, line) or extract_first(
                HOST_SESSION_START_RE, line
            )
        if session:
            last_session = session
    return last_session


def select_auto_session(
    host_lines: Optional[list[str]], viewer_lines: Optional[list[str]]
) -> tuple[Optional[str], str]:
    host_session = (
        find_session_in_probe_lines(host_lines, "host") if host_lines is not None else None
    )
    viewer_session = (
        find_session_in_probe_lines(viewer_lines, "viewer")
        if viewer_lines is not None
        else None
    )

    if host_session and viewer_session:
        if host_session == viewer_session:
            return host_session, "host+viewer"

        host_start = find_last_corr_start_ms(host_lines or [])
        viewer_start = find_last_corr_start_ms(viewer_lines or [])
        if host_start is not None and viewer_start is not None:
            if host_start >= viewer_start:
                return host_session, "host(latest)"
            return viewer_session, "viewer(latest)"
        return host_session, "host(preferred)"

    if host_session:
        return host_session, "host"
    if viewer_session:
        return viewer_session, "viewer"
    return None, "none"


def parse_legacy_probe_lines(
    lines: list[str], source: str, target_session: str
) -> list[dict[str, Any]]:
    session = find_session_in_probe_lines(lines, source)
    if session and session != target_session:
        return []

    corr_start_ms = find_corr_start_ms(lines)
    out: list[dict[str, Any]] = []

    if corr_start_ms is not None:
        out.append(
            {
                "wall_ms": corr_start_ms,
                "source": source,
                "kind": f"{source}_probe_start_legacy",
                "event": "start",
                "session": target_session,
                "mono_ms": 0,
                "raw": f"corr_start_unix_ms: {corr_start_ms}",
            }
        )

    # Legacy host progress line:
    # [ 12s] viewers=1 media_open=true control_open=true rtt_ms=0.0 disconnect=-
    if source == "host" and corr_start_ms is not None:
        for line in lines:
            match = HOST_TICK_RE.match(line.strip())
            if not match:
                continue
            sec = int(match.group(1))
            viewers = int(match.group(2))
            media_open = match.group(3)
            control_open = match.group(4)
            rtt_ms = match.group(5)
            disconnect = match.group(6)
            out.append(
                {
                    "wall_ms": corr_start_ms + sec * 1000,
                    "source": source,
                    "kind": "host_probe_tick_legacy",
                    "event": "tick",
                    "session": target_session,
                    "mono_ms": sec * 1000,
                    "raw": line,
                    "viewers": viewers,
                    "media_open": media_open,
                    "control_open": control_open,
                    "rtt_ms": rtt_ms,
                    "disconnect": disconnect,
                }
            )

    # If stderr wasn't captured, viewer logs may only include startup metadata.
    # Keep startup as a timeline anchor in that case.
    return out


def parse_host_diag_lines(lines: list[str], target_session: str) -> list[dict[str, Any]]:
    host_session = find_session_in_probe_lines(lines, "host")
    if host_session and host_session != target_session:
        return []

    corr_start_ms = find_corr_start_ms(lines)
    out: list[dict[str, Any]] = []
    for line in lines:
        marker = next((m for m in HOST_DIAG_MARKERS if m in line), None)
        if not marker:
            continue

        wall_ms = parse_ts_prefix_wall_ms(line)
        if wall_ms is None:
            continue

        fields = parse_kv_fields(line)
        entry = {
            "wall_ms": wall_ms,
            "source": "host",
            "kind": "host_stream_diag",
            "event": marker.rstrip(":"),
            "session": target_session,
            "raw": line,
            "fields": fields,
        }
        maybe_with_mono_ms(entry, corr_start_ms, wall_ms)
        out.append(entry)
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
        if session is None:
            data_for_session = obj.get("data")
            if isinstance(data_for_session, dict):
                session = data_for_session.get("session")
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

        scalar_data = {k: v for k, v in data.items() if isinstance(v, (str, int, float, bool))}
        out.append({
            "wall_ms": wall_ms,
            "source": "sfu",
            "kind": obj.get("event"),
            "event": obj.get("event"),
            "session": session,
            "mono_ms": data.get("session_age_ms"),
            "raw": line,
            "sfu_level": obj.get("level"),
            "fields": scalar_data,
            "data": data,
        })
    return out


def print_highlights(entries: list[dict[str, Any]]) -> None:
    host_diag = [
        e
        for e in entries
        if e.get("source") == "host" and e.get("kind") == "host_stream_diag"
    ]
    sfu_anomaly = [e for e in entries if e.get("kind") == "stream_relay_anomaly"]
    viewer_ticks = [e for e in entries if e.get("kind") == "viewer_probe_tick"]

    coalesce_events = sum(
        1
        for e in host_diag
        if str(e.get("event", "")).startswith("Stream manager video coalesce")
    )
    severe_coalesce = sum(
        1
        for e in host_diag
        if str(e.get("event", "")).startswith(
            "Stream manager severe coalesce under pressure"
        )
    )
    manager_diag = sum(
        1
        for e in host_diag
        if str(e.get("event", "")).startswith("Stream manager diag")
    )

    max_decode_stall_ms = 0
    max_decode_backlog = 0
    low_decode_samples = 0
    for tick in viewer_ticks:
        fields = tick.get("fields")
        if not isinstance(fields, dict):
            continue
        decode_stall = fields.get("decode_stall_ms")
        if isinstance(decode_stall, int):
            max_decode_stall_ms = max(max_decode_stall_ms, decode_stall)
        decode_backlog = fields.get("decode_backlog_est")
        if isinstance(decode_backlog, int):
            max_decode_backlog = max(max_decode_backlog, decode_backlog)
        dec_fps = fields.get("dec_fps")
        if isinstance(dec_fps, (int, float)) and dec_fps < 40:
            low_decode_samples += 1

    print(
        "Highlights: "
        f"host_diag={len(host_diag)} manager_diag={manager_diag} "
        f"coalesce={coalesce_events} severe_coalesce={severe_coalesce} "
        f"sfu_anomaly={len(sfu_anomaly)} "
        f"viewer_low_dec_samples(<40fps)={low_decode_samples} "
        f"max_decode_stall_ms={max_decode_stall_ms} "
        f"max_decode_backlog_est={max_decode_backlog}"
    )


def resolve_gcloud_command(args: argparse.Namespace) -> list[str]:
    # If user provided explicit path, trust it.
    if args.gcloud_bin:
        gcloud_path = pathlib.Path(args.gcloud_bin)
        if not gcloud_path.exists():
            raise RuntimeError(f"--gcloud-bin does not exist: {gcloud_path}")
        if gcloud_path.suffix.lower() == ".ps1":
            return [
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(gcloud_path),
            ]
        return [str(gcloud_path)]

    # First, normal PATH lookup.
    for name in ("gcloud", "gcloud.cmd", "gcloud.exe"):
        found = shutil.which(name)
        if found:
            return [found]

    # Windows fallback: common Cloud SDK install paths.
    if os.name == "nt":
        local_app_data = os.environ.get("LOCALAPPDATA")
        program_files_x86 = os.environ.get("ProgramFiles(x86)")
        program_files = os.environ.get("ProgramFiles")
        candidates = []
        if local_app_data:
            candidates.append(
                pathlib.Path(local_app_data)
                / "Google"
                / "Cloud SDK"
                / "google-cloud-sdk"
                / "bin"
                / "gcloud.cmd"
            )
            candidates.append(
                pathlib.Path(local_app_data)
                / "Google"
                / "Cloud SDK"
                / "google-cloud-sdk"
                / "bin"
                / "gcloud.ps1"
            )
        if program_files_x86:
            candidates.append(
                pathlib.Path(program_files_x86)
                / "Google"
                / "Cloud SDK"
                / "google-cloud-sdk"
                / "bin"
                / "gcloud.cmd"
            )
        if program_files:
            candidates.append(
                pathlib.Path(program_files)
                / "Google"
                / "Cloud SDK"
                / "google-cloud-sdk"
                / "bin"
                / "gcloud.cmd"
            )

        for candidate in candidates:
            if candidate.exists():
                if candidate.suffix.lower() == ".ps1":
                    return [
                        "powershell",
                        "-NoProfile",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-File",
                        str(candidate),
                    ]
                return [str(candidate)]

    raise RuntimeError(
        "Could not find gcloud CLI. Install Google Cloud SDK or provide --gcloud-bin.\n"
        "Windows example:\n"
        "  --gcloud-bin \"C:\\Users\\<you>\\AppData\\Local\\Google\\Cloud SDK\\google-cloud-sdk\\bin\\gcloud.cmd\""
    )


def fetch_sfu_logs(args: argparse.Namespace) -> str:
    gcloud_cmd = resolve_gcloud_command(args)
    remote_cmd = (
        f"sudo docker logs --since {shlex.quote(args.sfu_since)} "
        f"{shlex.quote(args.sfu_container)} 2>&1"
    )
    if args.sfu_tail_lines > 0:
        remote_cmd += f" | tail -n {args.sfu_tail_lines}"

    cmd = [
        *gcloud_cmd,
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
    host_lines: Optional[list[str]] = None
    viewer_lines: Optional[list[str]] = None

    if args.host_log:
        host_lines = read_lines(pathlib.Path(args.host_log))
    if args.viewer_log:
        viewer_lines = read_lines(pathlib.Path(args.viewer_log))

    target_session = args.session
    if args.session.lower() == "auto":
        inferred, source = select_auto_session(host_lines, viewer_lines)
        if not inferred:
            print(
                "Could not auto-detect session from host/viewer logs. "
                "Provide --session <id> explicitly.",
                file=sys.stderr,
            )
            return 2
        target_session = inferred
        print(f"Auto-selected session: {target_session} (source={source})")

    merged: list[dict[str, Any]] = []

    if host_lines is not None:
        host_session = find_session_in_probe_lines(host_lines, "host")
        if host_session and host_session != target_session:
            print(
                f"warning: host log session is {host_session}, "
                f"but --session is {target_session}",
                file=sys.stderr,
            )
        host_entries = parse_probe_lines(host_lines, "host", target_session)
        if not host_entries:
            host_entries = parse_legacy_probe_lines(host_lines, "host", target_session)
        merged.extend(host_entries)
        merged.extend(parse_host_diag_lines(host_lines, target_session))

    if viewer_lines is not None:
        viewer_session = find_session_in_probe_lines(viewer_lines, "viewer")
        if viewer_session and viewer_session != target_session:
            print(
                f"warning: viewer log session is {viewer_session}, "
                f"but --session is {target_session}",
                file=sys.stderr,
            )
        viewer_entries = parse_probe_lines(viewer_lines, "viewer", target_session)
        if not viewer_entries:
            viewer_entries = parse_legacy_probe_lines(
                viewer_lines, "viewer", target_session
            )
        merged.extend(viewer_entries)

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
    print_highlights(merged)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
