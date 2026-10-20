// Seeded crew members for the crew-stage journeys (FEED-04 to FEED-07).
//
// Real driver apps are heavy, so a journey runs one or two real apps for the
// viewers and seeds the rest of the crew straight into the run's Nakama over
// REST. A seeded member is a real account: device sign in, display name,
// avatar in storage, the invite-code join, a library report, game session ends
// and clips. Nothing is faked on the server.
//
// The target is the stack that the driver and its guard use (NAKAMA_HOST and
// NAKAMA_PORT, guard.ts nakamaTarget), which `run` has already checked to be
// local. Device ids carry the run id, so two runs never share an account.
// Logs print counts only: no names, no ids, no tokens.

import { nakamaBase } from "../../../tools/mello-driver/src/guard.ts";

/** The public client key of a development build (mello-core/src/config.rs). Not a secret. */
const SERVER_KEY = "mello_dev_key";
const BASIC = `Basic ${Buffer.from(`${SERVER_KEY}:`).toString("base64")}`;

/** The seed of every generated profile. Fixed, so a run repeats the same crew. */
const PROFILE_SEED = 0x6d33_6c6c;

export type SeedGame = { id: string; name: string; playtimeMin: number };

export type SeedSession = {
  game: SeedGame;
  durationMin: number;
  /** The end of the session as a minute of the local clock (0 to 1439). */
  endLocalMin: number;
  /** Whole days before now that the session ended (0 to 13). */
  daysAgo: number;
};

export type SeedProfile = {
  name: string;
  games: SeedGame[];
  sessions: SeedSession[];
  clips: number;
  /** An index into the avatar palette. */
  hue: number;
};

export type SeededMember = { name: string; userId: string; token: string };

// ── the pool ─────────────────────────────────────────────────────────

/**
 * Twelve Steam games. The ids are Steam app ids, which the client's catalogue
 * resolves to capsule art (client/assets/catalogue).
 */
export const GAME_POOL: Omit<SeedGame, "playtimeMin">[] = [
  { id: "steam-730", name: "Counter-Strike 2" },
  { id: "steam-252950", name: "Rocket League" },
  { id: "steam-291550", name: "Brawlhalla" },
  { id: "steam-1097150", name: "Fall Guys" },
  { id: "steam-570", name: "Dota 2" },
  { id: "steam-1172470", name: "Apex Legends" },
  { id: "steam-1245620", name: "Elden Ring" },
  { id: "steam-892970", name: "Valheim" },
  { id: "steam-105600", name: "Terraria" },
  { id: "steam-359550", name: "Tom Clancy's Rainbow Six Siege" },
  { id: "steam-440", name: "Team Fortress 2" },
  { id: "steam-578080", name: "PUBG: BATTLEGROUNDS" },
];

/** The game that almost every member owns, so the top shared game is contested. */
const SHARED_TOP = GAME_POOL[0];

const FIRST_NAMES = [
  "Jonas", "Max", "Sara", "Leo", "Mira", "Tobias", "Elin", "Oskar", "Nora", "Viktor",
  "Alva", "Hugo", "Ida", "Axel", "Maja", "Felix", "Wilma", "Isak", "Stella", "Noah",
  "Freja", "Elias", "Saga", "Liam", "Astrid", "Gustav", "Ebba", "Adam", "Selma", "Melvin",
  "Tuva", "Albin", "Julia", "Theo", "Klara", "Edvin", "Lova", "Arvid", "Signe", "Ruben",
  "Agnes", "Sixten", "Vera", "Ivar", "Lea", "Nils", "Olivia", "Malte", "Tilda", "Folke",
];

/** mulberry32: a small seeded generator. */
function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * `n` profiles from the fixed seed. The mix is the same for every n:
 * - almost everyone owns the shared top game, with playtimes from minutes to
 *   thousands of hours, so The Carry is contested;
 * - two night owls end their sessions between 01:00 and 04:00 local;
 * - a few members play short sessions of 3 to 9 minutes (rage_quit);
 * - two members have no sessions at all (a library and nothing else);
 * - sessions end anywhere in the last 14 days, so the daily buckets and the
 *   14 day titles (Sampler, Locked In, Has a Life, One Trick) differ;
 * - a few members post clips.
 * `runId` only names nothing today: it is accepted so a caller passes the
 * same arguments to both functions and a later change can use it.
 */
