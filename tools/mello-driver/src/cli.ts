// mello-driver command line.
//
//   node tools/mello-driver/src/cli.ts run qa/journeys/<file>.ts[#export|#*] [...] [--repeat N]
//                                        [--events <file>] [--live-screenshot-ms N]
//   node tools/mello-driver/src/cli.ts list [--json] [qa/journeys/<file>.ts ...]
//   node tools/mello-driver/src/cli.ts mcp
//
// `list` imports the journey modules and does not run them. With no files it
// reads every qa/journeys/*.ts. `--json` prints one JSON array on stdout and
// nothing else (warnings go to stderr). Each entry has id, file, export,
// selector, flows and knownIssues. `selector` is the argument for `run`, from
// the repo root. A default export that is also a named export is listed once,
// under the named export. Exit code 2, with a message on stderr, when a module
// does not import.
//
// `--events <file>` appends one JSON object per line to the file, for a live
// view of the run. Every object has `type`, `ts` (ISO 8601 UTC) and `t` (ms
// since the journey started). The types are journey_start {id, dir},
// user_launch {user, mcpPort, statePort}, step_start {index, title},
// step_end {index, title, ok, ms, error?}, action {user, text},
// screenshot {user, path} and journey_end {id, ok, ms, error?}. The events of
// each journey run follow each other in the file. Every N ms (default 2000;
// `--live-screenshot-ms 0` turns it off) the driver writes a screenshot of each
// running app to <artifacts dir>/live/<user>.png and emits `screenshot`. It
// skips a screenshot while a journey action is in flight. A failed screenshot
// goes to stderr and does not fail the journey. Without `--events` the driver
// writes no file and takes no live screenshot.
//
// Environment:
//   MELLO_BIN         app binary (default target/debug/mello). Build it with
//                     SLINT_EMIT_DEBUG_INFO=1 cargo build -p mello-client
//                       --no-default-features --features development,e2e
//   NAKAMA_HOST/PORT  local stack (default 127.0.0.1:7350)
//   MELLO_E2E_SFU_HEALTH  health address of the local SFU (default
//                     http://127.0.0.1:8080/health). `run` checks it before a
//                     journey that uses voice, and stops when it does not answer.
//   MELLO_E2E_ARTIFACTS  artifact root (default target/e2e)

import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { closeBrowser } from "./browser.ts";
import { defaultOptions, preflight, repoRoot } from "./config.ts";
import { parseRunArgs } from "./args.ts";
import { EventWriter } from "./events.ts";
import { runJourney, type Journey } from "./journey.ts";
import { defaultJourneyFiles, formatTable, listJourneys } from "./list.ts";
import { serveMcp } from "./mcp.ts";

const RUN_USAGE = "usage: cli.ts run <journey.ts> [...] [--repeat N] [--events <file>] [--live-screenshot-ms N]";

async function run(args: string[]): Promise<number> {
  const parsed = parseRunArgs(args);
  if (typeof parsed === "string") {
    console.error(`${parsed}\n${RUN_USAGE}`);
    return 2;
  }
  const { files, repeat } = parsed;
  const opts = defaultOptions();

  // `file.ts` runs the default export, `file.ts#name` one named export, and
  // `file.ts#*` every exported journey in the file.
  const journeys: Journey[] = [];
  for (const spec of files) {
    const [f, name] = spec.split("#");
    const mod = await import(pathToFileURL(resolve(f)).href);
    const isJourney = (v: any) => v && typeof v.id === "string" && typeof v.run === "function";
    if (name === "*") {
      const all = Object.entries(mod).filter(([k, v]) => k !== "default" && isJourney(v)).map(([, v]) => v as Journey);
      journeys.push(...(all.length ? all : [mod.default as Journey]));
    } else {
      const j = mod[name ?? "default"];
      if (!isJourney(j)) throw new Error(`${spec}: no journey export "${name ?? "default"}"`);
      journeys.push(j as Journey);
    }
  }
  await preflight(opts, journeys);
  if (parsed.events) {
    opts.events = new EventWriter(resolve(parsed.events));
    opts.liveScreenshotMs = parsed.liveScreenshotMs;
  }

  let failed = 0;
  const tally = new Map<string, { pass: number; fail: number }>();
  for (let i = 0; i < repeat; i++) {
    if (repeat > 1) console.log(`\n── run ${i + 1} of ${repeat} ──`);
    for (const j of journeys) {
      const r = await runJourney(j, opts);
      const t = tally.get(j.id) ?? { pass: 0, fail: 0 };
      if (r.ok) t.pass++;
      else {
        t.fail++;
        failed++;
      }
      tally.set(j.id, t);
    }
  }
  await closeBrowser();
  opts.events?.close();
  console.log("\nSummary");
  for (const [id, t] of tally) console.log(`  ${t.fail === 0 ? "✓" : "✗"} ${id}: ${t.pass}/${t.pass + t.fail} passed`);
  return failed === 0 ? 0 : 1;
}

async function list(args: string[]): Promise<number> {
  const bad = args.find((a) => a.startsWith("--") && a !== "--json");
  if (bad) {
    console.error(`unknown option ${bad}\nusage: cli.ts list [--json] [qa/journeys/<file>.ts ...]`);
    return 2;
  }
  const json = args.includes("--json");
  const given = args.filter((a) => !a.startsWith("--"));
  const entries = await listJourneys(given.length ? given : defaultJourneyFiles(repoRoot), repoRoot);
  console.log(json ? JSON.stringify(entries, null, 2) : formatTable(entries));
  return 0;
}

const [cmd, ...rest] = process.argv.slice(2);
if (cmd === "run") {
  run(rest).then(
    (code) => process.exit(code),
    (e) => {
      console.error(`✗ ${e instanceof Error ? e.message : e}`);
      process.exit(2);
    },
  );
} else if (cmd === "list") {
  list(rest).then(
    (code) => process.exit(code),
    (e) => {
      console.error(`✗ ${e instanceof Error ? e.message : e}`);
      process.exit(2);
    },
  );
} else if (cmd === "mcp") {
  serveMcp(defaultOptions());
} else {
  console.error("usage: cli.ts run <journey.ts> [...] [--repeat N] [--events <file>] [--live-screenshot-ms N] | cli.ts list [--json] [journey.ts ...] | cli.ts mcp");
  process.exit(2);
}
