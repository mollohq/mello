// One test user = one real mello process, isolated from every other user and
// from the developer's own client (plans/E2E-QA.md §5.2).

import { spawn, type ChildProcess, type SpawnOptions } from "node:child_process";
import { appendFileSync, mkdirSync, openSync, renameSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { nakamaTarget } from "./guard.ts";
import { SlintMcp, type Control, type Point, type TextElement } from "./slint.ts";

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
  messages: ChatMessage[];
  voice_channels: {
    name: string;
    active: boolean;
    members: {
      /** The user id. */
      id: string;
      name: string;
      speaking: boolean;
      /** The member muted their own mic. */
      muted: boolean;
      deafened: boolean;
      /** The playback volume of this member for this user, 0 to 200 (%). */
      volume: number;
      /** This user muted the member for themselves only. */
      local_muted: boolean;
    }[];
  }[];
  /**
   * The transport of the current voice call: "sfu". null when not in a
   * call. A channel switch keeps the old value until the new call starts:
   * see voice.ts, expectSfuVoice.
   */
  voice_transport: "sfu" | null;
  /**
   * The other members of the watched SFU stream: the host and the other
   * viewers, in join order. Empty when not watching.
   */
  stream_members: { user_id: string; role: "host" | "viewer" }[];
  /** Onboarding step 1: why the crew list did not load. Empty when it loaded. */
  discover_error: string;
  /** Onboarding step 1: the crew cards, in screen order, at most 20. */
  onboarding_crews: CrewCard[];
  /** The Discover panel: the crew cards, in screen order, at most 20. */
  discover_crews: CrewCard[];
  /** The Discover panel: the number of crews in its list. Can be more than discover_crews holds. */
  discover_crews_count: number;
  /** The banner text when the connection to the backend is lost. Empty when connected. */
  connection_warning: string;
  /** The crew settings modal: the values in its fields. */
  crew_settings: {
    name: string;
    description: string;
    /** The crew shows in Discover and anyone can join. */
    open: boolean;
    invite_policy: "everyone" | "admins";
    my_role: CrewRole;
    /** The "saved" confirmation shows. */
    saved: boolean;
  };
  /** The member list of the active crew, in screen order, at most 20. `members` has every name. */
  crew_members: {
    id: string;
    name: string;
    role: CrewRole;
    online: boolean;
    /** A fingerprint of the avatar image on screen: equal images, equal keys. "" when the member shows initials. */
    avatar_key: string;
  }[];
  stream: {
    /** This user streams. */
    hosting: boolean;
    /** This user watches a stream. */
    watching: boolean;
    /** The size of the watched stream. 0 when unknown. */
    width: number;
    height: number;
    viewer_count: number;
    /** The quality pill: 0 Ultra, 1 High, 2 Medium, 3 Low, 4 Potato. */
    preset: number;
    /** Frames the core gave the viewer since the watch started. The core reports it once a second. */
    frames_presented: number;
    /** The live stream cards of the active crew, in screen order. */
    cards: { host_id: string; streamer_name: string; viewer_count: number; width: number; height: number; can_watch: boolean }[];
  };
  /** The feed of the active crew, in screen order (newest first), at most 20. */
  feed_cards: { id: string; card_type: string; title: string; actor_name: string; clip_path: string }[];
  /** The clip that plays in the feed. `path` is "" when none plays. */
  clip: { path: string; paused: boolean; progress: number; duration_ms: number };
  /** The head2head card of the feed. `present` is false (placement "none") when the feed has none. */
  head2head: {
    present: boolean;
    placement: "wide" | "cell" | "versus" | "none";
    member_count: number;
    open_seats: number;
    /** The chips the card draws: the viewer's own values on the wide card. */
    own_values: { title_id: string; display: string }[];
    titles: { title_id: string; holder_name: string }[];
    /** Every pair of the viewer, drawn or not. */
    pairs_count: number;
    /** The pair rows the cell draws. */
    pair_rows: number;
    /** The "+N more" line of the cell. 0 when it does not show. */
    more_pairs: number;
    belt_moved_recent: boolean;
    /** A fingerprint of the viewer's octagon image. "" when it shows initials. */
    own_avatar_key: string;
    /** The same for the opponent octagon of each pair row. */
    pair_avatar_keys: string[];
  };
  /** The invite card of the feed. `present` is false when the feed has none. */
  invite_card: { present: boolean; octagons: number; more_count: number; with_avatar: number };
  /** The recent games card of the feed. Empty tiles when the feed has none. */
  recent_games: {
    /** The week's line ("14 sessions, 31 h this week"), or "". Never a note about the PCs. */
    header_line: string;
    /** "grid" from two PCs on, "list" for one, "" with no card. */
    shape: "grid" | "list" | "";
    pc_count: number;
    tiles: { name: string; owners: number; hours_text: string; launcher: string; tracked: boolean; art: "image" | "monogram" }[];
  };
  /** The daily recap card of the feed. `present` is false when the feed has none. */
  daily_recap: {
    present: boolean;
    day: string;
    hangout: string;
    clips: number;
    sessions: number;
    top_game: string;
    members: number;
    index: number;
    belts: string[];
  };
  /** The live session cards (a game in progress), in screen order. `index` is the place in `feed_cards`. */
  live_sessions: { game_name: string; member_names: string[]; since_min: number; since_text: string; index: number; octagons: number; with_avatar: number; poster: boolean }[];
  /** The joined line at the top of the feed, top row first. */
  joined_rows: { line: string; when: string }[];
  /** The skeleton trio cells that show, 0 to 3. */
  skeleton_trio: number;
  /** The audio page of the settings. */
  audio: { capture_device: string; playback_device: string; push_to_talk: boolean; push_to_talk_key: string };
  riot: { linked: boolean; id: string };
  /** The in-app update toast shows a new version. */
  update_available: boolean;
  /** The version the settings show, for example "v0.0.0-DEV". */
  app_version: string;
  last_event_seq: number;
};

