// `cli.ts list`: find the journeys in qa/journeys without running them
// (plans/E2E-QA.md §16). A journey module only defines journeys when it is
// imported. It must not launch an app or open a port at import time.

import { readdirSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";

export type JourneyEntry = {
  id: string;
  /** Relative to the repo root, with forward slashes on every platform. */
  file: string;
  /** The export name, or "default" when no named export is the same journey. */
  export: string;
  /** What `cli.ts run` takes, from the repo root. */
  selector: string;
  flows: string[];
  knownIssues: number[];
};

/** The same test that `run` uses to find a journey in a module. */
export function isJourney(v: any): boolean {
  return Boolean(v) && typeof v.id === "string" && typeof v.run === "function";
}

/** Every `qa/journeys/*.ts`, sorted. `qa/journeys/lib/` is not a journey folder. */
export function defaultJourneyFiles(root: string, dir = "qa/journeys"): string[] {
  return readdirSync(resolve(root, dir), { withFileTypes: true })
    .filter((e) => e.isFile() && e.name.endsWith(".ts"))
    .map((e) => resolve(root, dir, e.name))
    .sort(byCode);
}

/**
 * List the journeys in `files` (paths from the current directory, or absolute).
 * One entry per distinct journey object. A `default` export that is also a
 * named export is listed once, under the named export.
 */
export async function listJourneys(files: string[], root: string): Promise<JourneyEntry[]> {
  const entries: JourneyEntry[] = [];
  for (const arg of files) {
    const abs = resolve(arg);
    const rel = relative(root, abs);
    if (rel === "" || rel.startsWith("..") || isAbsolute(rel)) {
      throw new Error(`${arg}: not inside the repo (${root})`);
    }
    const file = rel.split(sep).join("/");

    let mod: Record<string, any>;
    try {
      mod = await import(pathToFileURL(abs).href);
    } catch (e) {
      throw new Error(`${file}: cannot import the module: ${e instanceof Error ? e.message : e}`);
    }

    // Named exports first, in name order, then `default`. The first name that
    // reaches a journey object is the one that is listed.
    const names = Object.keys(mod)
      .filter((k) => k !== "default")
      .sort(byCode);
    if ("default" in mod) names.push("default");
    const seen = new Set<unknown>();
    const found: JourneyEntry[] = [];
    for (const name of names) {
      const j = mod[name];
      if (!isJourney(j) || seen.has(j)) continue;
      seen.add(j);
      found.push({
        id: j.id,
        file,
        export: name,
        selector: `${file}#${name}`,
        flows: Array.isArray(j.flows) ? [...j.flows] : [],
        knownIssues: Array.isArray(j.knownIssues) ? [...j.knownIssues] : [],
      });
    }
    if (found.length === 0) console.error(`warning: ${file}: no journey export`);
    entries.push(...found);
  }
  return entries.sort((a, b) => byCode(a.file, b.file) || byCode(a.export, b.export));
}

/** A short table for people: id, selector, flows. */
export function formatTable(entries: JourneyEntry[]): string {
  const rows = [["ID", "SELECTOR", "FLOWS"], ...entries.map((e) => [e.id, e.selector, e.flows.join(", ")])];
  const width = [0, 1].map((c) => Math.max(...rows.map((r) => r[c].length)));
  return rows.map((r) => `${r[0].padEnd(width[0])}  ${r[1].padEnd(width[1])}  ${r[2]}`.trimEnd()).join("\n");
}

function byCode(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}
