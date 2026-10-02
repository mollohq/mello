// Tests for `cli.ts list` (src/list.ts). Run: node --test "tools/mello-driver/test/*.test.ts"
// The fixtures are plain modules; none of them launches anything.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

import { defaultJourneyFiles, isJourney, listJourneys } from "../src/list.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fixtures = resolve(here, "fixtures/journeys");
const fixtureFiles = readdirSync(fixtures)
  .sort()
  .map((f) => resolve(fixtures, f));
const cli = resolve(here, "../src/cli.ts");

const FIXTURE = "tools/mello-driver/test/fixtures/journeys";

test("lists every journey once, sorted by file then export name", async () => {
  const got = await listJourneys(fixtureFiles, root);
  assert.deepEqual(
    got.map((e) => e.selector),
    [
      `${FIXTURE}/a-named.ts#first`,
      `${FIXTURE}/a-named.ts#second`,
      `${FIXTURE}/b-default-same.ts#real`,
      `${FIXTURE}/c-default-different.ts#default`,
      `${FIXTURE}/c-default-different.ts#named`,
      `${FIXTURE}/d-default-only.ts#default`,
    ],
  );
});

test("a default export that is the same object as a named export is listed once, under the named export", async () => {
  const got = await listJourneys([resolve(fixtures, "b-default-same.ts")], root);
  assert.deepEqual(got, [
    {
      id: "fixture.default-same",
      file: `${FIXTURE}/b-default-same.ts`,
      export: "real",
      selector: `${FIXTURE}/b-default-same.ts#real`,
      flows: ["F-04"],
      knownIssues: [],
    },
  ]);
});

test("a default export that is a different object is listed as default", async () => {
  const got = await listJourneys([resolve(fixtures, "c-default-different.ts")], root);
  assert.deepEqual(
    got.map((e) => [e.export, e.id]),
    [
      ["default", "fixture.default-different.default"],
      ["named", "fixture.default-different.named"],
    ],
  );
});

test("knownIssues is [] when absent and the list when present", async () => {
  const got = await listJourneys([resolve(fixtures, "a-named.ts"), resolve(fixtures, "c-default-different.ts")], root);
  const issues = Object.fromEntries(got.map((e) => [e.id, e.knownIssues]));
  assert.deepEqual(issues, {
    "fixture.named.first": [],
    "fixture.named.second": [7, 9],
    "fixture.default-different.default": [1],
    "fixture.default-different.named": [],
  });
});

test("exports that are not journeys are ignored", async () => {
  const got = await listJourneys([resolve(fixtures, "a-named.ts")], root);
  assert.deepEqual(
    got.map((e) => e.export),
    ["first", "second"],
  );
});

test("a module that fails to import is an error that names the file", async () => {
  await assert.rejects(
    listJourneys([resolve(here, "fixtures/broken/broken.ts")], root),
    /fixtures\/broken\/broken\.ts: cannot import the module: .*broken on purpose/,
  );
});

test("a file outside the repo is an error", async () => {
  await assert.rejects(listJourneys([resolve(root, "../outside.ts")], root), /not inside the repo/);
});

test("the default file set is qa/journeys/*.ts and not lib/", () => {
  const files = defaultJourneyFiles(root);
  assert.ok(files.length > 0);
  for (const f of files) {
    assert.equal(dirname(f), resolve(root, "qa/journeys"));
    assert.ok(f.endsWith(".ts"));
  }
});

test("the real journeys: unique IDs, and every selector resolves the way `run` resolves it", async () => {
  const got = await listJourneys(defaultJourneyFiles(root), root);
  assert.ok(got.length > 0);
  assert.equal(new Set(got.map((e) => e.id)).size, got.length, "journey IDs are unique");
  assert.equal(new Set(got.map((e) => e.selector)).size, got.length, "selectors are unique");
  for (const e of got) {
    assert.ok(!e.file.includes("\\"), `${e.file} uses forward slashes`);
    const [f, name] = e.selector.split("#");
    const mod = await import(pathToFileURL(resolve(root, f)).href);
    const j = mod[name ?? "default"];
    assert.ok(isJourney(j), `${e.selector} resolves to a journey`);
    assert.equal(j.id, e.id, `${e.selector} resolves to ${e.id}`);
  }
});

test("cli list --json prints one JSON array on stdout and nothing else", () => {
  const r = spawnSync(process.execPath, [cli, "list", "--json", `${FIXTURE}/a-named.ts`, `${FIXTURE}/b-default-same.ts`], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(r.status, 0, r.stderr);
  const parsed = JSON.parse(r.stdout);
  assert.deepEqual(
    parsed.map((e: { selector: string }) => e.selector),
    [`${FIXTURE}/a-named.ts#first`, `${FIXTURE}/a-named.ts#second`, `${FIXTURE}/b-default-same.ts#real`],
  );
});

test("cli list without --json prints a table with id, selector and flows", () => {
  const r = spawnSync(process.execPath, [cli, "list", `${FIXTURE}/a-named.ts`], { cwd: root, encoding: "utf8" });
  assert.equal(r.status, 0, r.stderr);
  const lines = r.stdout.trimEnd().split("\n");
  assert.match(lines[0], /^ID\s+SELECTOR\s+FLOWS$/);
  assert.match(lines[2], /^fixture\.named\.second\s+\S+a-named\.ts#second\s+F-02, F-03$/);
});

test("cli list exits non-zero with a message on stderr when a module fails to import", () => {
  const r = spawnSync(process.execPath, [cli, "list", "--json", "tools/mello-driver/test/fixtures/broken/broken.ts"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.notEqual(r.status, 0);
  assert.equal(r.stdout, "");
  assert.match(r.stderr, /cannot import the module/);
});