export function profiles(n: number, _runId: string): SeedProfile[] {
  const rnd = prng(PROFILE_SEED);
  const int = (lo: number, hi: number) => lo + Math.floor(rnd() * (hi - lo + 1));
  const out: SeedProfile[] = [];
  for (let i = 0; i < n; i++) {
    const name = i < FIRST_NAMES.length ? FIRST_NAMES[i] : `${FIRST_NAMES[i % FIRST_NAMES.length]}${Math.floor(i / FIRST_NAMES.length) + 1}`;
    const owned: SeedGame[] = [];
    // Playtime in minutes, log-spread: 20 min up to about 3,000 h.
    const playtime = () => Math.round(Math.exp(Math.log(20) + rnd() * (Math.log(180_000) - Math.log(20))));
    if (rnd() < 0.85) owned.push({ ...SHARED_TOP, playtimeMin: playtime() });
    const others = GAME_POOL.slice(1);
    const extra = int(2, 7);
    for (let k = 0; k < extra; k++) {
      const g = others[int(0, others.length - 1)];
      if (!owned.some((o) => o.id === g.id)) owned.push({ ...g, playtimeMin: playtime() });
    }
    if (owned.length === 0) owned.push({ ...others[0], playtimeMin: playtime() });

    const sessions: SeedSession[] = [];
    const noSessions = i === 4 || i === 11; // two members with a library and nothing else
    const nightOwl = i === 1 || i === 6;
    const raging = i % 7 === 3;
    if (!noSessions) {
      const count = int(3, 6);
      for (let k = 0; k < count; k++) {
        const game = owned[int(0, owned.length - 1)];
        const short = raging && k < 3;
        sessions.push({
          game,
          durationMin: short ? int(3, 9) : int(25, 190),
          endLocalMin: nightOwl ? int(60, 240) : int(18 * 60, 23 * 60 + 30),
          daysAgo: int(0, 13),
        });
      }
    }
    const clips = i % 6 === 2 ? int(1, 2) : 0;
    out.push({ name, games: owned, sessions, clips, hue: int(0, 4) });
  }
  return out;
}

// ── REST ─────────────────────────────────────────────────────────────

async function call<T>(method: string, path: string, opts: { token?: string; body?: unknown; basic?: boolean } = {}): Promise<T> {
  const r = await fetch(`${nakamaBase()}${path}`, {
    method,
    headers: {
      "Content-Type": "application/json",
      Authorization: opts.basic ? BASIC : `Bearer ${opts.token}`,
    },
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
    signal: AbortSignal.timeout(20_000),
  });
  const text = await r.text();
  if (!r.ok) throw new Error(`${method} ${path.split("?")[0]} failed: HTTP ${r.status} ${text.slice(0, 200)}`);
  return (text ? JSON.parse(text) : {}) as T;
}

/** An RPC takes its payload as one JSON string. */
async function rpc<T = Record<string, unknown>>(token: string, id: string, payload: unknown): Promise<T> {
  const r = await call<{ payload?: string }>("POST", `/v2/rpc/${id}`, { token, body: JSON.stringify(payload) });
  return (r.payload ? JSON.parse(r.payload) : {}) as T;
}

/** Sign in, or create, the device account. Returns the session token. */
async function deviceToken(deviceId: string): Promise<string> {
  const r = await call<{ token: string }>("POST", "/v2/account/authenticate/device?create=true", { basic: true, body: { id: deviceId } });
  return r.token;
}

/** The session of an account that a journey made with email and password (onboarding step 3). */
export async function signInEmail(email: string, password: string): Promise<string> {
  const r = await call<{ token: string }>("POST", "/v2/account/authenticate/email?create=false", { basic: true, body: { email, password } });
  return r.token;
}

const PALETTE = ["#6E8FB8", "#8E7FB0", "#5FA396", "#B39158", "#B07793"];

/** A small generated avatar: a hue, a bright block and a dark one at places taken from the name. */
export function avatarSvg(name: string, hue: number): string {
  let h = 2166136261;
  for (const c of name) h = Math.imul(h ^ c.charCodeAt(0), 16777619) >>> 0;
  const r = prng(h);
  const bg = PALETTE[hue % PALETTE.length];
  const px = (v: number) => Math.round(v * 40) + 8;
  return (
    `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">` +
    `<rect width="64" height="64" fill="${bg}"/>` +
    `<rect x="${px(r())}" y="${px(r())}" width="22" height="22" fill="#F2F2F4"/>` +
    `<rect x="${px(r())}" y="${px(r())}" width="18" height="18" fill="#1B1B1F"/>` +
    `<circle cx="${px(r())}" cy="${px(r())}" r="9" fill="#26262B"/>` +
    `</svg>`
  );
}

/** The UTC offset that puts the local clock at `localMin` right now (-720..719). */
function tzForLocalMinute(localMin: number, nowMs = Date.now()): number {
  const utcMin = Math.floor(nowMs / 60_000) % 1440;
  return ((((localMin - utcMin + 720) % 1440) + 1440) % 1440) - 720;
}

export type LibraryEntry = { id: string; name: string; playtimeMin: number };

/** `library_report`: replaces the library of the account that owns `token`. */
export async function reportLibrary(token: string, games: LibraryEntry[]): Promise<void> {
  await rpc(token, "library_report", {
    games: games.map((g) => ({ game_id: g.id, name: g.name, launcher: "steam", playtime_min: g.playtimeMin })),
  });
}

/** Join a crew by invite code. Returns the crew id. */
export async function joinByCode(token: string, code: string): Promise<string> {
  const r = await rpc<{ crew_id: string }>(token, "join_by_invite_code", { code });
  return r.crew_id;
}

