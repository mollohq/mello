// Live progress of a journey run (`cli.ts run --events <file>`, plans/E2E-QA.md §16).
//
// The writer appends newline-delimited JSON to a file. Every line is one
// object with `type`, `ts` (ISO 8601 UTC) and `t` (ms since the journey
// started). Each line goes to the file in one write call, so a reader that
// tails the file sees whole lines, and sees each line at once.

import { closeSync, mkdirSync, openSync, writeSync } from "node:fs";
import { dirname } from "node:path";

export type EventFields = Record<string, unknown>;

export class EventWriter {
  readonly file: string;
  private readonly now: () => number;
  private fd: number | null;
  private t0: number;
  private failed = false;

  /** `now` is a seam for tests. */
  constructor(file: string, now: () => number = Date.now) {
    this.file = file;
    this.now = now;
    mkdirSync(dirname(file), { recursive: true });
    this.fd = openSync(file, "a");
    this.t0 = now();
  }

  /** Start the clock for the next journey: `t` counts from here. */
  beginJourney(): void {
    this.t0 = this.now();
  }

  /**
   * Append one event. A write error does not fail the journey: it goes to
   * stderr once, and the events stop.
   */
  emit(type: string, fields: EventFields = {}): void {
    if (this.fd === null || this.failed) return;
    const at = this.now();
    const line = JSON.stringify({ type, ts: new Date(at).toISOString(), t: at - this.t0, ...fields }) + "\n";
    try {
      writeSync(this.fd, line);
    } catch (e) {
      this.failed = true;
      console.error(`events: cannot write to ${this.file}: ${e instanceof Error ? e.message : e}`);
    }
  }

  close(): void {
    if (this.fd === null) return;
    closeSync(this.fd);
    this.fd = null;
  }
}
