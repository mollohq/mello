// Arguments of `cli.ts run`.

export type RunArgs = { files: string[]; repeat: number; events?: string; liveScreenshotMs?: number };

/** Parse the arguments of `run`. Returns an error message for a bad option. */
export function parseRunArgs(args: string[]): RunArgs | string {
  const out: RunArgs = { files: [], repeat: 1 };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (!a.startsWith("--")) {
      out.files.push(a);
      continue;
    }
    if (a !== "--repeat" && a !== "--events" && a !== "--live-screenshot-ms") return `unknown option ${a}`;
    const v = args[++i];
    if (v === undefined || v.startsWith("--")) return `${a} needs a value`;
    if (a === "--events") out.events = v;
    else {
      const n = Number(v);
      if (!Number.isInteger(n) || n < (a === "--repeat" ? 1 : 0)) return `${a} needs a whole number, got "${v}"`;
      if (a === "--repeat") out.repeat = n;
      else out.liveScreenshotMs = n;
    }
  }
  if (out.files.length === 0) return "no journey given";
  return out;
}
