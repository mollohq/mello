// Tests for `run --events` (src/events.ts, src/live.ts, src/journey.ts).
// Run: node --test "tools/mello-driver/test/*.test.ts"
// The journeys run against test/fixtures/events/fake-app.mjs, a stand-in for
// the app that answers the MCP port and the state port. Nothing here needs
// the real binary or the Nakama stack.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { createServer as netServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { test } from "node:test";

import { parseRunArgs } from "../src/args.ts";
import { EventWriter } from "../src/events.ts";
import { runJourney, type RunOptions } from "../src/journey.ts";
import { startLiveShots, type ShotTarget } from "../src/live.ts";
import { SlintMcp } from "../src/slint.ts";
import { failing, passing } from "./fixtures/events/journeys.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fakeApp = resolve(here, "fixtures/events/fake-app.mjs");
const journeysFile = resolve(here, "fixtures/events/journeys.ts");
const cli = resolve(here, "../src/cli.ts");

const tmp = () => mkdtempSync(join(tmpdir(), "mello-events-"));

type Ev = { type: string; ts: string; t: number; [k: string]: any };
const readEvents = (file: string): Ev[] =>
  readFileSync(file, "utf8")
    .split("\n")
    .filter((l) => l !== "")
    .map((l) => JSON.parse(l));

/** A base port where base and base + 100 are free. */
async function freeBase(): Promise<number> {
  const free = (port: number) =>
    new Promise<boolean>((ok) => {
      const s = netServer();
      s.once("error", () => ok(false));
      s.listen(port, "127.0.0.1", () => s.close(() => ok(true)));
    });
  for (;;) {
    const base = 41000 + Math.floor(Math.random() * 8000);
    if ((await free(base)) && (await free(base + 100))) return base;
  }
}

async function runOpts(extra: Partial<RunOptions> = {}): Promise<RunOptions & { requests: string }> {
  const dir = tmp();
  const requests = join(dir, "requests.log");
  return {
    binary: fakeApp,
    repoRoot: root,
    artifactsRoot: join(dir, "artifacts"),
    mcpPortBase: await freeBase(),
    env: { FAKE_APP_LOG: requests },
    requests,
    ...extra,
  };
}

// ── The event writer ─────────────────────────────────────────────

test("writer: one JSON object per line, with type, ts and t, written at once", () => {
  const file = join(tmp(), "sub", "events.ndjson");
  let clock = 1_000_000;
  const w = new EventWriter(file, () => clock);
  w.beginJourney();
  clock += 250;
  w.emit("step_start", { index: 0, title: "a title with \"quotes\" and\nnewline" });
  // No close yet: the line must already be in the file for a reader that tails it.
  const lines = readFileSync(file, "utf8").split("\n");
  assert.equal(lines.length, 2, "one line plus the empty tail");
  const ev = JSON.parse(lines[0]);
  assert.deepEqual(ev, {
    type: "step_start",
    ts: new Date(1_000_250).toISOString(),
    t: 250,
    index: 0,
    title: 'a title with "quotes" and\nnewline',
  });
  assert.match(ev.ts, /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/);
  w.close();
});

test("writer: appends to an existing file and starts t again for each journey", () => {
  const file = join(tmp(), "events.ndjson");
  writeFileSync(file, '{"type":"old","ts":"x","t":0}\n');
  let clock = 5_000;
  const w = new EventWriter(file, () => clock);
  w.beginJourney();
  clock += 100;
  w.emit("a");
  w.beginJourney();
  clock += 7;
  w.emit("b");
  w.close();
  const got = readEvents(file);
  assert.deepEqual(
    got.map((e) => [e.type, e.t]),
    [
      ["old", 0],
      ["a", 100],
      ["b", 7],
    ],
  );
  const again = new EventWriter(file);
  again.emit("c");
  again.close();
  assert.equal(readEvents(file).length, 4, "a new writer appends, it does not truncate");
});

// ── The sequence of a journey run ────────────────────────────────

