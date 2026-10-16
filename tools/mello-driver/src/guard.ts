// Production guards (plans/E2E-QA.md §16.8). A journey must never reach the
// production backend, SFU or lounge. The local stack falls back to production
// when its configuration is missing: `SFU_ENDPOINT_EU` defaults to
// wss://sfu-eu.m3llo.app/ws in backend/docker-compose.yml, Nakama has
// built-in production endpoints for each SFU region, and the lounge's
// NAKAMA_BASE_URL defaults to production in mello-site's wrangler.toml.
//
// The guards do not read any configuration file. They ask the running stack
// what it hands out, and check that each host resolves only to loopback or
// private LAN addresses.

import { lookup } from "node:dns/promises";
import { isIP } from "node:net";

/** A guard refused the run. The message names the target and the fix. */
export class ProductionGuardError extends Error {}

/** Resolve a host name to all its addresses. Tests replace it. */
export type Lookup = (host: string) => Promise<string[]>;

export const dnsLookup: Lookup = async (host) => {
  if (isIP(host)) return [host];
  return (await lookup(host, { all: true })).map((a) => a.address);
};

/** True for a loopback, private (RFC 1918, IPv6 ULA) or link-local address. */
export function isLocalAddress(ip: string): boolean {
  const v4 = ip.startsWith("::ffff:") ? ip.slice(7) : ip;
  if (isIP(v4) === 4) {
    const [a, b] = v4.split(".").map(Number);
    return a === 127 || a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254);
  }
  if (isIP(ip) === 6) {
    const first = parseInt(ip.split(":")[0] || "0", 16);
    return ip === "::1" || (first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80;
  }
  return false;
}

/**
 * Where a host leads: "local" when every address is local, "unresolved"
 * when the name has no address (the app cannot reach it either), or the
 * addresses that are not local.
 */
export type HostClass = { kind: "local" } | { kind: "unresolved" } | { kind: "remote"; addresses: string[] };

export async function classifyHost(host: string, resolve: Lookup = dnsLookup): Promise<HostClass> {
  const bare = host.replace(/^\[(.*)\]$/, "$1");
  let addresses: string[];
  try {
    addresses = isIP(bare) ? [bare] : await resolve(bare);
  } catch (e) {
    const code = (e as { code?: string }).code;
    if (code === "ENOTFOUND" || code === "ENODATA") return { kind: "unresolved" };
    throw new ProductionGuardError(`cannot resolve ${bare} (${code ?? e}), so the guard cannot check it`);
  }
  if (addresses.length === 0) return { kind: "unresolved" };
  const remote = addresses.filter((a) => !isLocalAddress(a));
  return remote.length ? { kind: "remote", addresses: remote } : { kind: "local" };
}

/** Refuse unless `host` resolves only to local addresses. */
export async function requireLocalHost(what: string, host: string, fix: string, resolve: Lookup = dnsLookup): Promise<void> {
  const c = await classifyHost(host, resolve);
  if (c.kind === "local") return;
  const why = c.kind === "remote" ? `resolves to ${c.addresses.join(", ")}` : "does not resolve";
  throw new ProductionGuardError(
    `refusing to run: ${what} is ${host}, which ${why}. Journeys use only loopback or private LAN addresses. ${fix}`,
  );
}

/** The Nakama of the local stack, as the driver and every app it starts use it. */
export type NakamaTarget = { host: string; port: string };

export function nakamaTarget(): NakamaTarget {
  return { host: process.env.NAKAMA_HOST ?? "127.0.0.1", port: process.env.NAKAMA_PORT ?? "7350" };
}

export function nakamaBase(t: NakamaTarget = nakamaTarget()): string {
  return `http://${t.host.includes(":") && !t.host.startsWith("[") ? `[${t.host}]` : t.host}:${t.port}`;
}

/** The public client key of a development build (mello-core/src/config.rs). Not a secret. */
const DEV_SERVER_KEY = "mello_dev_key";

/** The device of the guard's own test user on the local Nakama. */
const GUARD_DEVICE = "mello-e2e-production-guard";

/** A session of the guard's test user. The local stack creates it on first use. */
export async function guardSession(base = nakamaBase()): Promise<string> {
  const r = await fetch(`${base}/v2/account/authenticate/device?create=true`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Basic ${Buffer.from(`${DEV_SERVER_KEY}:`).toString("base64")}` },
    body: JSON.stringify({ id: GUARD_DEVICE }),
    signal: AbortSignal.timeout(5_000),
  });
  const body = (await r.json().catch(() => null)) as { token?: string } | null;
  if (!r.ok || !body?.token) throw new ProductionGuardError(`refusing to run: the guard could not sign in to Nakama at ${base} (HTTP ${r.status})`);
  return body.token;
}

/** Call a Nakama RPC as the guard's test user and parse its answer. */
export async function guardRpc<T>(base: string, token: string, name: string, payload: unknown): Promise<T> {
  const r = await fetch(`${base}/v2/rpc/${name}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` },
    body: JSON.stringify(JSON.stringify(payload)),
    signal: AbortSignal.timeout(5_000),
  });
  const body = (await r.json().catch(() => null)) as { payload?: string } | null;
  if (!r.ok || typeof body?.payload !== "string") {
    throw new ProductionGuardError(`refusing to run: the guard's call to ${name} on ${base} failed (HTTP ${r.status})`);
  }
  return JSON.parse(body.payload) as T;
}

