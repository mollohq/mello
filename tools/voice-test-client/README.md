# voice-test-client

Small UI harness for voice DSP A/B testing through the normal `mello-core` path.

It lets you:

- authenticate with `DeviceAuth`
- join a real crew/channel voice session
- switch NS mode (`RNNoise` vs WebRTC NS levels)
- toggle WebRTC transient suppression and high-pass filter
- inject WAV speech/noise frames at 20ms cadence (`960` samples @ `48kHz`)
- read live debug telemetry from the audio pipeline

## Run

From `mello/tools/voice-test-client`:

```bash
cargo run
```

Use production backend:

```bash
VOICE_TEST_PRODUCTION=1 cargo run
```

For production auth, also provide Nakama keys (same values used by `client-prod.sh`):

```bash
VOICE_TEST_PRODUCTION=1 \
NAKAMA_SERVER_KEY="..." \
NAKAMA_HTTP_KEY="..." \
cargo run
```

Or from repo root, use the helper script:

```bash
./voice-test-client-prod.sh
```

Optional runtime overrides:

- `NAKAMA_HOST`
- `NAKAMA_PORT`
- `NAKAMA_SSL` (`true/false`)

## WAV Contract

Injector WAV files must be:

- mono
- 48kHz
- 16-bit PCM

If a file is not in this format, the tool rejects it.

## Dataset Bootstrap

```bash
bash scripts/fetch_dataset.sh
```

The script writes converted WAV clips into `test-data/`:

- `test-data/clean` (LibriSpeech)
- `test-data/noise` (MUSAN)
- `test-data/pairs` (NOIZEUS)

NOIZEUS notes:

- NOIZEUS is fetched from split ZIP files listed on the index page (not a single monolithic ZIP).
- If auto-discovery fails, set `NOIZEUS_ZIP_URLS` manually (space-separated direct zip URLs).
- Useful overrides:
  - `NOIZEUS_INDEX_URL`
  - `NOIZEUS_SAMPLE_COUNT`
  - `NOIZEUS_SAMPLES_PER_ZIP`

## A/B Protocol

1. Login via device auth, select crew/channel, join voice.
2. Start with `RNNoise` and run a fixed clip for 30-60s.
3. Switch to `WebRTC High` (or other levels) with same clip.
4. Repeat with:
   - clean speech
   - speech + stationary noise
   - speech + transient noise
5. Record CPU + subjective quality for each mode.

## MOS Template

Use a 1-5 scale:

- `NS mode`
- `Transient on/off`
- `High-pass on/off`
- `Noise type`
- `CPU %`
- `Speech clarity (1-5)`
- `Noise removal (1-5)`
- `Artifacts/pumping (1-5)`
- `Overall preference`