/** A chat message on the state port: the last 20 in the active crew, oldest first. */
export type ChatMessage = {
  sender: string;
  text: string;
  id: string;
  /** The message this one replies to. "" when it is not a reply. */
  reply_to_name: string;
  reply_preview: string;
  edited: boolean;
  deleted: boolean;
  gif: boolean;
  /** The message mentions this user (the row is marked). */
  mentions_me: boolean;
};

/** A crew card on onboarding step 1 or in Discover. Full when max_members > 0 and member_count >= max_members. */
export type CrewCard = {
  id: string;
  name: string;
  member_count: number;
  max_members: number;
  /** Only in Discover: anyone can join without an invite. */
  open?: boolean;
};

export type CrewRole = "owner" | "admin" | "member" | "join_request" | "unknown";

/** A control that declares accessible-enabled: false (state port `GET /disabled`). */
export type DisabledControl = { role: string; label: string; x: number; y: number; width: number; height: number };

/** True when a disabled-control record and a Slint MCP control are the same element. */
export function sameControl(d: DisabledControl, c: Control): boolean {
  const near = (a: number, b: number) => Math.abs(a - b) < 0.5;
  return d.role === c.role && d.label === c.label && near(d.x, c.x) && near(d.y, c.y) && near(d.width, c.width) && near(d.height, c.height);
}

/** How far above its text a row's control can start: a reply quote sits between them. */
export const ROW_REACH = 40;

/**
 * The control with this label in the row of `row`. A chat row has its
 * controls (Reply, More actions) in a gutter at the top of the row, and its
 * text below them when the message quotes a reply. So the control starts
 * above the text's bottom and ends at most ROW_REACH above the text's top.
 * Of those, the lowest one is in the row of the text; the rows above have
 * theirs higher up. Null when no control fits.
 */