test("a passing journey: journey_start, user_launch, steps in order, journey_end ok", async () => {
  const o = await runOpts({ liveScreenshotMs: 50 });
  const file = join(tmp(), "events.ndjson");
  o.events = new EventWriter(file);
  const r = await runJourney(passing, o);
  o.events.close();
  assert.ok(r.ok);

  const events = readEvents(file);
  for (const e of events) {
    assert.equal(typeof e.type, "string");
    assert.ok(!Number.isNaN(Date.parse(e.ts)), `ts of ${e.type}`);
    assert.ok(Number.isInteger(e.t) && e.t >= 0, `t of ${e.type}`);
  }
  assert.deepEqual(
    events.map((e) => e.type).filter((t) => t !== "action" && t !== "screenshot"),
    ["journey_start", "user_launch", "step_start", "step_end", "step_start", "step_end", "journey_end"],
  );
  assert.deepEqual(
    events.map((e) => e.t),
    [...events.map((e) => e.t)].sort((a, b) => a - b),
    "t never goes back",
  );

  const [start, launch] = events;
  assert.equal(start.id, "fake.passing");
  assert.equal(start.dir, r.dir);
  assert.equal(launch.user, "alice");
  assert.equal(launch.mcpPort, o.mcpPortBase);
  assert.equal(launch.statePort, o.mcpPortBase + 100);

  const steps = events.filter((e) => e.type.startsWith("step_"));
  assert.deepEqual(
    steps.map((e) => [e.type, e.index, e.title, e.ok]),
    [
      ["step_start", 0, "click the button", undefined],
      ["step_end", 0, "click the button", true],
      ["step_start", 1, "wait with no action", undefined],
      ["step_end", 1, "wait with no action", true],
    ],
  );
  assert.ok(steps.filter((e) => e.type === "step_end").every((e) => Number.isInteger(e.ms) && !("error" in e)));

  // The action text is the text in actions.log, without the time and the position.
  const actions = events.filter((e) => e.type === "action");
  assert.deepEqual(
    actions.map((e) => [e.user, e.text]),
    [1, 2, 3].map(() => ["alice", 'click "Go"']),
  );
  const log = readFileSync(join(r.dir, "alice", "actions.log"), "utf8").trim().split("\n");
  assert.equal(log.length, 3);
  assert.ok(log.every((l) => l.includes('click "Go"')));

  const end = events.at(-1)!;
  assert.deepEqual([end.type, end.id, end.ok, "error" in end], ["journey_end", "fake.passing", true, false]);
  assert.equal(end.ms, r.ms);
});

test("a failing journey: step_end carries the error, journey_end is not ok", async () => {
  const o = await runOpts({ liveScreenshotMs: 50 });
  const file = join(tmp(), "events.ndjson");
  o.events = new EventWriter(file);
  const r = await runJourney(failing, o);
  o.events.close();
  assert.ok(!r.ok);

  const events = readEvents(file).filter((e) => e.type !== "action" && e.type !== "screenshot");
  assert.deepEqual(
    events.map((e) => e.type),
    ["journey_start", "user_launch", "step_start", "step_end", "step_start", "step_end", "journey_end"],
  );
  const [ok, bad] = events.filter((e) => e.type === "step_end");
  assert.equal(ok.ok, true);
  assert.equal(bad.ok, false);
  assert.equal(bad.index, 1);
  assert.equal(bad.title, "expect something false");
  assert.equal(bad.error, "expectation failed: the button changed the screen");
  const end = events.at(-1)!;
  assert.equal(end.ok, false);
  assert.equal(end.error, "expectation failed: the button changed the screen");
});

test("the events of several runs follow each other in one file", async () => {
  const o = await runOpts({ liveScreenshotMs: 0 });
  const file = join(tmp(), "events.ndjson");
  o.events = new EventWriter(file);
  await runJourney(passing, o);
  await runJourney(failing, o);
  o.events.close();
  const events = readEvents(file);
  assert.deepEqual(
    events.filter((e) => e.type === "journey_start" || e.type === "journey_end").map((e) => [e.type, e.id]),
    [
      ["journey_start", "fake.passing"],
      ["journey_end", "fake.passing"],
      ["journey_start", "fake.failing"],
      ["journey_end", "fake.failing"],
    ],
  );
  const second = events.slice(events.findIndex((e) => e.id === "fake.failing"));
  assert.ok(second[0].t < 50, "t counts from the start of the second journey");
  assert.ok(!events.some((e) => e.type === "screenshot"), "--live-screenshot-ms 0 turns screenshots off");
});

