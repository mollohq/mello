// Shared driver configuration: where the binary, the repo and the artifacts are,
// and the preflight check that the binary and the local stack exist.

import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import type { RunOptions } from "./journey.ts";

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

/** Fail early, with the fix, when the binary or the stack is missing. */
export async function preflight(opts: RunOptions): Promise<void> {
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
}
