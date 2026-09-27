// Journey runner (plans/E2E-QA.md §6): real binary, real clicks, local stack,
// one or more users. A journey is a TypeScript module that default-exports
// `journey({...})`. The runner owns process lifetimes and artifacts; the
// journey only describes what a user does and what must be true.

import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { App, DriverError, type AppOptions } from "./app.ts";

export type JourneyContext = {
  /** Unique per run; put it in names so repeated runs never collide. */
  runId: string;
  /** Start a user. The first user gets MCP port base, the next base + 1, … */
  launch(name: string, deeplink?: string): Promise<App>;
  /** A user that `launch` started. */
  user(name: string): App;
  /** A named step: timed, logged, and reported. */
  step(title: string, fn: () => Promise<void>): Promise<void>;
  /** Fail the journey with a message unless `cond` holds. */
  expect(cond: unknown, message: string): void;
};

export type Journey = {
  /** Stable ID, for example "invite.accept-deeplink-cold". */
  id: string;
  /** Flow IDs in qa/flows.yaml that this journey covers. */
  flows: string[];
  /** Known issues that make this journey fail today, for the report. */
  knownIssues?: number[];
  run(ctx: JourneyContext): Promise<void>;
};

export function journey(j: Journey): Journey {
  return j;
}

export type RunOptions = {
  binary: string;
  repoRoot: string;
  artifactsRoot: string;
  mcpPortBase: number;
  env?: Record<string, string>;
};

type StepRecord = { title: string; ms: number; ok: boolean; error?: string };

export type RunResult = {
  journey: string;
  ok: boolean;
  ms: number;
  dir: string;
  steps: StepRecord[];
  error?: string;
};

export async function runJourney(j: Journey, opts: RunOptions): Promise<RunResult> {
  const runId = `${Date.now().toString(36)}${Math.floor(Math.random() * 1296).toString(36)}`;
  const dir = resolve(opts.artifactsRoot, `${new Date().toISOString().replace(/[:.]/g, "-")}-${j.id}`);
  mkdirSync(dir, { recursive: true });

  const users = new Map<string, App>();
  const steps: StepRecord[] = [];
  const started = Date.now();
  const log = (line: string) => process.stdout.write(`${line}\n`);

  const appOpts = (port: number): AppOptions => ({
    binary: opts.binary,
    cwd: opts.repoRoot,
    runDir: dir,
    mcpPort: port,
    env: opts.env,
  });

  const ctx: JourneyContext = {
    runId,
    async launch(name, deeplink) {
      if (users.has(name)) throw new DriverError(`user ${name} already launched`);
      const app = new App(name, appOpts(opts.mcpPortBase + users.size));
      users.set(name, app);
      await app.launch(deeplink);
      return app;
    },
    user(name) {
      const u = users.get(name);
      if (!u) throw new DriverError(`user ${name} was not launched`);
      return u;
    },
    async step(title, fn) {
      const t0 = Date.now();
      log(`  ▸ ${title}`);
      try {
        await fn();
        steps.push({ title, ms: Date.now() - t0, ok: true });
      } catch (e) {
        steps.push({ title, ms: Date.now() - t0, ok: false, error: String(e) });
        throw e;
      }
    },
    expect(cond, message) {
      if (!cond) throw new DriverError(`expectation failed: ${message}`);
    },
  };

  log(`● ${j.id}  (${dir})`);
  let error: string | undefined;
  try {
    await j.run(ctx);
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
    for (const u of users.values()) {
      if (u.running) await u.checkpoint("failure").catch(() => undefined);
    }
  } finally {
    for (const u of users.values()) await u.kill();
  }

  const result: RunResult = { journey: j.id, ok: !error, ms: Date.now() - started, dir, steps, error };
  writeFileSync(join(dir, "result.json"), JSON.stringify(result, null, 2));
  writeFileSync(join(dir, "report.md"), report(j, result, users));
  log(error ? `✗ ${j.id} failed after ${result.ms} ms\n${indent(error)}` : `✓ ${j.id} passed in ${result.ms} ms`);
  return result;
}

function indent(s: string): string {
  return s
    .split("\n")
    .map((l) => `    ${l}`)
    .join("\n");
}

/** A report a person or an agent can read without the terminal output. */
function report(j: Journey, r: RunResult, users: Map<string, App>): string {
  const lines = [
    `# ${j.id}: ${r.ok ? "PASS" : "FAIL"}`,
    "",
    `- Flows: ${j.flows.join(", ")}`,
    `- Duration: ${r.ms} ms`,
    j.knownIssues?.length ? `- Known issues: ${j.knownIssues.map((n) => `#${n}`).join(", ")}` : "",
    "",
    "| Step | ms | Result |",
    "|---|---|---|",
    ...r.steps.map((s) => `| ${s.title} | ${s.ms} | ${s.ok ? "ok" : "FAIL"} |`),
    "",
  ];
  // The core's loop watchdog logs when one command stalls voice and every
  // other command (#88). Not a failure by itself, but always worth seeing.
  const stalls = [...users.values()].flatMap((u) =>
    u
      .logTail(100_000)
      .split("\n")
      .filter((l) => l.includes("loop watchdog") && l.includes("has blocked"))
      .map((l) => `- ${u.name}: ${l.replace(/\x1b\[[0-9;]*m/g, "").replace(/^.*loop watchdog: /, "")}`),
  );
  if (stalls.length) lines.push("## Command loop stalls", "", ...stalls, "");
  if (r.error) {
    lines.push("## Error", "", "```", r.error, "```", "");
    for (const u of users.values()) {
      lines.push(`## ${u.name}: last log lines`, "", "```", u.logTail(80), "```", "");
    }
  }
  return lines.filter((l, i, a) => !(l === "" && a[i - 1] === "")).join("\n");
}