test("live screenshots: the file is a whole PNG, is replaced in place, and no temp file stays", async () => {
  const o = await runOpts({ liveScreenshotMs: 50 });
  const file = join(tmp(), "events.ndjson");
  o.events = new EventWriter(file);
  const r = await runJourney(passing, o);
  o.events.close();
  const shots = readEvents(file).filter((e) => e.type === "screenshot");
  assert.ok(shots.length >= 2, `screenshots while the journey waits, got ${shots.length}`);
  for (const s of shots) {
    assert.equal(s.user, "alice");
    assert.equal(s.path, join(r.dir, "live", "alice.png"));
  }
  const png = readFileSync(join(r.dir, "live", "alice.png"));
  assert.equal(png.subarray(1, 4).toString(), "PNG");
  assert.deepEqual(readdirSync(join(r.dir, "live")), ["alice.png"]);
});

test("a journey without --events takes no live screenshot and writes no live folder", async () => {
  const o = await runOpts();
  const r = await runJourney(passing, o);
  assert.ok(r.ok);
  assert.ok(!existsSync(join(r.dir, "live")));
  const calls = readFileSync(o.requests, "utf8");
  assert.ok(!calls.includes("take_screenshot"));
});

test("live screenshots never overlap another request to the same app", async () => {
  const o = await runOpts({ liveScreenshotMs: 20 });
  o.events = new EventWriter(join(tmp(), "events.ndjson"));
  await runJourney(passing, o);
  o.events.close();
  const lines = readFileSync(o.requests, "utf8")
    .trim()
    .split("\n")
    .map((l) => l.split(" "));
  let open = 0;
  let screenshotOpen = false;
  let screenshots = 0;
  for (const [, kind, tool] of lines) {
    if (kind === "start") {
      if (tool === "take_screenshot") {
        assert.equal(open, 0, "a screenshot starts while a request is in flight");
        screenshotOpen = true;
        screenshots++;
      } else {
        assert.ok(!screenshotOpen, `${tool} starts while a screenshot is in flight`);
      }
      open++;
    } else {
      open--;
      if (tool === "take_screenshot") screenshotOpen = false;
    }
  }
  assert.ok(screenshots >= 3, `the check saw ${screenshots} screenshots`);
});

// ── The screenshot timer and the in-flight rule ──────────────────

/** A fake MCP server: every request takes `ms`, and it records the most requests at once. */
async function fakeMcp(ms: number): Promise<{ server: Server; port: number; stats: { max: number; tools: string[] } }> {
  const stats = { max: 0, tools: [] as string[] };
  let open = 0;
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", async () => {
      const msg = JSON.parse(body);
      stats.tools.push(msg.params.name);
      stats.max = Math.max(stats.max, ++open);
      await sleep(ms);
      open--;
      const content =
        msg.params.name === "take_screenshot"
          ? [{ type: "image", data: Buffer.from("PNG").toString("base64") }]
          : [{ type: "text", text: JSON.stringify({ windowHandles: [{ index: "1", generation: "1" }] }) }];
      res.end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: { content } }));
    });
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  return { server, port: (server.address() as any).port, stats };
}

test("a live screenshot is skipped while a journey request is in flight", async () => {
  const { server, port } = await fakeMcp(80);
  try {
    const ui = new SlintMcp(port);
    let ran = 0;
    const shot = () => ui.tryExclusive(async () => void (await ui.rawScreenshot(), ran++));

    const busy = ui.call("list_windows");
    assert.equal(await shot(), false, "skipped during a request");
    await busy;

    // An action that spans several requests, with gaps between them.
    const action = ui.hold(async () => {
      await ui.call("list_windows");
      await sleep(30);
      await ui.call("list_windows");
    });
    await sleep(100); // between the two requests
    assert.equal(await shot(), false, "skipped during an action, also between its requests");
    await action;

    assert.equal(await shot(), true, "runs when idle");
    assert.equal(ran, 1);
  } finally {
    server.close();
  }
});

