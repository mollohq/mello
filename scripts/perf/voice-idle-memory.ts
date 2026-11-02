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
// target/perf/voice-idle-memory-<tag>.json, where the tag is MELLO_PERF_TAG or
// the renderer. The window stays shown.
//
// MELLO_PERF_INSPECT runs a command halfway through each hold, with {pid}
// replaced by alice's process id, and saves its output next to the JSON as
// <tag>-idle.txt and <tag>-voice.txt. For example a working-set breakdown:
// MELLO_PERF_INSPECT="python ../m3llo-fx-lab/win/ws-breakdown.py {pid}".

import { execFileSync, execSync } from "node:child_process";
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

async function hold(app: App, ms: number, inspectTo: string): Promise<Sample[]> {
  const pid = app.pid;
  if (pid === undefined) throw new Error(`${app.name} is not running`);
  const inspect = process.env.MELLO_PERF_INSPECT;
  const samples: Sample[] = [];
  const start = Date.now();
  let inspected = false;
  while (Date.now() - start < ms) {
    samples.push({ t: Math.round((Date.now() - start) / 1000), ...sample(pid) });
    if (inspect && !inspected && Date.now() - start >= ms / 2) {
      inspected = true;
      const out = execSync(inspect.replaceAll("{pid}", String(pid)), { encoding: "utf8" });
      writeFileSync(inspectTo, out);
    }
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
    const tag = process.env.MELLO_PERF_TAG ?? renderer;
    const out = `target/perf/voice-idle-memory-${tag}`;
    mkdirSync("target/perf", { recursive: true });
    const { alice, bob } = await twoUsersInOneCrew(ctx);
    const phases: Record<string, Sample[]> = {};

    await step("alice idle, logged in, not in voice", async () => {
      await sleep(SETTLE_MS);
      phases.idle = await hold(alice, HOLD_MS, `${out}-idle.txt`);
    });

    await step(`alice and bob join ${CHANNEL}, alice idle in the call`, async () => {
      await joinVoice(alice, CHANNEL);
      await joinVoice(bob, CHANNEL);
      await sleep(SETTLE_MS);
      phases.voice = await hold(alice, HOLD_MS, `${out}-voice.txt`);
      await alice.checkpoint("idle-in-voice");
    });

    const result = {
      renderer,
      tag,
      holdMs: HOLD_MS,
      idle: { privateMb: steady(phases.idle, "privateMb"), gpuMb: steady(phases.idle, "gpuMb") },
      voice: { privateMb: steady(phases.voice, "privateMb"), gpuMb: steady(phases.voice, "gpuMb") },
      samples: phases,
    };
    console.log(
      `voice-idle-memory ${tag}: idle ${result.idle.privateMb} MB private (GPU ${result.idle.gpuMb}), ` +
        `in voice ${result.voice.privateMb} MB private (GPU ${result.voice.gpuMb})`,
    );
    writeFileSync(`${out}.json`, JSON.stringify(result, null, 2));
  },
});
