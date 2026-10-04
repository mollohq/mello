// Tests for the microphone permission that the driver gives each app
// (MELLO_E2E_MIC_PERMISSION, src/app.ts). Run: node --test "tools/mello-driver/test/*.test.ts"
// The journey runs against test/fixtures/events/fake-app.mjs, which reports
// the variable on its state port. Nothing here needs the real binary.

import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { createServer as netServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";

import { App, DEFAULT_MIC_PERMISSION, type AppOptions } from "../src/app.ts";
import { journey, runJourney } from "../src/journey.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fakeApp = resolve(here, "fixtures/events/fake-app.mjs");
const tmp = () => mkdtempSync(join(tmpdir(), "mello-mic-"));

function app(extra: Partial<AppOptions> = {}): App {
  return new App("alice", { binary: fakeApp, cwd: root, runDir: tmp(), mcpPort: 1, ...extra });
}

/** A base port where base .. base + 2 and base + 100 .. base + 102 are free. */
async function freeBase(): Promise<number> {
  const free = (port: number) =>
    new Promise<boolean>((ok) => {
      const s = netServer();
      s.once("error", () => ok(false));
      s.listen(port, "127.0.0.1", () => s.close(() => ok(true)));
    });
  for (;;) {
    const base = 41000 + Math.floor(Math.random() * 8000);
    let all = true;
    for (const p of [base, base + 1, base + 2, base + 100, base + 101, base + 102]) all &&= await free(p);
    if (all) return base;
  }
}

test("an app gets the permission granted by default", () => {
  assert.equal(DEFAULT_MIC_PERMISSION, "granted");
  assert.equal(app().launchEnv().MELLO_E2E_MIC_PERMISSION, "granted");
});

test("the user's option sets the permission of the app", () => {
  assert.equal(app({ micPermission: "denied" }).launchEnv().MELLO_E2E_MIC_PERMISSION, "denied");
  assert.equal(app({ micPermission: "undetermined" }).launchEnv().MELLO_E2E_MIC_PERMISSION, "undetermined");
});

test("the run environment does not change the permission: the journey decides", () => {
  const env = { MELLO_E2E_MIC_PERMISSION: "denied" };
  assert.equal(app({ env }).launchEnv().MELLO_E2E_MIC_PERMISSION, "granted");
  assert.equal(app({ env, micPermission: "undetermined" }).launchEnv().MELLO_E2E_MIC_PERMISSION, "undetermined");
});

test("a journey sets the permission for each user, with or without a deep link", async () => {
  const seen: Record<string, unknown> = {};
  const j = journey({
    id: "fake.mic",
    flows: [],
    async run({ launch }) {
      for (const [name, options] of [
        ["alice", undefined],
        ["bob", { micPermission: "undetermined" }],
        ["carol", { deeplink: "mello://join/ABCD-1234", micPermission: "denied" }],
      ] as const) {
        seen[name] = (await (await launch(name, options)).state()).mic_permission;
      }
    },
  });
  const dir = tmp();
  const r = await runJourney(j, {
    binary: fakeApp,
    repoRoot: root,
    artifactsRoot: join(dir, "artifacts"),
    mcpPortBase: await freeBase(),
  });
  assert.equal(r.ok, true, r.error);
  assert.deepEqual(seen, { alice: "granted", bob: "undetermined", carol: "denied" });
});

test("a deep link as the second argument still works", async () => {
  const j = journey({
    id: "fake.mic-deeplink",
    flows: [],
    async run({ launch, expect }) {
      const alice = await launch("alice", "mello://join/ABCD-1234");
      expect((await alice.state()).mic_permission === "granted", "alice has the default permission");
    },
  });
  const r = await runJourney(j, {
    binary: fakeApp,
    repoRoot: root,
    artifactsRoot: join(tmp(), "artifacts"),
    mcpPortBase: await freeBase(),
  });
  assert.equal(r.ok, true, r.error);
});