test("a journey request that starts during a live screenshot waits for it", async () => {
  const { server, port, stats } = await fakeMcp(80);
  try {
    const ui = new SlintMcp(port);
    const shot = ui.tryExclusive(async () => void (await ui.rawScreenshot()));
    await sleep(10);
    const t0 = Date.now();
    await ui.call("list_windows");
    assert.ok(Date.now() - t0 >= 120, "the request waited for the two requests of the screenshot");
    assert.equal(await shot, true);
    assert.equal(stats.max, 1, "never two requests at once");
    assert.deepEqual(stats.tools, ["list_windows", "take_screenshot", "list_windows"]);
  } finally {
    server.close();
  }
});

test("the timer never runs two screenshots of one user at once, and skips nothing for another user", async () => {
  const running = new Map<string, number>();
  const maxPerUser = new Map<string, number>();
  const done: string[] = [];
  const target = (name: string, ms: number): ShotTarget => ({
    name,
    live: true,
    async liveShot() {
      const n = (running.get(name) ?? 0) + 1;
      running.set(name, n);
      maxPerUser.set(name, Math.max(maxPerUser.get(name) ?? 0, n));
      await sleep(ms);
      running.set(name, n - 1);
      return true;
    },
  });
  const live = startLiveShots({
    targets: () => [target("alice", 100), target("bob", 5)],
    dir: "/unused",
    intervalMs: 10,
    onShot: (user) => void done.push(user),
    onError: () => assert.fail("no error expected"),
  });
  await sleep(400);
  await live.stop();
  assert.equal(maxPerUser.get("alice"), 1, "alice: the previous shot had not finished");
  assert.equal(maxPerUser.get("bob"), 1);
  const count = (u: string) => done.filter((d) => d === u).length;
  assert.ok(count("alice") >= 2 && count("alice") <= 4, `alice ${count("alice")}`);
  assert.ok(count("bob") > count("alice"), "a slow user does not hold back another user");
});

test("a screenshot error goes to onError once, does not stop the timer, and nothing is emitted after stop", async () => {
  let calls = 0;
  const errors: string[] = [];
  const shots: string[] = [];
  const flaky: ShotTarget = {
    name: "alice",
    live: true,
    async liveShot() {
      calls++;
      if (calls <= 3) throw new Error("app closed");
      return true;
    },
  };
  const live = startLiveShots({
    targets: () => [flaky],
    dir: "/unused",
    intervalMs: 10,
    onShot: (u) => void shots.push(u),
    onError: (_u, e) => void errors.push(String(e)),
  });
  await sleep(150);
  await live.stop();
  assert.equal(errors.length, 1, "one message for a run of failures");
  assert.ok(shots.length >= 1, "the timer kept running");
  const n = shots.length;
  await sleep(60);
  assert.equal(shots.length, n, "stopped");
  assert.ok(calls >= 4);
});

test("a screenshot that fails does not fail the journey, and goes to stderr once", async () => {
  const o = await runOpts({ liveScreenshotMs: 20 });
  o.env = { ...o.env, FAKE_APP_FAIL_SHOT: "1" }; // take_screenshot answers with an error
  const file = join(tmp(), "events.ndjson");
  o.events = new EventWriter(file);
  const stderr: string[] = [];
  const original = console.error;
  console.error = (...a: unknown[]) => void stderr.push(a.join(" "));
  let r;
  try {
    r = await runJourney(passing, o);
  } finally {
    console.error = original;
  }
  o.events.close();
  assert.ok(r.ok);
  const events = readEvents(file);
  assert.equal(events.at(-1)!.ok, true);
  assert.ok(!events.some((e) => e.type === "screenshot"), "no screenshot event for a failed screenshot");
  assert.equal(stderr.length, 1, stderr.join("\n"));
  assert.match(stderr[0], /live screenshot of alice failed: .*take_screenshot/);
});

// ── The command line ─────────────────────────────────────────────

