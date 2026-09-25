// mello-driver command line.
//
//   node tools/mello-driver/src/cli.ts run qa/journeys/<file>.ts[#export|#*] [...] [--repeat N]
//   node tools/mello-driver/src/cli.ts mcp
//
// Environment:
//   MELLO_BIN         app binary (default target/debug/mello). Build it with
//                     SLINT_EMIT_DEBUG_INFO=1 cargo build -p mello-client
//                       --no-default-features --features development,e2e
//   NAKAMA_HOST/PORT  local stack (default 127.0.0.1:7350)
//   MELLO_E2E_ARTIFACTS  artifact root (default target/e2e)

import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { closeBrowser } from "./browser.ts";
import { defaultOptions, preflight } from "./config.ts";
import { runJourney, type Journey } from "./journey.ts";
import { serveMcp } from "./mcp.ts";

async function run(args: string[]): Promise<number> {
  const repeatAt = args.indexOf("--repeat");
  const repeat = repeatAt >= 0 ? Number(args[repeatAt + 1]) : 1;
  const files = args.filter((a, i) => !a.startsWith("--") && args[i - 1] !== "--repeat");
  if (files.length === 0) {
    console.error("usage: cli.ts run <journey.ts> [...] [--repeat N]");
    return 2;
  }
  const opts = defaultOptions();
  await preflight(opts);

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
  console.log("\nSummary");
  for (const [id, t] of tally) console.log(`  ${t.fail === 0 ? "✓" : "✗"} ${id}: ${t.pass}/${t.pass + t.fail} passed`);
  return failed === 0 ? 0 : 1;
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
} else if (cmd === "mcp") {
  serveMcp(defaultOptions());
} else {
  console.error("usage: cli.ts run <journey.ts> [...] [--repeat N] | cli.ts mcp");
  process.exit(2);
}
