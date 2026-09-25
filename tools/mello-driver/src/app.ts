// One test user = one real mello process, isolated from every other user and
// from the developer's own client (plans/E2E-QA.md §5.2).

import { spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, openSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { SlintMcp, type Control } from "./slint.ts";

/** The state port snapshot (client/src/e2e_state.rs). */
export type AppState = {
  screen: "onboarding" | "sign_in" | "app" | "blank";
  onboarding_step: number;
  logged_in: boolean;
  show_sign_in: boolean;
  user_id: string;
  user_name: string;
  login_error: string;
  active_crew_id: string;
  active_crew_name: string;
  crews: string[];
  members: string[];
  in_voice: boolean;
  join_crew_modal_open: boolean;
  join_crew_name: string;
  join_crew_error: string;
  mic_muted: boolean;
  deafened: boolean;
  /** Open modals by name: settings, crew_settings, new_crew, join_crew, invite_share, … */
  open_modals: string[];
  /** The last 20 chat messages in the active crew, oldest first. */
  messages: { sender: string; text: string }[];
  voice_channels: {
    name: string;
    active: boolean;
    members: { name: string; speaking: boolean; muted: boolean; deafened: boolean }[];
  }[];
  last_event_seq: number;
};

/** The names in a voice channel, or [] when the channel is not listed. */
export function voiceMembers(s: AppState, channel: string): string[] {
  return s.voice_channels.find((c) => c.name === channel)?.members.map((m) => m.name) ?? [];
}

export type AppEvent = { seq: number; ts_ms: number; type: string; message?: string };

export type AppOptions = {
  /** Path to a build with `--features development,e2e` and SLINT_EMIT_DEBUG_INFO=1. */
  binary: string;
  /** Working directory for the process (the repo root). */
  cwd: string;
  /** Per-run directory; each user gets a subfolder. */
  runDir: string;
  /** Slint MCP port; the state port is this + 100. */
  mcpPort: number;
  /** Extra environment, for example NAKAMA_HOST. */
  env?: Record<string, string>;
};

export class DriverError extends Error {}

export class App {
  readonly name: string;
  readonly dir: string;
  readonly ui: SlintMcp;
  private readonly opts: AppOptions;
  private proc: ChildProcess | null = null;
  private shots = 0;

  constructor(name: string, opts: AppOptions) {
    this.name = name;
    this.opts = opts;
    this.dir = join(opts.runDir, name);
    this.ui = new SlintMcp(opts.mcpPort);
    mkdirSync(join(this.dir, "config"), { recursive: true });
  }

  get statePort(): number {
    return this.opts.mcpPort + 100;
  }

  get logPath(): string {
    return join(this.dir, "app.log");
  }

  /**
   * Start the app. A deep link goes first on the command line: the client
   * reads it only from argv[1] (client/src/deep_link.rs).
   */
  async launch(deeplink?: string): Promise<void> {
    if (this.proc) throw new DriverError(`${this.name}: already running`);
    const args = [...(deeplink ? [deeplink] : []), "--instance", `e2e-${this.name}`];
    const log = openSync(this.logPath, "a");
    this.proc = spawn(this.opts.binary, args, {
      cwd: this.opts.cwd,
      stdio: ["ignore", log, log],
      env: {
        ...process.env,
        MELLO_CONFIG_DIR: join(this.dir, "config"),
        MELLO_SESSION_KEY: `e2e-${this.name}`,
        MELLO_E2E_SESSION_FILE: join(this.dir, "session.token"),
        SLINT_MCP_PORT: String(this.opts.mcpPort),
        MELLO_E2E_STATE_PORT: String(this.statePort),
        NAKAMA_SERVER_KEY: "mello_dev_key",
        RUST_LOG: "info,mello=debug,mello_core=debug",
        ...this.opts.env,
      },
    });
    this.proc.on("exit", () => {
      this.proc = null;
    });
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      if (!this.proc) throw new DriverError(`${this.name}: exited during start; see ${this.logPath}`);
      if ((await this.ui.ready()) && (await this.stateOrNull())) return;
      await sleep(200);
    }
    throw new DriverError(`${this.name}: no MCP or state port after 30 s; see ${this.logPath}`);
  }

  /**
   * Send a deep link to this user's running app, the way the OS does: a second
   * process with the same instance relays the URL over IPC and exits.
   */
  async openLink(url: string): Promise<void> {
    const relay = spawn(this.opts.binary, [url, "--instance", `e2e-${this.name}`], {
      cwd: this.opts.cwd,
      stdio: "ignore",
      env: { ...process.env, MELLO_CONFIG_DIR: join(this.dir, "config"), ...this.opts.env },
    });
    await new Promise<void>((resolve) => relay.on("exit", () => resolve()));
  }

  async kill(): Promise<void> {
    const p = this.proc;
    if (!p) return;
    p.kill("SIGTERM");
    for (let i = 0; i < 50 && this.proc; i++) await sleep(100);
    if (this.proc) p.kill("SIGKILL");
    for (let i = 0; i < 20 && this.proc; i++) await sleep(100);
  }

  /** Quit and start again with the same config and session: a returning user. */
  async restart(): Promise<void> {
    await this.kill();
    await this.launch();
  }

  get running(): boolean {
    return this.proc !== null;
  }

  // ── State port ────────────────────────────────────────────────

  private async stateOrNull(): Promise<AppState | null> {
    try {
      const r = await fetch(`http://127.0.0.1:${this.statePort}/state`);
      return r.ok ? ((await r.json()) as AppState) : null;
    } catch {
      return null;
    }
  }

  async state(): Promise<AppState> {
    const s = await this.stateOrNull();
    if (!s) throw new DriverError(`${this.name}: state port did not answer`);
    return s;
  }

  async events(): Promise<AppEvent[]> {
    const r = await fetch(`http://127.0.0.1:${this.statePort}/events`);
    return (await r.json()) as AppEvent[];
  }

  /**
   * Wait until `predicate` holds for the state. Never a fixed sleep: TESTING.md
   * requires waiting on the state that the assertion reads.
   */
  async waitFor(what: string, predicate: (s: AppState) => boolean, timeoutMs = 15_000): Promise<AppState> {
    const deadline = Date.now() + timeoutMs;
    let last: AppState | null = null;
    while (Date.now() < deadline) {
      last = await this.stateOrNull();
      if (last && predicate(last)) return last;
      await sleep(150);
    }
    const errors = (await this.events().catch(() => [] as AppEvent[]))
      .filter((e) => e.message)
      .map((e) => `  Error event: ${e.message}`)
      .join("\n");
    throw new DriverError(
      `${this.name}: timed out after ${timeoutMs} ms waiting for: ${what}\n` +
        `  last state: ${JSON.stringify(last)}` +
        (errors ? `\n${errors}` : ""),
    );
  }

  // ── UI ────────────────────────────────────────────────────────

  async controls(): Promise<Control[]> {
    return this.ui.controls();
  }

  /**
   * Find the Nth control with this exact label, retrying while the screen
   * settles. A control appears when the UI thread has rendered it, which can
   * trail the state port by a frame.
   */
  async find(label: string, n = 0, timeoutMs = 5_000): Promise<Control> {
    const deadline = Date.now() + timeoutMs;
    let seen: Control[] = [];
    while (Date.now() < deadline) {
      seen = await this.ui.controls();
      const hits = seen.filter((c) => c.label === label);
      if (hits.length > n) return hits[n];
      await sleep(150);
    }
    const on = [...new Set(seen.map((c) => c.label))].sort().join(", ");
    throw new DriverError(`${this.name}: no control labelled "${label}" (#${n}). On screen: ${on}`);
  }

  /** A real pointer click at the control's center. */
  async click(label: string, n = 0): Promise<void> {
    const c = await this.find(label, n);
    if (c.width <= 0 || c.height <= 0) {
      throw new DriverError(`${this.name}: "${label}" has zero size (${c.width}x${c.height})`);
    }
    await this.ui.click(c.handle);
  }

  /** Focus a text field by label, clear it, and type with real key events. */
  async type(label: string, text: string): Promise<void> {
    const c = await this.find(label);
    if (c.role !== "TextInput") throw new DriverError(`${this.name}: "${label}" is a ${c.role}, not a text field`);
    await this.ui.click(c.handle);
    if (c.value !== "") await this.ui.setValue(c.handle, "");
    await this.ui.key(text);
  }

  /**
   * Close a modal the way a user does when it has no close button: click
   * outside its card. The click lands on the backdrop at the center of a
   * control that the backdrop covers.
   */
  async dismiss(modal: string, outside = "Settings"): Promise<void> {
    const c = await this.find(outside);
    await this.ui.click(c.handle);
    await this.waitFor(`${modal} closed`, (s) => !s.open_modals.includes(modal), 5_000);
  }

  /** Press a key, for example "\n" for Enter. */
  async key(text: string): Promise<void> {
    await this.ui.key(text);
  }

  /** The first visible text that contains `fragment`, or null. */
  async text(fragment: string): Promise<string | null> {
    return (await this.ui.texts()).find((t) => t.includes(fragment)) ?? null;
  }

  /** Save a screenshot and a state dump into this user's artifact folder. */
  async checkpoint(label: string): Promise<string> {
    const base = join(this.dir, `${String(++this.shots).padStart(2, "0")}-${label.replace(/[^\w-]+/g, "_")}`);
    try {
      writeFileSync(`${base}.png`, await this.ui.screenshot());
    } catch (e) {
      writeFileSync(`${base}.png.error.txt`, String(e));
    }
    writeFileSync(`${base}.state.json`, JSON.stringify(await this.stateOrNull(), null, 2));
    return base;
  }

  /** The last lines of this user's log, for failure reports. */
  logTail(lines = 200): string {
    if (!existsSync(this.logPath)) return "";
    return readFileSync(this.logPath, "utf8").split("\n").slice(-lines).join("\n");
  }
}
