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

export type SeedCrewArgs = { code: string; members: number; belts: boolean; clips?: number; recap: boolean };

/** Parse the arguments of `seed-crew`. Returns an error message for a bad option. */
export function parseSeedCrewArgs(args: string[]): SeedCrewArgs | string {
  const out: SeedCrewArgs = { code: "", members: 3, belts: false, recap: false };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "--belts") out.belts = true;
    else if (a === "--recap") out.recap = true;
    else if (a === "--members" || a === "--clips") {
      const v = args[++i];
      if (v === undefined || v.startsWith("--")) return `${a} needs a value`;
      const n = Number(v);
      if (!Number.isInteger(n) || n < (a === "--members" ? 1 : 0) || n > 200) {
        return `${a} needs a whole number from ${a === "--members" ? 1 : 0} to 200, got "${v}"`;
      }
      if (a === "--members") out.members = n;
      else out.clips = n;
    } else if (a.startsWith("--")) return `unknown option ${a}`;
    else if (out.code) return `more than one invite code given: ${out.code}, ${a}`;
    else out.code = a;
  }
  if (!out.code) return "no invite code given";
  return out;
}
