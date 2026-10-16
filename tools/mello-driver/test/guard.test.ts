// Tests for the production guards (src/guard.ts), the preflight that runs
// them (src/config.ts) and a journey's `needs`. Run:
// node --test "tools/mello-driver/test/*.test.ts"
// The stack is test/fixtures/stack.ts. Host names resolve through a fake, or
// are IP literals that need no DNS. Nothing here leaves this machine.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { createServer as netServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";

import { App } from "../src/app.ts";
import { checkLounge, checkSfuRegions, classifyHost, isLocalAddress, loungeBoot, ProductionGuardError, type Lookup } from "../src/guard.ts";
import { journeyNeeds } from "../src/journey.ts";
import { fakeLounge, fakeNakama } from "./fixtures/stack.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fakeApp = resolve(here, "fixtures/events/fake-app.mjs");
const journeysFile = resolve(here, "fixtures/guard/journeys.ts");
const cli = resolve(here, "../src/cli.ts");
const tmp = () => mkdtempSync(join(tmpdir(), "mello-guard-"));

/** A public address from TEST-NET-3 (RFC 5737). Never routed; needs no DNS. */
const PUBLIC = "203.0.113.10";

/** A resolver with fixed answers. A name it does not know does not resolve. */
const names =
  (table: Record<string, string[]>): Lookup =>
  async (host) => {
    if (table[host]) return table[host];
    throw Object.assign(new Error(`getaddrinfo ENOTFOUND ${host}`), { code: "ENOTFOUND" });
  };

// ── Addresses ────────────────────────────────────────────────────

test("loopback, private and link-local addresses are local; public ones are not", () => {
  for (const ip of ["127.0.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.67", "169.254.1.1", "::1", "fd00::1", "fe80::1", "::ffff:192.168.1.2"]) {
    assert.equal(isLocalAddress(ip), true, ip);
  }
  for (const ip of ["35.234.113.109", PUBLIC, "172.32.0.1", "192.169.0.1", "8.8.8.8", "2a00:1450::1", "::ffff:8.8.8.8"]) {
    assert.equal(isLocalAddress(ip), false, ip);
  }
});

test("a host is local only when every address it resolves to is local", async () => {
  const resolve = names({ "lan.test": ["192.168.1.5"], "split.test": ["127.0.0.1", PUBLIC] });
  assert.deepEqual(await classifyHost("lan.test", resolve), { kind: "local" });
  assert.deepEqual(await classifyHost("split.test", resolve), { kind: "remote", addresses: [PUBLIC] });
  assert.deepEqual(await classifyHost("sfu-us.example.test", resolve), { kind: "unresolved" });
  assert.deepEqual(await classifyHost("[::1]"), { kind: "local" }, "an IP literal needs no DNS");
  await assert.rejects(
    classifyHost("flaky.test", async () => Promise.reject(Object.assign(new Error("again"), { code: "EAI_AGAIN" }))),
    ProductionGuardError,
  );
});

// ── SFU regions ──────────────────────────────────────────────────

test("an SFU region on a public host refuses the run", async () => {
  const nakama = await fakeNakama({
    regions: { "eu-west": "http://192.168.1.67:8080/health", "us-east": "https://sfu-us.m3llo.test:8080/health" },
  });
  try {
    const resolve = names({ "sfu-us.m3llo.test": [PUBLIC] });
    await assert.rejects(checkSfuRegions(nakama.url, resolve), (e: Error) => {
      assert.ok(e instanceof ProductionGuardError);
      assert.match(e.message, /the SFU region "us-east" on sfu-us\.m3llo\.test \(203\.0\.113\.10\)/);
      assert.match(e.message, /SFU_ENDPOINT_EU and SFU_ENDPOINT_US/);
      return true;
    });
  } finally {
    nakama.close();
  }
});

test("an SFU region whose host does not resolve gives a warning: the app cannot reach it", async () => {
  const nakama = await fakeNakama({
    regions: { "eu-west": "http://127.0.0.1:8080/health", "us-east": "https://sfu-us.m3llo.test:8080/health" },
  });
  try {
    const warnings = await checkSfuRegions(nakama.url, names({}));
    assert.deepEqual(warnings, ['the SFU region "us-east" is on sfu-us.m3llo.test, which does not resolve: the app cannot reach it']);
    assert.ok(nakama.requests.includes("POST /v2/rpc/get_sfu_regions"), nakama.requests.join(", "));
  } finally {
    nakama.close();
  }
});

// ── Lounge ───────────────────────────────────────────────────────

test("the lounge boot data is read from the invite page", () => {
  assert.deepEqual(loungeBoot('<p><script type="application/json" id="lounge-boot">{"voice":{"url":"http://127.0.0.1:7350"}}</script>'), {
    voice: { url: "http://127.0.0.1:7350" },
  });
  assert.equal(loungeBoot("<p>no boot</p>"), null);
});

test("a lounge that reads another Nakama refuses the run", async () => {
  const nakama = await fakeNakama({ inviteCode: "GARD-0002" });
  const lounge = await fakeLounge({ knows: "OTHR-0001" });
  try {
    await assert.rejects(checkLounge(lounge.url, nakama.url), /does not know the invite GARD-0002 of the local Nakama/);
    assert.deepEqual(lounge.requests, ["GET /join/GARD-0002"]);
  } finally {
    nakama.close();
    lounge.close();
  }
});

