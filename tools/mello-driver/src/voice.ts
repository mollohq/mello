// Voice journeys run only through the SFU (plans/E2E-QA.md §16.6). P2P is
// being removed as a feature. A call that fell back to P2P tests a path that
// is going away, so it fails the journey.

import { setTimeout as sleep } from "node:timers/promises";

import { DriverError, type App, type AppEvent } from "./app.ts";
import { sfuHealthUrl } from "./config.ts";

export type VoiceTransport = NonNullable<AppEvent["transport"]>;

/**
 * The transport of the last voice join in an event tail, or null while that
 * join runs.
 *
 * The core reports a join with VoiceJoined. It then starts the media and
 * reports the transport that started with VoiceStateChanged: "sfu", "p2p"
 * after an SFU join failed, or "disconnected". A new join cancels the join
 * that runs, so no VoiceStateChanged of an earlier join follows a later
 * VoiceJoined. The answer is the first VoiceStateChanged after the last
 * VoiceJoined.
 */
export function lastJoinTransport(events: AppEvent[]): VoiceTransport | null {
  const joined = events.findLastIndex((e) => e.type === "VoiceJoined");
  if (joined < 0) return null;
  const started = events.slice(joined + 1).find((e) => e.type === "VoiceStateChanged");
  return started?.transport ?? null;
}

/**
 * Wait until this user's last voice join has started its call, then fail
 * unless the call runs through the SFU. Call it after every voice join.
 */
export async function expectSfuVoice(app: App, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let transport: VoiceTransport | null = null;
  for (;;) {
    transport = lastJoinTransport(await app.events());
    if (transport || Date.now() > deadline) break;
    await sleep(150);
  }
  if (transport === "sfu") {
    // The state port must agree: the call is still on the SFU.
    await app.waitFor("the state port reports the SFU transport", (s) => s.voice_transport === "sfu", 2_000);
    return;
  }
  if (transport === "p2p") {
    throw new DriverError(
      `${app.name}: the voice call runs over P2P, not through the SFU. The SFU join failed and the app ` +
        `fell back to P2P (see "falling back to P2P" in ${app.logPath}). ` +
        `Voice journeys run only through the local SFU (${sfuHealthUrl()}).`,
    );
  }
  if (transport === "disconnected") {
    throw new DriverError(`${app.name}: the voice call did not start (VoiceStateChanged reports no transport); see ${app.logPath}`);
  }
  throw new DriverError(
    `${app.name}: timed out after ${timeoutMs} ms waiting for the transport of the last voice join ` +
      "(no VoiceStateChanged after the last VoiceJoined on the state port)",
  );
}
