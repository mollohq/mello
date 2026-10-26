// The art Worker base that the driver gives each app (MELLO_ART_BASE, src/app.ts).
// Run: node --test "tools/mello-driver/test/*.test.ts"

import assert from "node:assert/strict";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { test } from "node:test";

import { App, DEFAULT_ART_BASE, type AppOptions } from "../src/app.ts";

const here = import.meta.dirname;
const app = (extra: Partial<AppOptions> = {}) =>
  new App("alice", {
    binary: resolve(here, "fixtures/events/fake-app.mjs"),
    cwd: resolve(here, "../../.."),
    runDir: mkdtempSync(join(tmpdir(), "mello-art-")),
    mcpPort: 1,
    ...extra,
  });

test("an app asks the local art Worker, whatever the shell has", () => {
  const was = process.env.MELLO_ART_BASE;
  process.env.MELLO_ART_BASE = "https://art.m3llo.app";
  try {
    assert.equal(DEFAULT_ART_BASE, "http://127.0.0.1:8787");
    assert.equal(app().launchEnv().MELLO_ART_BASE, DEFAULT_ART_BASE);
  } finally {
    if (was === undefined) delete process.env.MELLO_ART_BASE;
    else process.env.MELLO_ART_BASE = was;
  }
});

test("a journey can point the app at another Worker", () => {
  assert.equal(app({ env: { MELLO_ART_BASE: "http://127.0.0.1:9999" } }).launchEnv().MELLO_ART_BASE, "http://127.0.0.1:9999");
});

test("each app has its own art cache dir", () => {
  const a = app();
  assert.match(String(a.launchEnv().MELLO_ART_CACHE_DIR), /art$/);
});
