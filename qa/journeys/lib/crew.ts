// Crew steps shared by the crew journeys: the crew menu and crew settings.

import type { App } from "../../../tools/mello-driver/src/app.ts";

/** The items of the crew menu (crew_panel.slint). "Crew settings" and "Add channel" show only for an owner or an admin. */
export type CrewMenuItem = "Crew settings" | "Add channel" | "Invite" | "Leave crew";

/** The tabs of crew settings (crew_settings_modal.slint). */
export type CrewSettingsTab = "OVERVIEW" | "CHANNELS" | "MEMBERS";

/**
 * Open the crew menu of the active crew and choose an item. The menu is a
 * PopupWindow: its items take `activate`, not a pointer click (see
 * App.activate). The item is chosen at once after the menu opens.
 */
export async function crewMenu(app: App, item: CrewMenuItem): Promise<void> {
  await app.click("Crew menu");
  await app.activate(item);
}

/**
 * Open crew settings from the crew menu, on a tab. Waits until the modal is
 * open. The modal keeps the last tab, so the tab is always clicked.
 */
export async function openCrewSettings(app: App, tab: CrewSettingsTab = "OVERVIEW"): Promise<void> {
  await crewMenu(app, "Crew settings");
  await app.waitFor("crew settings open", (s) => s.open_modals.includes("crew_settings"), 5_000);
  await app.click(tab);
}

/** Close crew settings with its "Close" button. Waits until the modal is closed. */
export async function closeCrewSettings(app: App): Promise<void> {
  await app.click("Close");
  await app.waitFor("crew settings closed", (s) => !s.open_modals.includes("crew_settings"), 5_000);
}

/** The value of a text field by its label, as `controls()` reads it. */
export async function fieldValue(app: App, label: string): Promise<string> {
  return (await app.find(label)).value;
}
