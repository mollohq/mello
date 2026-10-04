// Tests for voice through the SFU only (src/voice.ts and the SFU check in
// src/config.ts). Run: node --test "tools/mello-driver/test/*.test.ts"
// The journeys run against test/fixtures/events/fake-app.mjs. Nothing here
// needs the real binary, the Nakama stack or an SFU.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { createServer, type RequestListener, type Server } from "node:http";
import { createServer as netServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";

import type { AppEvent } from "../src/app.ts";
import { runJourney, type RunOptions } from "../src/journey.ts";
import { lastJoinTransport } from "../src/voice.ts";
import { inVoice } from "./fixtures/voice/journeys.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fakeApp = resolve(here, "fixtures/events/fake-app.mjs");
const journeysFile = resolve(here, "fixtures/voice/journeys.ts");
const cli = resolve(here, "../src/cli.ts");

const tmp = () => mkdtempSync(join(tmpdir(), "mello-voice-"));

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

/** A port that nothing listens on. */
async function closedPort(): Promise<number> {
  const s = netServer();
  await new Promise<void>((ok) => s.listen(0, "127.0.0.1", ok));
  const port = (s.address() as any).port;
  await new Promise<void>((ok) => s.close(() => ok()));
  return port;
}

async function server(handler: RequestListener): Promise<{ server: Server; port: number }> {
  const s = createServer(handler);
  await new Promise<void>((ok) => s.listen(0, "127.0.0.1", ok));
  return { server: s, port: (s.address() as any).port };
}

const ev = (seq: number, type: string, transport?: AppEvent["transport"]): AppEvent => ({
  seq,
  ts_ms: seq,
  type,
  ...(transport ? { transport } : {}),
});

// ── The transport of the last join ───────────────────────────────

test("the transport of a join is the first VoiceStateChanged after its VoiceJoined", () => {
  assert.equal(lastJoinTransport([ev(1, "VoiceJoined"), ev(2, "MicLevel"), ev(3, "VoiceStateChanged", "sfu")]), "sfu");
  assert.equal(lastJoinTransport([ev(1, "VoiceJoined"), ev(2, "VoiceStateChanged", "p2p")]), "p2p");
  assert.equal(lastJoinTransport([ev(1, "VoiceJoined"), ev(2, "VoiceStateChanged", "disconnected")]), "disconnected");
});

test("an earlier join does not answer for a later join that still runs", () => {
  // A channel switch: the first call started on the SFU, the second join runs.
  const switching = [ev(1, "VoiceJoined"), ev(2, "VoiceStateChanged", "sfu"), ev(3, "VoiceJoined"), ev(4, "VoiceUpdated")];
  assert.equal(lastJoinTransport(switching), null);
  assert.equal(lastJoinTransport([...switching, ev(5, "VoiceStateChanged", "p2p")]), "p2p");
  assert.equal(lastJoinTransport([]), null, "no join");
});

// ── The assertion in a journey ───────────────────────────────────

async function runVoice(transport: string) {
  const dir = tmp();
  const opts: RunOptions = {
    binary: fakeApp,
    repoRoot: root,
    artifactsRoot: join(dir, "artifacts"),
    mcpPortBase: await freeBase(),
    env: { FAKE_APP_VOICE_TRANSPORT: transport },
  };
  return runJourney(inVoice, opts);
}

test("a voice call over P2P fails the journey", async () => {
  const r = await runVoice("p2p");
  assert.equal(r.ok, false, "a P2P call must fail a voice journey");
  assert.match(r.error ?? "", /alice: the voice call runs over P2P, not through the SFU/);
});

test("a voice call through the SFU passes the journey", async () => {
  const r = await runVoice("sfu");
  assert.equal(r.ok, true, r.error);
});

test("a voice call that did not start fails the journey", async () => {
  const r = await runVoice("disconnected");
  assert.equal(r.ok, false);
  assert.match(r.error ?? "", /the voice call did not start/);
});

// ── The SFU check before a voice journey ─────────────────────────

/** Run the CLI with a fake Nakama that answers, and the SFU health at `sfuHealth`. */
async function cliRun(selector: string, sfuHealth: string): Promise<{ status: number | null; out: string; artifacts: string }> {
  const nakama = await server((_req, res) => res.end("{}"));
  try {
    const artifacts = join(tmp(), "artifacts");
    // Async spawn: the fake servers live in this process, so they must keep answering.
    const child = spawn(process.execPath, [cli, "run", selector], {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        MELLO_BIN: fakeApp,
        NAKAMA_HOST: "127.0.0.1",
        NAKAMA_PORT: String(nakama.port),
        MELLO_E2E_SFU_HEALTH: sfuHealth,
        MELLO_E2E_ARTIFACTS: artifacts,
        MELLO_E2E_PORT_BASE: String(await freeBase()),
        FAKE_APP_VOICE_TRANSPORT: "sfu",
      },
    });
    let out = "";
    child.stdout.on("data", (c) => (out += c));
    child.stderr.on("data", (c) => (out += c));
    const status = await new Promise<number | null>((ok) => child.on("exit", (code) => ok(code)));
    return { status, out, artifacts };
  } finally {
    nakama.server.close();
  }
}

test("cli: a voice journey stops before it starts when the SFU health address does not answer", async () => {
  const url = `http://127.0.0.1:${await closedPort()}/health`;
  const r = await cliRun(`${journeysFile}#inVoice`, url);
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, new RegExp(`no local SFU at ${url.replace(/[.]/g, "\\.")} `));
  assert.match(r.out, /Voice journeys run only through the local SFU/);
  assert.ok(!existsSync(r.artifacts), "no journey ran and no app started");
});

test("cli: a voice journey stops when the SFU health is not ok", async () => {
  const sfu = await server((_req, res) => res.end(JSON.stringify({ status: "draining" })));
  try {
    const r = await cliRun(`${journeysFile}#inVoice`, `http://127.0.0.1:${sfu.port}/health`);
    assert.equal(r.status, 2, r.out);
    assert.match(r.out, /status "draining", not "ok"/);
  } finally {
    sfu.server.close();
  }
});

test("cli: a voice journey runs when the SFU is healthy", async () => {
  const sfu = await server((_req, res) => res.end(JSON.stringify({ status: "ok" })));
  try {
    const r = await cliRun(`${journeysFile}#inVoice`, `http://127.0.0.1:${sfu.port}/health`);
    assert.equal(r.status, 0, r.out);
    assert.match(r.out, /fake\.voice: 1\/1 passed/);
  } finally {
    sfu.server.close();
  }
});

test("cli: a journey with no voice does not need the SFU", async () => {
  const r = await cliRun(`${journeysFile}#noVoice`, `http://127.0.0.1:${await closedPort()}/health`);
  assert.equal(r.status, 0, r.out);
});
