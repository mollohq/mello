// Shared driver configuration: where the binary, the repo and the artifacts are,
// and the preflight check that the binary and the local stack exist.

import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { LOUNGE } from "./browser.ts";
import { checkLounge, checkSfuRegions, dnsLookup, nakamaBase, nakamaTarget, requireLocalHost, type Lookup } from "./guard.ts";
import { journeyNeeds, type Journey, type RunOptions } from "./journey.ts";

export const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

export function defaultOptions(): RunOptions {
  const exe = process.platform === "win32" ? "mello.exe" : "mello";
  return {
    binary: resolve(repoRoot, process.env.MELLO_BIN ?? `target/debug/${exe}`),
    repoRoot,
    artifactsRoot: resolve(repoRoot, process.env.MELLO_E2E_ARTIFACTS ?? "target/e2e"),
    mcpPortBase: Number(process.env.MELLO_E2E_PORT_BASE ?? 9401),
  };
}

/** The health address of the local SFU. MELLO_E2E_SFU_HEALTH overrides it. */
export function sfuHealthUrl(): string {
  return process.env.MELLO_E2E_SFU_HEALTH ?? "http://127.0.0.1:8080/health";
}

/**
 * Fail early when the local SFU does not answer. Without it, every voice join
 * fails: the SFU is the only voice transport.
 * The SFU is healthy when its health address answers JSON with status "ok".
 */
export async function checkSfu(url = sfuHealthUrl()): Promise<void> {
  let why: string;
  try {
    const r = await fetch(url, { signal: AbortSignal.timeout(3_000) });
    const body = (await r.json().catch(() => null)) as { status?: unknown } | null;
    if (r.ok && body?.status === "ok") return;
    why = r.ok ? `status ${JSON.stringify(body?.status)}, not "ok"` : `HTTP ${r.status}`;
  } catch (e) {
    why = String(e);
  }
  throw new Error(
    `no local SFU at ${url} (${why}). Voice journeys run only through the local SFU. ` +
      "Start the local SFU, or set MELLO_E2E_SFU_HEALTH to its health address.",
  );
}

/**
 * Fail early, with the fix, when the binary or the stack is missing, or when
 * an app would reach production (guard.ts). When one of `journeys` uses
 * voice, the local SFU must answer too. Each need of a journey is checked
 * once. Warnings go to stderr.
 */
export async function preflight(opts: RunOptions, journeys: Journey[] = [], resolve: Lookup = dnsLookup): Promise<void> {
  if (!existsSync(opts.binary)) {
    throw new Error(
      `no app binary at ${opts.binary}. Build it:\n` +
        "  SLINT_EMIT_DEBUG_INFO=1 cargo build -p mello-client --no-default-features --features development,e2e",
    );
  }
  const target = nakamaTarget();
  await requireLocalHost(
    "the Nakama of the run (NAKAMA_HOST)",
    target.host,
    "Start the local stack and unset NAKAMA_HOST, or set it to the local Nakama.",
    resolve,
  );
  const base = nakamaBase(target);
  try {
    const r = await fetch(`${base}/healthcheck`);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
  } catch (e) {
    throw new Error(`no Nakama at ${target.host}:${target.port} (${e}). Start the local stack:\n  scripts/e2e.sh --keep`);
  }
  for (const w of await checkSfuRegions(base, resolve)) console.error(`! ${w}`);
  const needs = new Set(journeys.flatMap(journeyNeeds));
  if (needs.has("sfu")) {
    const health = sfuHealthUrl();
    await requireLocalHost("the SFU health address (MELLO_E2E_SFU_HEALTH)", new URL(health).hostname, "Use the local SFU.", resolve);
    await checkSfu(health);
  }
  if (needs.has("lounge")) await checkLounge(LOUNGE, base, resolve);
}
