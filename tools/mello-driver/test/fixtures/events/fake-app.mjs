#!/usr/bin/env node
// A stand-in for the mello binary, for the driver tests. It answers the two
// ports that App.launch reads (the Slint MCP port and the state port) with
// one button labelled "Go", and writes one line per MCP request to the file
// in FAKE_APP_LOG: "<ms> <start|end> <tool>".
//
// With FAKE_APP_VOICE_TRANSPORT ("sfu", "p2p" or "disconnected") the app is in
// a voice call that started on that transport: the state port reports it, and
// the event tail has the VoiceJoined and VoiceStateChanged of the join.
//
// The state port reports MELLO_E2E_MIC_PERMISSION as `mic_permission`, as the
// real app does with the e2e-mic feature.

import { appendFileSync } from "node:fs";
import { createServer } from "node:http";

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
      const button = JSON.stringify(args.queryStack).includes('"Button"');
      return text({ elementHandles: button ? [{ index: "3", generation: "1" }] : [] });
    }
    case "get_element_properties":
      return text({
        accessibleLabel: "Go",
        accessibleRole: "Button",
        size: { width: 40, height: 20 },
        absolutePosition: { x: 10, y: 10 },
      });
    case "click_element":
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
  res.end(JSON.stringify(req.url === "/events" ? events : state));
}).listen(Number(process.env.MELLO_E2E_STATE_PORT), "127.0.0.1");
