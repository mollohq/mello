// MCP server over stdio for the agent runners (plans/E2E-QA.md §7).
//
// One server drives every test user. It implements the small part of MCP that
// a tool server needs (initialize, tools/list, tools/call, ping) as
// newline-delimited JSON-RPC, so the driver needs no MCP SDK dependency.
//
// Register it with an agent, for example:
//   { "mcpServers": { "mello-driver": { "command": "node",
//       "args": ["tools/mello-driver/src/cli.ts", "mcp"] } } }

import { mkdirSync } from "node:fs";
import { createInterface } from "node:readline";
import { resolve } from "node:path";

import { App, type AppState } from "./app.ts";
import { preflight } from "./config.ts";
import type { RunOptions } from "./journey.ts";

type ToolResult = { content: { type: string; text?: string; data?: string; mimeType?: string }[]; isError?: boolean };
type Args = Record<string, any>;

const USER = { user: { type: "string", description: "Test user name, for example alice. One isolated app per name." } };

const TOOLS: { name: string; description: string; inputSchema: object }[] = [
  {
    name: "launch",
    description:
      "Start a fresh, isolated mello app for a user (own config, session and ports). Optional deeplink, for example mello://join/ABCD-1234, opens as on a cold start.",
    inputSchema: { type: "object", properties: { ...USER, deeplink: { type: "string" } }, required: ["user"] },
  },
  {
    name: "restart",
    description: "Quit the user's app and start it again with the same config and session (a returning user).",
    inputSchema: { type: "object", properties: USER, required: ["user"] },
  },
  {
    name: "kill",
    description: "Quit the user's app.",
    inputSchema: { type: "object", properties: USER, required: ["user"] },
  },
  {
    name: "open_link",
    description: "Open a deep link in the user's running app, as the OS does when a link is clicked.",
    inputSchema: { type: "object", properties: { ...USER, url: { type: "string" } }, required: ["user", "url"] },
  },
  {
    name: "controls",
    description:
      "List the controls on the user's screen: role, label (the text the user reads), value, checked. Use a label with click or type.",
    inputSchema: { type: "object", properties: USER, required: ["user"] },
  },
  {
    name: "click",
    description: "Real pointer click on the control with this exact label. n picks among equal labels (0 = first).",
    inputSchema: {
      type: "object",
      properties: { ...USER, label: { type: "string" }, n: { type: "number" } },
      required: ["user", "label"],
    },
  },
  {
    name: "type",
    description: "Focus the text field with this label, clear it, and type the text with real key events.",
    inputSchema: {
      type: "object",
      properties: { ...USER, label: { type: "string" }, text: { type: "string" } },
      required: ["user", "label", "text"],
    },
  },
  {
    name: "key",
    description: 'Send a key to the focused element. "\\n" is Enter.',
    inputSchema: { type: "object", properties: { ...USER, text: { type: "string" } }, required: ["user", "text"] },
  },
  {
    name: "screenshot",
    description: "A PNG screenshot of the user's window. Also saved to the artifact folder.",
    inputSchema: { type: "object", properties: { ...USER, name: { type: "string" } }, required: ["user"] },
  },
  {
    name: "state",
    description:
      "The app state from the state port: screen, onboarding_step, logged_in, user_name, crews, members, in_voice, join_crew_modal_open, errors.",
    inputSchema: { type: "object", properties: USER, required: ["user"] },
  },
  {
    name: "events",
    description:
      "The last core events, oldest first. Error events carry a message; the app never shows these to the user, so check them.",
    inputSchema: { type: "object", properties: { ...USER, since_seq: { type: "number" } }, required: ["user"] },
  },
  {
    name: "wait_for",
    description:
      "Wait until a state field equals a value, or a list field contains a value. Fails with the last state on timeout. Prefer this to sleeping.",
    inputSchema: {
      type: "object",
      properties: {
        ...USER,
        field: { type: "string", description: "A field of the state tool's output, for example screen or crews." },
        equals: { description: "The value the field must equal." },
        contains: { type: "string", description: "A value a list field must contain." },
        timeout_ms: { type: "number" },
      },
      required: ["user", "field"],
    },
  },
  {
    name: "read_text",
    description: "Visible texts on the user's screen that contain a fragment (all visible texts when the fragment is empty).",
    inputSchema: { type: "object", properties: { ...USER, contains: { type: "string" } }, required: ["user"] },
  },
  {
    name: "log_tail",
    description: "The last lines of the user's app log.",
    inputSchema: { type: "object", properties: { ...USER, lines: { type: "number" } }, required: ["user"] },
  },
];

