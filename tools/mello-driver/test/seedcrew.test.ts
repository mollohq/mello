// Tests for `cli.ts seed-crew`: argument parsing, the name offset of a rerun and the
// production guard. Run: node --test "tools/mello-driver/test/*.test.ts"

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import { test } from "node:test";

import { parseSeedCrewArgs } from "../src/args.ts";
import { ProductionGuardError } from "../src/guard.ts";
import { nextIndex, seedCrewCommand } from "../src/seedcrew.ts";
import { profilesFrom } from "../../../qa/journeys/lib/seed.ts";

test("seed-crew: defaults are three members and no extras", () => {
  assert.deepEqual(parseSeedCrewArgs(["ABC123"]), { code: "ABC123", members: 3, belts: false, recap: false });
});

test("seed-crew: options in any order", () => {
  assert.deepEqual(parseSeedCrewArgs(["--belts", "--members", "5", "CODE", "--clips", "4", "--recap"]), {
    code: "CODE", members: 5, belts: true, clips: 4, recap: true,
  });
});

test("seed-crew: bad input gives a message", () => {
  assert.match(String(parseSeedCrewArgs([])), /no invite code/);
  assert.match(String(parseSeedCrewArgs(["A", "B"])), /more than one/);
  assert.match(String(parseSeedCrewArgs(["A", "--nope"])), /unknown option --nope/);
  assert.match(String(parseSeedCrewArgs(["A", "--members"])), /needs a value/);
  assert.match(String(parseSeedCrewArgs(["A", "--members", "0"])), /whole number/);
  assert.match(String(parseSeedCrewArgs(["A", "--members", "x"])), /whole number/);
  assert.match(String(parseSeedCrewArgs(["A", "--clips", "-1"])), /whole number/);
});

test("seed-crew: a rerun starts at the first name with no account, with new names", async () => {
  const names = profilesFrom(0, 20, "r").map((p) => p.name);
  const taken = new Set(names.slice(0, 5));
  const start = await nextIndex("r", (i) => names[i], async (n) => taken.has(n));
  assert.equal(start, 5);
  const next = profilesFrom(start, 3, "r").map((p) => p.name);
  assert.deepEqual(next, names.slice(5, 8));
  assert.equal(new Set([...names.slice(0, 5), ...next]).size, 8);
});

test("seed-crew: profilesFrom is a slice of the same sequence", () => {
  assert.deepEqual(profilesFrom(2, 3, "r"), profilesFrom(0, 5, "r").slice(2));
});

test("seed-crew: refuses a host that is not local", async () => {
  const old = process.env.NAKAMA_HOST;
  process.env.NAKAMA_HOST = "203.0.113.10";
  try {
    await assert.rejects(seedCrewCommand({ code: "X", members: 1, belts: false, recap: false }), ProductionGuardError);
  } finally {
    if (old === undefined) delete process.env.NAKAMA_HOST;
    else process.env.NAKAMA_HOST = old;
  }
});

test("seed-crew: the cli exits 2 with usage on a missing code", () => {
  const r = spawnSync(process.execPath, [resolve(import.meta.dirname, "../src/cli.ts"), "seed-crew"], { encoding: "utf8" });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /no invite code/);
});