/**
 * `game_session_end`: the member ended a session `daysAgo` days ago, at
 * `endLocalMin` (minute of the local day) on their clock. `start_ms` and
 * `end_ms` go along as the real client sends them, and the server stamps the
 * session with them, as it does for a session that waited in the client's
 * pending queue.
 */
export async function endSession(
  token: string,
  crewId: string,
  game: { id: string; name: string },
  durationMin: number,
  endLocalMin: number,
  sessionId = `seed-${Math.random().toString(36).slice(2)}`,
  daysAgo = 0,
): Promise<void> {
  const endMs = Date.now() - daysAgo * 86_400_000 - 1000;
  await rpc(token, "game_session_end", {
    crew_id: crewId,
    game_name: game.name,
    game_id: game.id,
    duration_min: durationMin,
    active_min: durationMin,
    session_id: sessionId,
    start_ms: endMs - durationMin * 60_000,
    end_ms: endMs,
    tz_offset_min: tzForLocalMinute(endLocalMin, endMs),
  });
}

/**
 * Create the account of one crew member and fill it from `profile`: display
 * name, avatar at `avatars/current`, the library report, the invite-code join,
 * then sessions and clips. A session end can be skipped by the 60 s debounce.
 * `seedCrew` ends with one library report that recomputes the crew once.
 *
 * Each session carries the `start_ms` and `end_ms` of its day in the last 14,
 * and the server stamps it with them. The local clock comes through
 * `tz_offset_min`.
 */
export async function seedMember(crewInviteCode: string, name: string, profile: SeedProfile, runId: string): Promise<SeededMember> {
  const token = await deviceToken(`seed-${runId}-${name}`.toLowerCase());
  const me = await call<{ user: { id: string } }>("GET", "/v2/account", { token });
  const userId = me.user.id;
  await call("PUT", "/v2/account", { token, body: { display_name: name, avatar_url: `/v2/storage/avatars/current/${userId}` } });
  await call("PUT", "/v2/storage", {
    token,
    body: {
      objects: [
        {
          collection: "avatars",
          key: "current",
          value: JSON.stringify({ format: "svg", data: avatarSvg(name, profile.hue) }),
          permission_read: 2,
          permission_write: 1,
        },
      ],
    },
  });
  // The library goes before the join, as a real sensor reports before an
  // invite is used. A report recomputes the crews of the user, and the user
  // has none yet, so a crew that fills up is not recomputed once for each
  // member (and does not move a belt for each of them).
  await reportLibrary(token, profile.games);
  const crewId = await joinByCode(token, crewInviteCode);

  let seq = 0;
  for (const s of profile.sessions) {
    await endSession(token, crewId, s.game, s.durationMin, s.endLocalMin, `seed-${runId}-${name}-${seq++}`.toLowerCase(), s.daysAgo);
  }
  for (let c = 0; c < profile.clips; c++) {
    await rpc(token, "post_clip", {
      crew_id: crewId,
      clip_id: `seed-${runId}-${name}-clip-${c}`.toLowerCase(),
      clip_type: "voice",
      duration_seconds: 12 + c * 5,
      game: profile.games[0]?.name ?? "",
    });
  }
  return { name, userId, token };
}

/** Seed `profiles` into the crew one after the other. Logs counts only. */
export async function seedCrew(crewInviteCode: string, list: SeedProfile[], runId: string): Promise<SeededMember[]> {
  const t0 = Date.now();
  const out: SeededMember[] = [];
  for (const p of list) out.push(await seedMember(crewInviteCode, p.name, p, runId));
  // A library report is never debounced: it recomputes the holders doc with
  // every member in, as one change. Titles that turn contested here have not
  // changed hands, so no belt moved.
  if (out.length > 0) await reportLibrary(out[out.length - 1].token, list[list.length - 1].games);
  const sessions = list.reduce((n, p) => n + p.sessions.length, 0);
  const clips = list.reduce((n, p) => n + p.clips, 0);
  const games = list.reduce((n, p) => n + p.games.length, 0);
  console.log(`seeded ${out.length} members, ${games} library games, ${sessions} sessions, ${clips} clips in ${Date.now() - t0} ms`);
  return out;
}

/** The `crew_feed` RPC as one member sees it. For timing and for checks. */
export async function crewFeed(token: string, crewId: string): Promise<{ ms: number; feed: any }> {
  const t0 = Date.now();
  const feed = await rpc<any>(token, "crew_feed", { crew_id: crewId });
  return { ms: Date.now() - t0, feed };
}

/**
 * Add a voice channel as the crew owner. The server pushes `channel_created`
 * to every member who is online, and an open app loads the crew feed again on
 * it. Session ends and library reports push nothing, so a journey whose
 * viewers stay open uses this after seeding, as a crew mate would.
 */
export async function addChannel(token: string, crewId: string, name: string): Promise<void> {
  await rpc(token, "channel_create", { crew_id: crewId, name });
}