export function serveMcp(opts: RunOptions): void {
  const runDir = resolve(opts.artifactsRoot, `mcp-${new Date().toISOString().replace(/[:.]/g, "-")}`);
  mkdirSync(runDir, { recursive: true });
  const users = new Map<string, App>();
  let checked = false;

  const user = (name: string): App => {
    const u = users.get(name);
    if (!u) throw new Error(`user ${name} is not launched; call launch first`);
    return u;
  };
  const text = (value: unknown): ToolResult => ({
    content: [{ type: "text", text: typeof value === "string" ? value : JSON.stringify(value, null, 2) }],
  });

  async function call(name: string, a: Args): Promise<ToolResult> {
    switch (name) {
      case "launch": {
        if (!checked) {
          await preflight(opts);
          checked = true;
        }
        if (users.get(a.user)?.running) throw new Error(`${a.user} is already running`);
        const app =
          users.get(a.user) ??
          new App(a.user, {
            binary: opts.binary,
            cwd: opts.repoRoot,
            runDir,
            mcpPort: opts.mcpPortBase + users.size,
            env: opts.env,
          });
        users.set(a.user, app);
        await app.launch(a.deeplink);
        return text(await app.state());
      }
      case "restart":
        await user(a.user).restart();
        return text(await user(a.user).state());
      case "kill":
        await user(a.user).kill();
        return text(`${a.user} stopped`);
      case "open_link":
        await user(a.user).openLink(a.url);
        return text(`opened ${a.url}`);
      case "controls":
        return text(
          (await user(a.user).controls()).map(({ role, label, value, checked }) => ({ role, label, value, checked })),
        );
      case "click":
        await user(a.user).click(a.label, a.n ?? 0);
        return text(`clicked "${a.label}"`);
      case "type":
        await user(a.user).type(a.label, a.text);
        return text(`typed into "${a.label}"`);
      case "key":
        await user(a.user).key(a.text);
        return text("ok");
      case "screenshot": {
        const u = user(a.user);
        const base = await u.checkpoint(a.name ?? "screenshot");
        const png = await u.ui.screenshot();
        return {
          content: [
            { type: "image", data: png.toString("base64"), mimeType: "image/png" },
            { type: "text", text: `saved ${base}.png` },
          ],
        };
      }
      case "state":
        return text(await user(a.user).state());
      case "events":
        return text((await user(a.user).events()).filter((e) => e.seq > (a.since_seq ?? 0)));
      case "wait_for": {
        const field = a.field as keyof AppState;
        const pred = (s: AppState) => {
          const v = s[field] as unknown;
          if (a.contains !== undefined) return Array.isArray(v) && v.includes(a.contains);
          return JSON.stringify(v) === JSON.stringify(a.equals);
        };
        const what = a.contains !== undefined ? `${a.field} contains ${a.contains}` : `${a.field} == ${JSON.stringify(a.equals)}`;
        return text(await user(a.user).waitFor(what, pred, a.timeout_ms ?? 15_000));
      }
      case "read_text": {
        const all = await user(a.user).ui.texts();
        return text(all.filter((t) => t.includes(a.contains ?? "")));
      }
      case "log_tail":
        return text(user(a.user).logTail(a.lines ?? 100));
      default:
        throw new Error(`unknown tool ${name}`);
    }
  }

  const send = (msg: object) => process.stdout.write(`${JSON.stringify(msg)}\n`);
  const rl = createInterface({ input: process.stdin });
  rl.on("line", async (line) => {
    if (!line.trim()) return;
    let msg: any;
    try {
      msg = JSON.parse(line);
    } catch {
      return send({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" } });
    }
    const reply = (result: object) => msg.id !== undefined && send({ jsonrpc: "2.0", id: msg.id, result });
    switch (msg.method) {
      case "initialize":
        return reply({
          protocolVersion: msg.params?.protocolVersion ?? "2025-06-18",
          capabilities: { tools: {} },
          serverInfo: { name: "mello-driver", version: "0.1.0" },
          instructions:
            "Drive real mello apps as test users against the local stack. Find controls with `controls` and act by label. " +
            "Wait on state with `wait_for`, never by sleeping. Check `events` for Error events: the app does not show them. " +
            `Artifacts: ${runDir}`,
        });
      case "tools/list":
        return reply({ tools: TOOLS });
      case "tools/call":
        try {
          return reply(await call(msg.params.name, msg.params.arguments ?? {}));
        } catch (e) {
          return reply({ content: [{ type: "text", text: e instanceof Error ? e.message : String(e) }], isError: true });
        }
      case "ping":
        return reply({});
      default:
        if (msg.id !== undefined) send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `no method ${msg.method}` } });
    }
  });
  rl.on("close", async () => {
    for (const u of users.values()) await u.kill();
    process.exit(0);
  });
}
