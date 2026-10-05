// One test user = one real mello process, isolated from every other user and
// from the developer's own client (plans/E2E-QA.md §5.2).

import { spawn, type ChildProcess, type SpawnOptions } from "node:child_process";
import { appendFileSync, mkdirSync, openSync, renameSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
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
  /** Onboarding step 3: why linking an identity failed. */
  link_error: string;
  /** A sign-in or link is in progress (the spinner shows). */
  login_loading: boolean;
  /** Onboarding step 2 and the welcome screen: the crew of the invite link. */
  onboarding_invite_crew_name: string;
  /** The welcome screen: who made the invite. Empty when there is none. */
  onboarding_invite_inviter: string;
  /** The invite welcome screen (onboarding step 5) is on screen. */
  invite_welcome: boolean;
  /** Onboarding skipped step 1 for an invite: two steps, not three. */
  onboarding_invite_path: boolean;
  /** The join modal: who made the invite. Empty when there is none. */
  join_crew_inviter: string;
  /** Onboarding step 1: why the invite link could not be used. */
  onboarding_invite_error: string;
  /** The invite-code card on step 1: why the typed text was refused. */
  onboarding_invite_code_error: string;
  /** The invite-code card on step 1: a resolve is running. */
  onboarding_invite_code_checking: boolean;
  /** The welcome screen and step 2: the invited crew shows its avatar. */
  onboarding_invite_crew_has_avatar: boolean;
  /** Onboarding step 2: the avatar cards that show their image, in grid order. */
  avatar_loaded: boolean[];
  /** Onboarding step 2: the chosen avatar. 0 to 6 is a card in the grid, 7 is an upload, -1 is none. */
  selected_avatar: number;
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
  /** The microphone permission that the control bar shows. */
  mic_permission: MicPermission;
  /** Open modals by name: settings, crew_settings, new_crew, join_crew, invite_share, … */
  open_modals: string[];
  /** The last 20 chat messages in the active crew, oldest first. */
  messages: { sender: string; text: string }[];
  voice_channels: {
    name: string;
    active: boolean;
    members: { name: string; speaking: boolean; muted: boolean; deafened: boolean }[];
  }[];
  /**
   * The transport of the current voice call: "sfu", or "p2p" after an SFU
   * join fell back. null when not in a call. A channel switch keeps the old
   * value until the new call starts: see voice.ts, expectSfuVoice.
   */
  voice_transport: "sfu" | "p2p" | null;
  last_event_seq: number;
};

/** The names in a voice channel, or [] when the channel is not listed. */
export function voiceMembers(s: AppState, channel: string): string[] {
  return s.voice_channels.find((c) => c.name === channel)?.members.map((m) => m.name) ?? [];
}

export type AppEvent = {
  seq: number;
  ts_ms: number;
  type: string;
  /** Only on Error events. */
  message?: string;
  /** Only on VoiceStateChanged: the transport the call started on, or "disconnected". */
  transport?: "sfu" | "p2p" | "disconnected";
};

/**
 * The file that stands in for the system clipboard in a run. A fresh install
 * reads it once at startup. Empty until a journey writes it, so no journey
 * reads the developer's clipboard.
 */
export function clipboardFile(runDir: string): string {
  return join(runDir, "clipboard.txt");
}

/**
 * The microphone permission of a test app (MELLO_E2E_MIC_PERMISSION, the
 * `e2e-mic` feature in mello-core). The app reports it instead of the
 * decision that macOS keeps for the app that started the driver, and a
 * request answers at once without the OS dialog: "undetermined" becomes
 * "granted", as a user who presses Allow.
 */
export type MicPermission = "granted" | "denied" | "undetermined";

/**
 * Every app gets this unless its journey asks for another value. Voice
 * journeys then find Mute and the voice controls on every machine.
 */
export const DEFAULT_MIC_PERMISSION: MicPermission = "granted";

/** What a journey can set for one user when it starts the app. */
export type UserOptions = {
  /** A deep link for argv[1], as the OS passes it. */
  deeplink?: string;
  /** Default: DEFAULT_MIC_PERMISSION. */
  micPermission?: MicPermission;
};

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
  /** The microphone permission of this user. Default: DEFAULT_MIC_PERMISSION. */
  micPermission?: MicPermission;
  /** Called with the text of each action, the same text as in actions.log. */
  onAction?: (user: string, text: string) => void;
};

export class DriverError extends Error {}

/** The fake OAuth provider's browser-side address (backend/docker-compose.e2e.yml). */
export const FAKE_OAUTH = process.env.MELLO_E2E_OAUTH_BASE ?? "http://127.0.0.1:18080";

/** Every app process this driver started and has not reaped yet. */
const live = new Set<ChildProcess>();

/**
 * Kill every app this driver started. Installed on SIGINT and SIGTERM so an
 * interrupted run never leaves test apps holding their ports and windows.
 */
export function killAllApps(): void {
  for (const p of live) p.kill("SIGKILL");
  live.clear();
}

