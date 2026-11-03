#!/bin/sh
# Replay gate: the game clip lane of the stream gate (CLIPS.md 15.13).
#
# The reference clip and its game audio play into the replay buffer in place
# of a game window (the capture manager, the recording encoder, the rings).
# Presses at 35 s, 47 s, 60 s and 62 s capture 30 s clips with
# mello_replay_capture. Each MP4 is read back: every frame decoded through a
# stream viewer and its burned-in code read, the game audio markers found.
# No SFU and no shim: a clip is a file (the playback row has its own HTTP
# shim).
#
# Lanes:
#   --lane cpu (default)  the 720p60 clip and clip quality (5000 kbps),
#                         OpenH264 encode and decode
#   --lane hw             the 1080p60 clip and clip quality (8000 kbps), the
#                         machine's encoder and decoder
#   --lane hw --loopback  adds the real loopback row: mello_tone_player plays
#                         1 kHz bursts ON THE SPEAKERS and the replay records
#                         them through process loopback. Build the player
#                         first: cmake --build libmello/build-ci --config
#                         Release --target mello_tone_player
#   --playback            adds the streamed playback row: the press-35s clip
#                         over HTTP through a delay and rate shim (20 Mbps,
#                         50 ms), mello-core's range reader and libmello's
#                         clip player; first frame, buffering state, frame
#                         order, a seek to 20 s
#   --playback-only       records press-35s and runs that row alone
# Gates: benchmarks/baselines/replay/gates.json (per lane and clip: the start
# frame, no missing frame, the length, the game audio offset, and the PSNR,
# SSIM and VMAF floors). --write-baseline sets the lane's floors from the run
# (with --repeat N, from N runs) and writes baseline-<lane>.json.
#
# Exit code: 0 = pass, 1 = an enforced gate failed, 2 = setup error or a busy
# machine (other processes used more than 10 % of the CPU while the replay
# recorded: run it again on a quiet machine).
#
# The downloads and checks are the stream gate's: this script runs
# scripts/stream-gate.sh, which fetches and checks the pinned OpenH264 DLL and
# the vmaf CLI, builds tools/stream-gate in release and runs
# `stream-gate replay`. Disk: the clip renders once into benchmarks/clips/
# (1.7 GB for 720p, 3.7 GB for 1080p); each clip needs its luma frames and
# VMAF files for a while (about 7 GB in the cpu lane, 10 GB in the hw lane);
# the gate refuses to start without the space.
#
# Usage:
#   ./scripts/replay-gate.sh                          # CPU lane
#   ./scripts/replay-gate.sh --lane hw                # hardware lane
#   ./scripts/replay-gate.sh --no-vmaf                # PSNR and SSIM only
#   ./scripts/replay-gate.sh --repeat 3 --write-baseline   # set the floors
#   ./scripts/replay-gate.sh --playback-only          # the streamed playback row
# Output (results.json; per clip: the MP4, frames.csv, markers.csv; the
# playback row's CSVs in playback/) lands in target/replay-gate/<lane>/.
set -eu

cd "$(dirname "$0")/.."
export CI=true
# No audio device: the clip's game audio comes from the reference source.
export MELLO_AUDIO_BACKEND=test
exec ./scripts/stream-gate.sh replay "$@"