export function controlInRow(controls: Control[], row: TextElement): Control | null {
  const fits = controls.filter(
    (c) => c.width > 0 && c.height > 0 && c.y <= row.y + row.height && c.y + c.height >= row.y - ROW_REACH,
  );
  fits.sort((a, b) => b.y - a.y || Math.abs(a.x - row.x) - Math.abs(b.x - row.x));
  return fits[0] ?? null;
}

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
  transport?: "sfu" | "disconnected";
  /** Only on ConnectionStateChanged: the realtime socket is up. */
  connected?: boolean;
  /** Only on ConnectionStateChanged: a reconnect attempt is in flight. */
  reconnecting?: boolean;
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
      // The Nakama that the production guard checked (guard.ts). Last, so a
      // build with the production defaults goes there too, and nothing in the
      // run's environment sends the app elsewhere.
      ...this.nakamaEnv(),
    };
  }

  /** NAKAMA_HOST, NAKAMA_PORT and NAKAMA_SSL of the checked local Nakama. */
  private nakamaEnv(): Record<string, string> {
    const t = nakamaTarget();
    return { NAKAMA_HOST: t.host, NAKAMA_PORT: t.port, NAKAMA_SSL: "false" };
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
   * Feed the game sensor a start or an end for a catalogue game (`counter-strike-2`),
   * through the real handler path. E2e builds only (`POST /fake-game`).
   * `agoMin` moves a start into the past, so that the session is long enough
   * for the ledger when it ends (2 min).
   */
  async fakeGame(action: "start" | "end", gameId: string, agoMin = 0): Promise<void> {
    const r = await fetch(`http://127.0.0.1:${this.statePort}/fake-game`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ action, game_id: gameId, ago_min: agoMin }),
    });
    if (!r.ok) throw new DriverError(`${this.name}: fakeGame ${action} ${gameId} failed: HTTP ${r.status} ${await r.text()}`);
  }

  /** The controls on screen that declare accessible-enabled: false. */
  async disabledControls(): Promise<DisabledControl[]> {
    const r = await fetch(`http://127.0.0.1:${this.statePort}/disabled`);
    if (!r.ok) throw new DriverError(`${this.name}: the state port has no disabled controls (HTTP ${r.status}); rebuild the app`);
    return (await r.json()) as DisabledControl[];
  }

  /** Set `enabled` on each control from the state port: Slint MCP leaves out a false value. */
  private async withEnabled(controls: Control[]): Promise<Control[]> {
    const off = await this.disabledControls();
    return controls.map((c) => ({ ...c, enabled: !off.some((d) => sameControl(d, c)) }));
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

  /** Every labelled control on screen, with `enabled` from the state port. */
  async controls(): Promise<Control[]> {
    return this.withEnabled(await this.ui.controls());
  }

  /**
   * Find the Nth control with this exact label, retrying while the screen
   * settles. A control appears when the UI thread has rendered it, which can
   * trail the state port by a frame. `enabled` comes from the state port.
   */
  async find(label: string, n = 0, timeoutMs = 5_000): Promise<Control> {
    const [c] = await this.withEnabled([await this.ui.hold(() => this.findNow(label, n, timeoutMs))]);
    return c;
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
      const c = await this.findNow(label, n, Math.max(500, deadline - Date.now()));
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
   * Move the pointer onto the control and press nothing, for hover states
   * such as the action bar of a chat message. The pointer stays there.
   */
  async hover(label: string, n = 0): Promise<void> {
    await this.onControl(label, n, async (c) => {
      this.trace("hover", label, c);
      await this.ui.hover(c.handle);
    });
  }

  /**
   * Drag the control like a user: press at its center, move in small steps,
   * release. `to` is a window position, or a distance from the center.
   */
  async drag(label: string, to: Point | { dx?: number; dy?: number }, n = 0): Promise<void> {
    await this.onControl(label, n, async (c) => {
      const target =
        "x" in to && "y" in to
          ? to
          : { x: c.x + c.width / 2 + (to.dx ?? 0), y: c.y + c.height / 2 + (to.dy ?? 0) };
      this.trace(`drag to (${target.x.toFixed(0)},${target.y.toFixed(0)})`, label, c);
      await this.ui.drag(c.handle, target);
    });
  }

  /**
   * Set a slider like a user: drag its thumb to a place on the track.
   * `value` is that place, from 0 (the left end) to 1 (the right end). The
   * app's sliders take no value through accessibility, so this uses the
   * pointer, as a user does.
   */
  async setValue(label: string, value: number, n = 0): Promise<void> {
    if (!(value >= 0 && value <= 1)) throw new DriverError(`${this.name}: setValue("${label}") takes 0 to 1, not ${value}`);
    await this.onControl(label, n, async (c) => {
      if (c.role !== "Slider") throw new DriverError(`${this.name}: "${label}" is a ${c.role}, not a slider`);
      // Stay one pixel inside the track: a release on its edge is outside it.
      const x = Math.min(Math.max(c.x + value * c.width, c.x + 1), c.x + c.width - 1);
      const target = { x, y: c.y + c.height / 2 };
      this.trace(`set value ${value} at (${x.toFixed(0)},${target.y.toFixed(0)})`, label, c);
      await this.ui.drag(c.handle, target);
    });
  }

  /**
   * Click the control with this label in the row that shows `rowText`, for
   * labels that every row has ("Reply", "More actions"). `n` picks among
   * rows that show the text, from the top. See controlInRow.
   */
  async clickInRow(label: string, rowText: string, n = 0, timeoutMs = 5_000): Promise<void> {
    await this.ui.hold(async () => {
      const deadline = Date.now() + timeoutMs;
      let rows: TextElement[] = [];
      let candidates: Control[] = [];
      for (;;) {
        rows = (await this.ui.textElements()).filter((t) => t.text.includes(rowText)).sort((a, b) => a.y - b.y);
        candidates = (await this.ui.controls()).filter((c) => c.label === label);
        const row = rows[n];
        const c = row ? controlInRow(candidates, row) : null;
        if (c) {
          try {
            this.trace(`click in row "${rowText}"`, label, c);
            await this.ui.click(c.handle);
            return;
          } catch (e) {
            if (!/destroyed|Invalid handle/i.test(String(e)) || Date.now() > deadline) throw e;
            continue;
          }
        }
        if (Date.now() > deadline) break;
        await sleep(150);
      }
      const at = (r: { x: number; y: number }) => `(${r.x.toFixed(0)},${r.y.toFixed(0)})`;
      throw new DriverError(
        `${this.name}: no control labelled "${label}" in row #${n} of "${rowText}". ` +
          `Rows: ${rows.map(at).join(" ") || "none"}. "${label}" controls: ${candidates.map(at).join(" ") || "none"}`,
      );
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
