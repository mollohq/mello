// Voice steps shared by the voice journeys. A journey that uses them sets
// `voice: true`, so the runner checks the local SFU before it starts.

import type { App } from "../../../tools/mello-driver/src/app.ts";
import { expectSfuVoice } from "../../../tools/mello-driver/src/voice.ts";

/**
 * Join a voice channel the way a user does: click it. Waits until the user
 * is in voice and the call runs through the SFU. A P2P call fails here.
 */
export async function joinVoice(app: App, channel: string): Promise<void> {
  await app.click(channel);
  await app.waitFor(`${app.name} is in voice`, (s) => s.in_voice, 20_000);
  await expectSfuVoice(app);
}
