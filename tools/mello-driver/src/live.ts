// Live screenshots for `run --events`: every N ms, take a screenshot of each
// running user's app and report it. A screenshot never changes the journey:
// the target skips a tick while a journey action is in flight (App.liveShot),
// and this timer never starts a second screenshot for a user whose previous
// one has not finished.

import { join } from "node:path";

export type ShotTarget = {
  name: string;
  /** The app is up and can answer. */
  live: boolean;
  /** Write the screenshot to `file`. False when it was skipped. */
  liveShot(file: string): Promise<boolean>;
};

export type LiveShotsOptions = {
  targets: () => Iterable<ShotTarget>;
  /** The artifacts folder of the journey; files go to `<dir>/live/<user>.png`. */
  dir: string;
  intervalMs: number;
  /** Called after the file is complete. */
  onShot: (user: string, path: string) => void;
  /** Called for a failed screenshot, once per run of failures of one user. */
  onError: (user: string, error: unknown) => void;
};

export function startLiveShots(o: LiveShotsOptions): { stop(): Promise<void> } {
  const pending = new Map<string, Promise<void>>();
  const failing = new Set<string>();
  let stopped = false;

  const tick = () => {
    if (stopped) return;
    for (const t of o.targets()) {
      if (!t.live || pending.has(t.name)) continue;
      const path = join(o.dir, "live", `${t.name}.png`);
      const job = t
        .liveShot(path)
        .then((shot) => {
          failing.delete(t.name);
          if (shot && !stopped) o.onShot(t.name, path);
        })
        .catch((e) => {
          if (stopped || failing.has(t.name)) return;
          failing.add(t.name);
          o.onError(t.name, e);
        })
        .finally(() => pending.delete(t.name));
      pending.set(t.name, job);
    }
  };

  const timer = setInterval(tick, o.intervalMs);
  return {
    async stop() {
      stopped = true;
      clearInterval(timer);
      await Promise.all(pending.values());
    },
  };
}
