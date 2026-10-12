#!/usr/bin/env python3
"""Score aligned speech pairs for the voice quality gate.

Scorer: PESQ wideband (ITU-T P.862.2) from the `pesq` package, MOS-LQO.
Called by tools/voice-gate with a manifest:

    {"sample_rate": 16000, "segments": [{"id": "...", "ref": "a.wav", "deg": "b.wav"}]}

Prints one JSON object on stdout. When numpy or pesq is missing it prints
{"available": false, "reason": ...} and exits 0, so the harness still
reports every structural metric.

Install the pinned scorer with scripts/voice-gate-requirements.txt.
"""
import json
import platform
import sys
import wave


def unavailable(reason):
    print(json.dumps({"available": False, "reason": reason}))
    sys.exit(0)


try:
    import importlib.metadata as metadata

    import numpy as np
    from pesq import pesq
except Exception as exc:  # noqa: BLE001 - any import failure means "no scorer"
    unavailable("pesq/numpy not importable: %s" % exc)


def read_wav(path, rate):
    with wave.open(path, "rb") as w:
        if w.getnchannels() != 1 or w.getsampwidth() != 2 or w.getframerate() != rate:
            raise ValueError("%s: need mono 16-bit %d Hz" % (path, rate))
        data = w.readframes(w.getnframes())
    # int16 scale, as the pesq package examples pass scipy-read int16 data.
    return np.frombuffer(data, dtype="<i2").astype(np.float32)


def main():
    if len(sys.argv) != 2:
        unavailable("usage: voice-gate-score.py <segments.json>")
    with open(sys.argv[1], "r", encoding="utf-8") as f:
        manifest = json.load(f)
    rate = int(manifest["sample_rate"])
    results = []
    for seg in manifest["segments"]:
        try:
            ref = read_wav(seg["ref"], rate)
            deg = read_wav(seg["deg"], rate)
            mos = float(pesq(rate, ref, deg, "wb"))
            results.append({"id": seg["id"], "mos": round(mos, 4)})
        except Exception as exc:  # noqa: BLE001 - report per segment, keep going
            results.append({"id": seg["id"], "mos": None, "error": str(exc)})
    print(
        json.dumps(
            {
                "available": True,
                "scorer": "pesq-wb (ITU-T P.862.2), pesq %s" % metadata.version("pesq"),
                "pesq_version": metadata.version("pesq"),
                "numpy_version": np.__version__,
                "python_version": platform.python_version(),
                "mode": "wb",
                "sample_rate": rate,
                "results": results,
            }
        )
    )


if __name__ == "__main__":
    main()
