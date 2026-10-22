#!/usr/bin/env node
// A stand-in for the mello binary, for the driver tests. It answers the two
// ports that App.launch reads (the Slint MCP port and the state port) with
// one button labelled "Go", and writes one line per MCP request to the file
// in FAKE_APP_LOG: "<ms> <start|end> <tool>".
//
// With FAKE_APP_VOICE_TRANSPORT ("sfu" or "disconnected") the app is in
// a voice call that started on that transport: the state port reports it, and
// the event tail has the VoiceJoined and VoiceStateChanged of the join.
//
// The state port reports MELLO_E2E_MIC_PERMISSION as `mic_permission`, as the
// real app does with the e2e-mic feature.
//
// FAKE_APP_SCENE (JSON) replaces the "Go" button with a screen:
// { controls: [{ role, label, x, y, width, height, enabled? }],
//   texts: [{ text, x, y, width, height }] }. As in Slint MCP, a control's
// `accessibleEnabled` is in the answer only when it is true. The state port's
// `/disabled` lists the controls with `enabled: false`, as the real app does.
// FAKE_APP_ACTIONS is a file that gets one JSON line per pointer action
// (click, hover, drag): { tool, label, args }.

import { appendFileSync } from "node:fs";
import { createServer } from "node:http";

const scene = process.env.FAKE_APP_SCENE
  ? JSON.parse(process.env.FAKE_APP_SCENE)
  : { controls: [{ role: "Button", label: "Go", x: 10, y: 10, width: 40, height: 20 }], texts: [] };
const controls = scene.controls ?? [];
const texts = scene.texts ?? [];
// Handles: controls are 3.., texts are 1000..
const element = (index) => {
  const i = Number(index);
  if (i >= 1000) {
    const t = texts[i - 1000];
    return t && { accessibleLabel: t.text, accessibleRole: "Text", size: { width: t.width, height: t.height }, absolutePosition: { x: t.x, y: t.y } };
  }
  const c = controls[i - 3];
  return (
    c && {
      accessibleLabel: c.label,
      accessibleRole: c.role,
      ...(c.enabled === true ? { accessibleEnabled: true } : {}),
      size: { width: c.width, height: c.height },
      absolutePosition: { x: c.x, y: c.y },
    }
  );
};
const actions = process.env.FAKE_APP_ACTIONS;
const act = (tool, args) => {
  if (actions) appendFileSync(actions, `${JSON.stringify({ tool, label: element(args.elementHandle?.index)?.accessibleLabel, args })}\n`);
};

const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==", "base64");
const log = process.env.FAKE_APP_LOG;
const text = (v) => ({ content: [{ type: "text", text: JSON.stringify(v) }] });
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

function tool(name, args) {
  switch (name) {
    case "list_windows":
      return text({ windowHandles: [{ index: "1", generation: "1" }] });
    case "get_window_properties":
      return text({ rootElementHandle: { index: "2", generation: "1" } });
    case "query_element_descendants": {
      const q = args.queryStack.at(-1) ?? {};
      const role = q.matchElementAccessibleRole;
      const handles = [];
      if (role === "Text" || q.matchElementTypeName === "Text") texts.forEach((_, i) => handles.push(String(1000 + i)));
      else controls.forEach((c, i) => c.role === role && handles.push(String(3 + i)));
      return text({ elementHandles: handles.map((index) => ({ index, generation: "1" })) });
    }
    case "get_element_properties": {
      const p = element(args.elementHandle?.index);
      return p ? text(p) : { isError: true, content: [{ type: "text", text: "element destroyed" }] };
    }
    case "click_element":
    case "hover_element":
    case "drag_element":
      act(name, args);
      return text({});
    case "take_screenshot":
      if (process.env.FAKE_APP_FAIL_SHOT) return { isError: true, content: [{ type: "text", text: "window closed" }] };
      return { content: [{ type: "image", data: PNG.toString("base64"), mimeType: "image/png" }] };
    default:
      return { isError: true, content: [{ type: "text", text: `no tool ${name}` }] };
  }
}

createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", async () => {
    const msg = JSON.parse(body);
    const name = msg.params.name;
    if (log) appendFileSync(log, `${Date.now()} start ${name}\n`);
    // A screenshot and a click take time, as in the real app.
    await wait(name === "take_screenshot" ? 120 : name === "click_element" ? 60 : 5);
    const result = tool(name, msg.params.arguments ?? {});
    if (log) appendFileSync(log, `${Date.now()} end ${name}\n`);
    res.setHeader("Content-Type", "application/json");
    res.end(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }));
  });
}).listen(Number(process.env.SLINT_MCP_PORT), "127.0.0.1");

const transport = process.env.FAKE_APP_VOICE_TRANSPORT;
const state = {
  screen: "app",
  crews: [],
  members: [],
  open_modals: [],
  in_voice: false,
  voice_transport: null,
  mic_permission: process.env.MELLO_E2E_MIC_PERMISSION ?? null,
};
const events = [];
if (transport) {
  state.in_voice = transport !== "disconnected";
  state.voice_transport = transport === "disconnected" ? null : transport;
  events.push({ seq: 1, ts_ms: Date.now(), type: "VoiceJoined" }, { seq: 2, ts_ms: Date.now(), type: "VoiceStateChanged", transport });
}

createServer((req, res) => {
  res.setHeader("Content-Type", "application/json");
  const disabled = controls
    .filter((c) => c.enabled === false)
    .map(({ role, label, x, y, width, height }) => ({ role, label, x, y, width, height }));
  res.end(JSON.stringify(req.url === "/events" ? events : req.url === "/disabled" ? disabled : state));
}).listen(Number(process.env.MELLO_E2E_STATE_PORT), "127.0.0.1");
