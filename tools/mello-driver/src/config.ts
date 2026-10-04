// Shared driver configuration: where the binary, the repo and the artifacts are,
// and the preflight check that the binary and the local stack exist.

import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type { Journey, RunOptions } from "./journey.ts";

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
 * falls back to P2P, and a voice journey tests a path that is being removed.
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
 * Fail early, with the fix, when the binary or the stack is missing. When one
 * of `journeys` uses voice, the local SFU must answer too.
 */
export async function preflight(opts: RunOptions, journeys: Journey[] = []): Promise<void> {
  if (!existsSync(opts.binary)) {
    throw new Error(
      `no app binary at ${opts.binary}. Build it:\n` +
        "  SLINT_EMIT_DEBUG_INFO=1 cargo build -p mello-client --no-default-features --features development,e2e",
    );
  }
  const host = process.env.NAKAMA_HOST ?? "127.0.0.1";
  const port = process.env.NAKAMA_PORT ?? "7350";
  try {
    const r = await fetch(`http://${host}:${port}/healthcheck`);
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
  } catch (e) {
    throw new Error(
      `no Nakama at ${host}:${port} (${e}). Start the local stack:\n  scripts/e2e.sh --keep`,
    );
  }
  if (journeys.some((j) => j.voice)) await checkSfu();
}
