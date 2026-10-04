// Voice channel steps shared by the voice-channel journeys.

import type { App } from "../../../tools/mello-driver/src/app.ts";

/**
 * Add a voice channel the way a crew admin does: Crew menu → Add channel
 * opens crew settings on the channels tab, then type the name and press
 * "Add channel". Waits until the channel is in this user's list and the
 * modal is closed again.
 */
export async function addVoiceChannel(app: App, name: string): Promise<void> {
  await app.click("Crew menu");
  // A crew menu item is in a PopupWindow: see App.activate. A crew data
  // update changes the crew list in place, so the menu stays open (#105).
  await app.activate("Add channel");
  await app.waitFor("crew settings open", (s) => s.open_modals.includes("crew_settings"), 5_000);
  await app.type("New channel", name);
  await app.click("Add channel");
  await app.waitFor(`${name} in ${app.name}'s channel list`, (s) => s.voice_channels.some((c) => c.name === name));
  await app.click("Close");
  await app.waitFor("crew settings closed", (s) => !s.open_modals.includes("crew_settings"), 5_000);
}

/** Wait until this user lists every one of these voice channels. */
export async function waitForChannels(app: App, names: string[]): Promise<void> {
  await app.waitFor(`${names.join(", ")} in ${app.name}'s channel list`, (s) =>
    names.every((n) => s.voice_channels.some((c) => c.name === n)),
  );
}