test("a lounge that gives guests a public Nakama for voice refuses the run", async () => {
  const nakama = await fakeNakama();
  const lounge = await fakeLounge({ voiceUrl: `https://${PUBLIC}` });
  try {
    await assert.rejects(checkLounge(lounge.url, nakama.url), new RegExp(`gives guests for voice is ${PUBLIC.replace(/\./g, "\\.")}`));
  } finally {
    nakama.close();
    lounge.close();
  }
});

test("a lounge on a public address refuses the run before any request", async () => {
  const nakama = await fakeNakama();
  try {
    await assert.rejects(checkLounge(`http://${PUBLIC}:8788`, nakama.url), /the lounge \(MELLO_E2E_LOUNGE_URL\) is 203\.0\.113\.10/);
    assert.deepEqual(nakama.requests, [], "no request to Nakama either");
  } finally {
    nakama.close();
  }
});

test("a local lounge on the local Nakama passes", async () => {
  const nakama = await fakeNakama();
  const lounge = await fakeLounge({ voiceUrl: `http://127.0.0.1:${nakama.port}` });
  try {
    await checkLounge(lounge.url, nakama.url);
  } finally {
    nakama.close();
    lounge.close();
  }
});

// ── The app's Nakama ─────────────────────────────────────────────

test("every app gets the checked Nakama last, so a production build goes there too", () => {
  const app = new App("alice", {
    binary: fakeApp,
    cwd: root,
    runDir: tmp(),
    mcpPort: 1,
    env: { NAKAMA_HOST: "mello-api.example.test", NAKAMA_PORT: "443", NAKAMA_SSL: "true" },
  });
  const env = app.launchEnv();
  assert.equal(env.NAKAMA_HOST, process.env.NAKAMA_HOST ?? "127.0.0.1");
  assert.equal(env.NAKAMA_PORT, process.env.NAKAMA_PORT ?? "7350");
  assert.equal(env.NAKAMA_SSL, "false");
});

test("a journey needs the SFU for voice, and what it lists", () => {
  const run = async () => {};
  assert.deepEqual(journeyNeeds({ id: "a", flows: [], run }), []);
  assert.deepEqual(journeyNeeds({ id: "a", flows: [], voice: true, run }), ["sfu"]);
  assert.deepEqual(journeyNeeds({ id: "a", flows: [], voice: true, needs: ["lounge", "sfu"], run }), ["sfu", "lounge"]);
});

// ── The CLI preflight ────────────────────────────────────────────

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

async function cliRun(selector: string, env: Record<string, string>): Promise<{ status: number | null; out: string; artifacts: string }> {
  const artifacts = join(tmp(), "artifacts");
  const child = spawn(process.execPath, [cli, "run", selector], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, MELLO_BIN: fakeApp, MELLO_E2E_ARTIFACTS: artifacts, MELLO_E2E_PORT_BASE: String(await freeBase()), ...env },
  });
  let out = "";
  child.stdout.on("data", (c) => (out += c));
  child.stderr.on("data", (c) => (out += c));
  const status = await new Promise<number | null>((ok) => child.on("exit", (code) => ok(code)));
  return { status, out, artifacts };
}

test("cli: a public NAKAMA_HOST refuses every journey before any request", async () => {
  const r = await cliRun(`${journeysFile}#plain`, { NAKAMA_HOST: PUBLIC, NAKAMA_PORT: "7350" });
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, /refusing to run: the Nakama of the run \(NAKAMA_HOST\) is 203\.0\.113\.10/);
  assert.doesNotMatch(r.out, /no Nakama at/, "the guard runs before the health check");
  assert.ok(!existsSync(r.artifacts), "no journey ran");
});

test("cli: a Nakama that hands out a public SFU region refuses every journey", async () => {
  const nakama = await fakeNakama({ regions: { "eu-west": "http://127.0.0.1:8080/health", "us-east": `https://${PUBLIC}:8080/health` } });
  try {
    const r = await cliRun(`${journeysFile}#plain`, { NAKAMA_HOST: "127.0.0.1", NAKAMA_PORT: String(nakama.port) });
    assert.equal(r.status, 2, r.out);
    assert.match(r.out, /refusing to run: Nakama at .* hands out the SFU region "us-east"/);
    assert.ok(!existsSync(r.artifacts), "no journey ran");
  } finally {
    nakama.close();
  }
});

test("cli: a journey that needs the lounge stops when the lounge reads another Nakama", async () => {
  const nakama = await fakeNakama();
  const lounge = await fakeLounge({ knows: "OTHR-0001" });
  try {
    const env = { NAKAMA_HOST: "127.0.0.1", NAKAMA_PORT: String(nakama.port), MELLO_E2E_LOUNGE_URL: lounge.url };
    const r = await cliRun(`${journeysFile}#lounge`, env);
    assert.equal(r.status, 2, r.out);
    assert.match(r.out, /does not know the invite/);
    assert.ok(!existsSync(r.artifacts), "no journey ran");
    const plain = await cliRun(`${journeysFile}#plain`, env);
    assert.equal(plain.status, 0, `a journey with no lounge does not check it\n${plain.out}`);
  } finally {
    nakama.close();
    lounge.close();
  }
});

test("cli: a journey that needs the lounge runs when the lounge reads the local Nakama", async () => {
  const nakama = await fakeNakama();
  const lounge = await fakeLounge();
  try {
    const r = await cliRun(`${journeysFile}#lounge`, { NAKAMA_HOST: "127.0.0.1", NAKAMA_PORT: String(nakama.port), MELLO_E2E_LOUNGE_URL: lounge.url });
    assert.equal(r.status, 0, r.out);
    assert.match(r.out, /fake\.lounge: 1\/1 passed/);
  } finally {
    nakama.close();
    lounge.close();
  }
});
