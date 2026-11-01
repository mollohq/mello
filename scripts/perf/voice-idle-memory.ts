// Memory of the Windows client while idle in a voice channel: the 100 MB rule
// (CLAUDE.md, spec 00 section 2). Not part of the e2e suite (it lives outside
// qa/journeys and holds for two minutes). Run it with the local stack and SFU:
//
//   MELLO_BIN=target/release/mello.exe MELLO_RENDERER=d2d \
//     node tools/mello-driver/src/cli.ts run scripts/perf/voice-idle-memory.ts
//
// MELLO_BIN is a release build with `--features development,e2e` and
// SLINT_EMIT_DEBUG_INFO=1. MELLO_RENDERER picks the renderer of both apps
// (d2d or skia-software). alice and bob share a crew. alice holds idle and
// logged in, then joins General with bob, then holds idle in the call. A
// sampler reads alice's private working set (Task Manager "Memory") and GPU
// memory every two seconds. The result goes to stdout and to
// target/perf/voice-idle-memory-<renderer>.json. The window stays shown.

import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

import type { App } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { twoUsersInOneCrew } from "../../qa/journeys/lib/setup.ts";
import { joinVoice } from "../../qa/journeys/lib/voice.ts";

const CHANNEL = "General";
const SETTLE_MS = 15_000;
const HOLD_MS = Number(process.env.MELLO_PERF_HOLD_MS ?? 60_000);
const EVERY_MS = 2_000;

type Sample = { t: number; privateMb: number; gpuMb: number };

/** Private working set and dedicated GPU memory of one process, in MB. */
function sample(pid: number): { privateMb: number; gpuMb: number } {
  const script =
    `$p = Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=${pid}";` +
    `$g = 0; try { $c = Get-Counter "\\GPU Process Memory(pid_${pid}*)\\Dedicated Usage" -ErrorAction Stop;` +
    ` $g = ($c.CounterSamples | Measure-Object CookedValue -Sum).Sum } catch { };` +
    `"{0} {1}" -f [int]($p.WorkingSetPrivate / 1MB), [int]($g / 1MB)`;
  const out = execFileSync("powershell", ["-NoProfile", "-Command", script], { encoding: "utf8" }).trim();
  const [privateMb, gpuMb] = out.split(/\s+/).map(Number);
  return { privateMb, gpuMb };
}

async function hold(app: App, ms: number): Promise<Sample[]> {
  const pid = app.pid;
  if (pid === undefined) throw new Error(`${app.name} is not running`);
  const samples: Sample[] = [];
  const start = Date.now();
  while (Date.now() - start < ms) {
    samples.push({ t: Math.round((Date.now() - start) / 1000), ...sample(pid) });
    await sleep(EVERY_MS);
  }
  return samples;
}

/** The median of the second half: the flat part after the settle. */
function steady(samples: Sample[], key: "privateMb" | "gpuMb"): number {
  const tail = samples.slice(Math.floor(samples.length / 2)).map((s) => s[key]).sort((a, b) => a - b);
  return tail[Math.floor(tail.length / 2)];
}

export default journey({
  id: "perf.voice-idle-memory",
  flows: [],
  voice: true,
  async run(ctx) {
    const { step } = ctx;
    const renderer = process.env.MELLO_RENDERER ?? "default";
    const { alice, bob } = await twoUsersInOneCrew(ctx);
    const phases: Record<string, Sample[]> = {};

    await step("alice idle, logged in, not in voice", async () => {
      await sleep(SETTLE_MS);
      phases.idle = await hold(alice, HOLD_MS);
    });

    await step(`alice and bob join ${CHANNEL}, alice idle in the call`, async () => {
      await joinVoice(alice, CHANNEL);
      await joinVoice(bob, CHANNEL);
      await sleep(SETTLE_MS);
      phases.voice = await hold(alice, HOLD_MS);
      await alice.checkpoint("idle-in-voice");
    });

    const result = {
      renderer,
      holdMs: HOLD_MS,
      idle: { privateMb: steady(phases.idle, "privateMb"), gpuMb: steady(phases.idle, "gpuMb") },
      voice: { privateMb: steady(phases.voice, "privateMb"), gpuMb: steady(phases.voice, "gpuMb") },
      samples: phases,
    };
    console.log(
      `voice-idle-memory ${renderer}: idle ${result.idle.privateMb} MB private (GPU ${result.idle.gpuMb}), ` +
        `in voice ${result.voice.privateMb} MB private (GPU ${result.voice.gpuMb})`,
    );
    mkdirSync("target/perf", { recursive: true });
    writeFileSync(`target/perf/voice-idle-memory-${renderer}.json`, JSON.stringify(result, null, 2));
  },
});