test("parseRunArgs: options, values and errors", () => {
  assert.deepEqual(parseRunArgs(["a.ts", "b.ts#x", "--repeat", "3", "--events", "e.ndjson", "--live-screenshot-ms", "0"]), {
    files: ["a.ts", "b.ts#x"],
    repeat: 3,
    events: "e.ndjson",
    liveScreenshotMs: 0,
  });
  assert.deepEqual(parseRunArgs(["a.ts"]), { files: ["a.ts"], repeat: 1 });
  assert.match(String(parseRunArgs(["a.ts", "--event", "f"])), /unknown option --event/);
  assert.match(String(parseRunArgs(["a.ts", "--events"])), /--events needs a value/);
  assert.match(String(parseRunArgs(["a.ts", "--repeat", "0"])), /whole number/);
  assert.match(String(parseRunArgs(["a.ts", "--live-screenshot-ms", "-5"])), /whole number/);
  assert.match(String(parseRunArgs(["--repeat", "2"])), /no journey/);
});

async function healthServer(): Promise<{ server: Server; port: number }> {
  const server = createServer((_req, res) => res.end("{}"));
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  return { server, port: (server.address() as any).port };
}

test("cli: --events with --repeat writes the events of each run to the file", async () => {
  const { server, port } = await healthServer();
  try {
    const dir = tmp();
    const file = join(dir, "events.ndjson");
    // Async spawn: the health server lives in this process, so it must keep answering.
    const child = spawn(
      process.execPath,
      [cli, "run", `${journeysFile}#passing`, "--repeat", "2", "--events", file, "--live-screenshot-ms", "0"],
      {
        stdio: ["ignore", "pipe", "pipe"],
        env: {
          ...process.env,
          MELLO_BIN: fakeApp,
          NAKAMA_HOST: "127.0.0.1",
          NAKAMA_PORT: String(port),
          MELLO_E2E_ARTIFACTS: join(dir, "artifacts"),
          MELLO_E2E_PORT_BASE: String(await freeBase()),
        },
      },
    );
    let out = "";
    child.stdout.on("data", (c) => (out += c));
    child.stderr.on("data", (c) => (out += c));
    const status = await new Promise<number | null>((ok) => child.on("exit", (code) => ok(code)));
    assert.equal(status, 0, out);
    const types = readEvents(file)
      .map((e) => e.type)
      .filter((t) => t !== "action");
    const one = ["journey_start", "user_launch", "step_start", "step_end", "step_start", "step_end", "journey_end"];
    assert.deepEqual(types, [...one, ...one]);
  } finally {
    server.close();
  }
});

// Windows has no POSIX signals: kill("SIGTERM") ends the process at once, so
// no handler runs and no journey_end is written. The behaviour is POSIX only.
const sigtermSkip = process.platform === "win32" ? "Windows has no SIGTERM handler" : false;

test("cli: SIGTERM stops the timer, writes journey_end and kills the app", { skip: sigtermSkip }, async () => {
  const { server, port } = await healthServer();
  try {
    const dir = tmp();
    const file = join(dir, "events.ndjson");
    const base = await freeBase();
    const child = spawn(process.execPath, [cli, "run", `${journeysFile}#hanging`, "--events", file, "--live-screenshot-ms", "50"], {
      stdio: "ignore",
      env: {
        ...process.env,
        MELLO_BIN: fakeApp,
        NAKAMA_HOST: "127.0.0.1",
        NAKAMA_PORT: String(port),
        MELLO_E2E_ARTIFACTS: join(dir, "artifacts"),
        MELLO_E2E_PORT_BASE: String(base),
      },
    });
    const exited = new Promise<number | null>((ok) => child.on("exit", (code) => ok(code)));
    const deadline = Date.now() + 20_000;
    while (!(existsSync(file) && readEvents(file).some((e) => e.type === "screenshot"))) {
      assert.ok(Date.now() < deadline, "no screenshot event before the signal");
      await sleep(50);
    }
    child.kill("SIGTERM");
    assert.equal(await exited, 130);
    const events = readEvents(file);
    const end = events.at(-1)!;
    assert.equal(end.type, "journey_end");
    assert.equal(end.ok, false);
    assert.match(end.error, /SIGTERM/);
    assert.equal(events.filter((e) => e.type === "journey_end").length, 1);
    await sleep(300);
    assert.equal(readEvents(file).length, events.length, "nothing is written after the signal");
    // The fake app is gone: its ports are free again.
    const ok = await fetch(`http://127.0.0.1:${base + 100}/state`).then(
      () => true,
      () => false,
    );
    assert.equal(ok, false, "the app was killed");
  } finally {
    server.close();
  }
});
