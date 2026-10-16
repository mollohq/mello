// Tests for the control actions of src/app.ts: enabled, hover, drag,
// setValue and clickInRow. Run: node --test "tools/mello-driver/test/*.test.ts"
// The app is test/fixtures/events/fake-app.mjs with a scene in FAKE_APP_SCENE.
// Nothing here needs the real binary or the Nakama stack.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { createServer as netServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { after, test } from "node:test";

import { App, controlInRow } from "../src/app.ts";
import type { Control } from "../src/slint.ts";

const here = import.meta.dirname;
const root = resolve(here, "../../..");
const fakeApp = resolve(here, "fixtures/events/fake-app.mjs");

type SceneControl = { role: string; label: string; x: number; y: number; width: number; height: number; enabled?: boolean };
type SceneText = { text: string; x: number; y: number; width: number; height: number };

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

const apps: App[] = [];
after(async () => {
  for (const a of apps) await a.kill();
});

/** Start the fake app with a scene. `actions()` reads the pointer actions it got. */
async function launch(controls: SceneControl[], texts: SceneText[] = []) {
  const dir = mkdtempSync(join(tmpdir(), "mello-controls-"));
  const file = join(dir, "actions.ndjson");
  const app = new App("alice", {
    binary: fakeApp,
    cwd: root,
    runDir: dir,
    mcpPort: await freeBase(),
    env: { FAKE_APP_SCENE: JSON.stringify({ controls, texts }), FAKE_APP_ACTIONS: file },
  });
  apps.push(app);
  await app.launch();
  const actions = (): { tool: string; label: string; args: any }[] =>
    existsSync(file)
      ? readFileSync(file, "utf8")
          .split("\n")
          .filter((l) => l)
          .map((l) => JSON.parse(l))
      : [];
  return { app, actions };
}

const button = (label: string, x: number, y: number, extra: Partial<SceneControl> = {}): SceneControl => ({
  role: "Button",
  label,
  x,
  y,
  width: 16,
  height: 16,
  ...extra,
});

// ── enabled ─────────────────────────────────────────────────────

test("a control that declares accessible-enabled false reports enabled false", async () => {
  // Two cards with one label: only the one at the disabled place is off.
  const { app } = await launch([
    button("Night Owls", 10, 10, { width: 200, height: 80, enabled: false }),
    button("Night Owls", 10, 100, { width: 200, height: 80 }),
    button("Continue", 10, 200, { enabled: true }),
  ]);
  const all = await app.controls();
  assert.deepEqual(
    all.map((c) => [c.label, c.y, c.enabled]),
    [
      ["Night Owls", 10, false],
      ["Night Owls", 100, true],
      ["Continue", 200, true],
    ],
  );
  assert.equal((await app.find("Night Owls")).enabled, false);
  assert.equal((await app.find("Night Owls", 1)).enabled, true);
});

// ── hover and drag ──────────────────────────────────────────────

test("hover moves the pointer onto the control and presses nothing", async () => {
  const { app, actions } = await launch([button("Reply", 10, 10), button("More actions", 30, 10)]);
  await app.hover("More actions");
  assert.deepEqual(
    actions().map((a) => [a.tool, a.label]),
    [["hover_element", "More actions"]],
  );
});

test("drag goes to a window position, or to a distance from the center", async () => {
  const { app, actions } = await launch([button("Handle", 100, 50, { width: 20, height: 10 })]);
  await app.drag("Handle", { x: 300, y: 70 });
  await app.drag("Handle", { dx: -40 });
  assert.deepEqual(
    actions().map((a) => [a.tool, a.label, a.args.target]),
    [
      ["drag_element", "Handle", { x: 300, y: 70 }],
      ["drag_element", "Handle", { x: 70, y: 55 }],
    ],
  );
});

// ── setValue ────────────────────────────────────────────────────

test("setValue drags a slider to its place on the track, one pixel inside the ends", async () => {
  const { app, actions } = await launch([{ role: "Slider", label: "Clip position", x: 100, y: 40, width: 200, height: 20 }]);
  await app.setValue("Clip position", 0.25);
  await app.setValue("Clip position", 1);
  await app.setValue("Clip position", 0);
  assert.deepEqual(
    actions().map((a) => a.args.target),
    [
      { x: 150, y: 50 },
      { x: 299, y: 50 },
      { x: 101, y: 50 },
    ],
  );
});

test("setValue refuses a control that is not a slider, and a place off the track", async () => {
  const { app, actions } = await launch([button("Save", 10, 10), { role: "Slider", label: "Volume", x: 0, y: 0, width: 100, height: 10 }]);
  await assert.rejects(app.setValue("Save", 0.5), /"Save" is a Button, not a slider/);
  await assert.rejects(app.setValue("Volume", 1.5), /takes 0 to 1, not 1.5/);
  assert.deepEqual(actions(), [], "no pointer action");
});

// ── clickInRow ──────────────────────────────────────────────────

// A chat with three rows. Each row has its Reply in a gutter at the top. The
// third row is a reply: its quote sits above its text, so its text starts
// 24 px below its Reply.
const chat = {
  controls: [button("Reply", 10, 100), button("Reply", 10, 140), button("Reply", 10, 180)],
  texts: [
    { text: "ready?", x: 40, y: 100, width: 300, height: 18 },
    { text: "yes", x: 40, y: 140, width: 300, height: 18 },
    { text: "Replying to alice", x: 40, y: 180, width: 120, height: 16 },
    { text: "ready?", x: 170, y: 180, width: 60, height: 16 },
    { text: "go now", x: 40, y: 204, width: 300, height: 18 },
  ],
};

test("clickInRow clicks the control of the row that shows the text", async () => {
  const { app, actions } = await launch(chat.controls, chat.texts);
  await app.clickInRow("Reply", "yes");
  await app.clickInRow("Reply", "go now");
  await app.clickInRow("Reply", "ready?");
  await app.clickInRow("Reply", "ready?", 1);
  assert.deepEqual(
    actions().map((a) => a.args.elementHandle.index),
    ["4", "5", "3", "5"],
    "yes is row 2, the reply text is row 3 below its quote, ready? is row 1 and the quote in row 3",
  );
});

test("clickInRow fails with the rows and controls it saw", async () => {
  const { app, actions } = await launch(chat.controls, chat.texts);
  await assert.rejects(app.clickInRow("Reply", "no such text", 0, 300), /no control labelled "Reply" in row #0 of "no such text"\. Rows: none\. "Reply" controls: \(10,100\)/);
  await assert.rejects(app.clickInRow("Edit", "yes", 0, 300), /Rows: \(40,140\)\. "Edit" controls: none/);
  assert.deepEqual(actions(), []);
});

test("controlInRow takes the lowest control that starts above the text's bottom and reaches its top", () => {
  const c = (y: number, x = 10): Control => ({
    handle: { index: String(y), generation: "1" },
    role: "Button",
    label: "Reply",
    value: "",
    checked: false,
    enabled: true,
    x,
    y,
    width: 16,
    height: 16,
  });
  const row = { text: "t", x: 40, y: 100, width: 100, height: 18 };
  assert.equal(controlInRow([c(40), c(100), c(140)], row)?.y, 100);
  assert.equal(controlInRow([c(60)], row)?.y, 60, "a control 24 px above the text, over a reply quote");
  assert.equal(controlInRow([c(40)], row), null, "too far above: the row above");
  assert.equal(controlInRow([c(119)], row), null, "below the text: the next row");
  assert.equal(controlInRow([c(100, 600), c(100, 10)], row)?.x, 10, "the nearest of two at one height");
});
