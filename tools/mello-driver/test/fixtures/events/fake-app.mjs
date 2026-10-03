#!/usr/bin/env node
// A stand-in for the mello binary, for the driver tests. It answers the two
// ports that App.launch reads (the Slint MCP port and the state port) with
// one button labelled "Go", and writes one line per MCP request to the file
// in FAKE_APP_LOG: "<ms> <start|end> <tool>".

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

createServer((req, res) => {
  res.setHeader("Content-Type", "application/json");
  res.end(req.url === "/events" ? "[]" : JSON.stringify({ screen: "app", crews: [], members: [], open_modals: [] }));
}).listen(Number(process.env.MELLO_E2E_STATE_PORT), "127.0.0.1");