/**
 * Check every SFU region that the local Nakama hands out. The app measures
 * each region's health address at login and joins the fastest, so a region
 * on a production host can carry a journey's voice. A region whose host does
 * not resolve cannot be reached: it gives a warning, not a refusal.
 * Returns the warnings.
 */
export async function checkSfuRegions(base = nakamaBase(), resolve: Lookup = dnsLookup): Promise<string[]> {
  const token = await guardSession(base);
  const { regions } = await guardRpc<{ regions?: { region: string; health_url: string }[] }>(base, token, "get_sfu_regions", {});
  if (!regions?.length) throw new ProductionGuardError(`refusing to run: Nakama at ${base} lists no SFU region, so the guard cannot check them`);
  const warnings: string[] = [];
  for (const r of regions) {
    const host = new URL(r.health_url).hostname;
    const c = await classifyHost(host, resolve);
    if (c.kind === "remote") {
      throw new ProductionGuardError(
        `refusing to run: Nakama at ${base} hands out the SFU region "${r.region}" on ${host} (${c.addresses.join(", ")}). ` +
          "The app measures every region and can join it. Point every region of the local stack at the local SFU " +
          "(SFU_ENDPOINT_EU and SFU_ENDPOINT_US for mello-nakama).",
      );
    }
    if (c.kind === "unresolved") {
      warnings.push(`the SFU region "${r.region}" is on ${host}, which does not resolve: the app cannot reach it`);
    }
  }
  return warnings;
}

/** The boot data that the lounge's invite page hands to the browser (mello-site functions/join/[code].ts). */
export type LoungeBoot = { voice?: { url?: string } | null };

/** Read the `lounge-boot` JSON of an invite page. Null when the page has none. */
export function loungeBoot(html: string): LoungeBoot | null {
  const m = html.match(/<script type="application\/json" id="lounge-boot">([\s\S]*?)<\/script>/);
  if (!m) return null;
  try {
    return JSON.parse(m[1]) as LoungeBoot;
  } catch {
    return null;
  }
}

/** An invite code of a crew of the guard's test user. Makes the crew on first use. */
export async function guardInviteCode(base = nakamaBase()): Promise<string> {
  const token = await guardSession(base);
  const auth = { headers: { Authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(5_000) };
  const account = (await (await fetch(`${base}/v2/account`, auth)).json()) as { user?: { id?: string } };
  const userId = account.user?.id;
  if (!userId) throw new ProductionGuardError(`refusing to run: the guard could not read its account on ${base}`);
  const groups = (await (await fetch(`${base}/v2/user/${userId}/group?limit=1`, auth)).json()) as {
    user_groups?: { group?: { id?: string } }[];
  };
  let crewId = groups.user_groups?.[0]?.group?.id;
  if (!crewId) {
    const made = await guardRpc<{ crew_id: string; invite_code?: string }>(base, token, "create_crew", {
      name: "e2e guard",
      description: "The e2e driver checks the lounge with this crew.",
      invite_only: true,
    });
    if (made.invite_code) return made.invite_code;
    crewId = made.crew_id;
  }
  return (await guardRpc<{ code: string }>(base, token, "create_invite_code", { crew_id: crewId })).code;
}

/**
 * Check the local lounge. Its address must be local. It must resolve an
 * invite of the local Nakama: a lounge on the production Nakama does not know
 * the code and answers 404. The Nakama address that it gives the browser for
 * guest voice must be local too.
 */
export async function checkLounge(lounge: string, base = nakamaBase(), resolve: Lookup = dnsLookup): Promise<void> {
  const fix = "Run `npm run dev` in mello-site with NAKAMA_BASE_URL on the local Nakama, or set MELLO_E2E_LOUNGE_URL.";
  await requireLocalHost("the lounge (MELLO_E2E_LOUNGE_URL)", new URL(lounge).hostname, fix, resolve);
  const code = await guardInviteCode(base);
  let r: Response;
  try {
    r = await fetch(`${lounge}/join/${encodeURIComponent(code)}`, { signal: AbortSignal.timeout(10_000) });
  } catch (e) {
    throw new ProductionGuardError(`no lounge at ${lounge} (${e}). ${fix}`);
  }
  if (r.status === 404) {
    throw new ProductionGuardError(
      `refusing to run: the lounge at ${lounge} does not know the invite ${code} of the local Nakama at ${base}. ` +
        `It reads another Nakama (mello-site's wrangler.toml defaults NAKAMA_BASE_URL to production). ${fix}`,
    );
  }
  if (!r.ok) throw new ProductionGuardError(`the lounge at ${lounge} answered HTTP ${r.status} for an invite page. ${fix}`);
  const boot = loungeBoot(await r.text());
  if (!boot) throw new ProductionGuardError(`the lounge at ${lounge} sent an invite page with no lounge-boot data. ${fix}`);
  const voice = boot.voice?.url;
  if (voice) {
    await requireLocalHost(`the Nakama address that the lounge gives guests for voice`, new URL(voice).hostname, fix, resolve);
  }
}