const shutdownHooks = new Set<(signal: string) => void>();

/**
 * Run `fn` when the driver gets SIGINT or SIGTERM, before the apps are
 * killed. It must be synchronous. Returns a function that removes the hook.
 */
export function onShutdown(fn: (signal: string) => void): () => void {
  shutdownHooks.add(fn);
  return () => void shutdownHooks.delete(fn);
}

for (const sig of ["SIGINT", "SIGTERM"] as const) {
  process.once(sig, () => {
    for (const fn of shutdownHooks) {
      try {
        fn(sig);
      } catch {
        // A failing hook must not keep the apps alive.
      }
    }
    killAllApps();
    process.exit(130);
  });
}

/**
 * Start the app binary. A script binary (the test stand-in `fake-app.mjs`)
 * runs under this Node: Windows has no shebang support and refuses to start
 * a script file with EFTYPE. macOS and Linux would start it directly.
 */
export function spawnBinary(binary: string, args: string[], options: SpawnOptions): ChildProcess {
  if (/\.(mjs|cjs|js|ts)$/i.test(binary)) {
    return spawn(process.execPath, [binary, ...args], options);
  }
  return spawn(binary, args, options);
}

export class App {
  readonly name: string;
  readonly dir: string;
  readonly ui: SlintMcp;
  private readonly opts: AppOptions;
  private proc: ChildProcess | null = null;
  private shots = 0;
  private ready = false;

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
   * The saved session (the refresh token) in an e2e build. Delete it while the
   * app is stopped to simulate a lost keychain entry.
   */
  get sessionFile(): string {
    return join(this.dir, "session.token");
  }

  /**
   * The single-instance name. The lock is global on the machine, so it
   * carries the MCP port: two runs with different port bases can both have
   * an "alice".
   */
  private get instance(): string {
    return `e2e-${this.opts.mcpPort}-${this.name}`;
  }

  /** Where the app writes an OAuth URL instead of opening the system browser. */
  get browserFile(): string {
    return join(this.dir, "browser-url.txt");
  }

  /**
   * Start the app. A deep link goes first on the command line: the client
   * reads it only from argv[1] (client/src/deep_link.rs).
   */
  async launch(deeplink?: string): Promise<void> {
    if (this.proc) throw new DriverError(`${this.name}: already running`);
    const args = [...(deeplink ? [deeplink] : []), "--instance", this.instance];
    const log = openSync(this.logPath, "a");
    this.proc = spawnBinary(this.opts.binary, args, {
      cwd: this.opts.cwd,
      stdio: ["ignore", log, log],
      env: this.launchEnv(),
    });
    const child = this.proc;
    live.add(child);
    child.on("exit", () => {
      live.delete(child);
      if (this.proc === child) {
        this.proc = null;
        this.ready = false;
      }
    });
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      if (!this.proc) throw new DriverError(`${this.name}: exited during start; see ${this.logPath}`);
      if ((await this.ui.ready()) && (await this.stateOrNull())) {
        this.ready = true;
        return;
      }
      await sleep(200);
    }
    throw new DriverError(`${this.name}: no MCP or state port after 30 s; see ${this.logPath}`);
  }

  /** The environment of the app process. */
  launchEnv(): Record<string, string | undefined> {
    return {
      ...process.env,
      MELLO_CONFIG_DIR: join(this.dir, "config"),
      MELLO_SESSION_KEY: `e2e-${this.name}`,
      MELLO_E2E_SESSION_FILE: this.sessionFile,
      SLINT_MCP_PORT: String(this.opts.mcpPort),
      MELLO_E2E_STATE_PORT: String(this.statePort),
      NAKAMA_SERVER_KEY: "mello_dev_key",
      // e2e-oauth seams (mello-core/src/oauth.rs): the fake provider, the
      // browser handoff, and a short wait for a callback that never comes.
      MELLO_E2E_OAUTH_BASE: FAKE_OAUTH,
      MELLO_E2E_BROWSER_FILE: this.browserFile,
      MELLO_E2E_OAUTH_TIMEOUT_MS: "8000",
      // The machine's clipboard: one file per run, shared by every user and
      // the browser (client/src/onboarding_invite.rs).
      MELLO_E2E_CLIPBOARD_FILE: clipboardFile(this.opts.runDir),
      RUST_LOG: "info,mello=debug,mello_core=debug",
      ...this.opts.env,
      // The e2e-mic seam (mello-core/src/client/mic_permission.rs). Last, so
      // the user's option decides, not the developer's shell or the run.
      MELLO_E2E_MIC_PERMISSION: this.opts.micPermission ?? DEFAULT_MIC_PERMISSION,
    };
  }

  /**
   * Send a deep link to this user's running app, the way the OS does: a second
   * process with the same instance relays the URL over IPC and exits.
   */
  async openLink(url: string): Promise<void> {
    const relay = spawnBinary(this.opts.binary, [url, "--instance", this.instance], {
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

  /** The app is started and answers on both ports. */
  get live(): boolean {
    return this.proc !== null && this.ready;
  }

  /**
   * Screenshot for the live view (`run --events`). Written to `file` through
   * a temp file and a rename, so a reader never sees half a file. It skips
   * (returns false) while any journey action is in flight, and it holds the
   * MCP connection alone while it runs: a journey action that starts then
   * waits for it. See SlintMcp.tryExclusive.
   */
  async liveShot(file: string): Promise<boolean> {
    if (!this.live) return false;
    return this.ui.tryExclusive(async () => {
      const png = await this.ui.rawScreenshot();
      mkdirSync(dirname(file), { recursive: true });
      const tmp = `${file}.tmp`;
      writeFileSync(tmp, png);
      renameSync(tmp, file);
    });
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

  /**
   * Wait until events of these types have arrived at least once and then
   * stopped for `quietMs`. Use it where the app loads data more than once
   * and re-renders the screen each time, so an action lands on the final
   * screen, not on one that is about to be replaced.
   */
  async settle(types: string[], quietMs = 500, timeoutMs = 10_000): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const hits = (await this.events()).filter((e) => types.includes(e.type));
      const last = hits.at(-1);
      if (last && Date.now() - last.ts_ms >= quietMs) return;
      await sleep(100);
    }
    throw new DriverError(`${this.name}: ${types.join("/")} did not settle within ${timeoutMs} ms`);
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
  find(label: string, n = 0, timeoutMs = 5_000): Promise<Control> {
    return this.ui.hold(() => this.findNow(label, n, timeoutMs));
  }

  private async findNow(label: string, n: number, timeoutMs: number): Promise<Control> {
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

  /**
   * Resolve a label and act on the control at once. Slint can rebuild an
   * element between two MCP calls (a list model resets, a step re-renders),
   * which leaves the handle pointing at a destroyed element. A user's click
   * lands on whatever is at that point, so the driver does the same: it
   * resolves the label again and acts on the current element. This is not a
   * test retry. The action still fails when the control is not there.
   */
  private onControl(label: string, n: number, act: (c: Control) => Promise<void>, timeoutMs = 5_000): Promise<void> {
    return this.ui.hold(() => this.onControlNow(label, n, act, timeoutMs));
  }

  private async onControlNow(label: string, n: number, act: (c: Control) => Promise<void>, timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const c = await this.find(label, n, Math.max(500, deadline - Date.now()));
      try {
        await act(c);
        return;
      } catch (e) {
        const stale = /destroyed|Invalid handle/i.test(String(e));
        if (!stale || Date.now() > deadline) throw e;
      }
    }
  }

  /**
   * One line per action in actions.log, for post-mortems: what the driver
   * acted on, where it was, and when, next to the app's own log.
   */
  private trace(action: string, label: string, c?: Control): void {
    const where = c ? ` ${c.role} at (${c.x.toFixed(0)},${c.y.toFixed(0)}) ${c.width.toFixed(0)}x${c.height.toFixed(0)}` : "";
    appendFileSync(join(this.dir, "actions.log"), `${new Date().toISOString()} ${action} "${label}"${where}\n`);
    this.opts.onAction?.(this.name, `${action} "${label}"`);
  }

  /** A real pointer click at the control's center. */
  async click(label: string, n = 0): Promise<void> {
    await this.onControl(label, n, async (c) => {
      if (c.width <= 0 || c.height <= 0) {
        throw new DriverError(`${this.name}: "${label}" has zero size (${c.width}x${c.height})`);
      }
      this.trace("click", label, c);
      await this.ui.click(c.handle);
    });
  }

  /**
   * Activate a control through its default accessibility action. Use it only
   * for items in a PopupWindow (for example the account menu): Slint reports
   * their position relative to the popup, so a pointer click at that position
   * misses. Every other control takes a real click.
   */
  async activate(label: string, n = 0): Promise<void> {
    await this.onControl(label, n, async (c) => {
      this.trace("activate", label, c);
      await this.ui.activate(c.handle);
    });
  }

  /** Focus a text field by label, clear it, and type with real key events. */
  async type(label: string, text: string): Promise<void> {
    await this.ui.hold(async () => {
      await this.onControl(label, 0, async (c) => {
        if (c.role !== "TextInput") throw new DriverError(`${this.name}: "${label}" is a ${c.role}, not a text field`);
        this.trace("type", label, c);
        await this.ui.click(c.handle);
        if (c.value !== "") await this.ui.setValue(c.handle, "");
      });
      await this.ui.key(text);
    });
  }

  /**
   * Close a modal the way a user does when it has no close button: click
   * outside its card. The click lands on the backdrop at the center of a
   * control that the backdrop covers.
   */
  async dismiss(modal: string, outside = "Settings"): Promise<void> {
    await this.onControl(outside, 0, (c) => this.ui.click(c.handle));
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
