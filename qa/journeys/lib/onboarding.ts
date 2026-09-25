// Reusable onboarding steps. Labels are the text the user reads
// (client/src/a11y_lint.rs keeps them on every control).

import type { App } from "../../../tools/mello-driver/src/app.ts";

export type Visibility = "Private" | "Public";

/** Step 1: create a new crew with this name, which moves to step 2. Private is the app's default. */
export async function createCrewAtStep1(app: App, crewName: string, visibility: Visibility = "Private"): Promise<void> {
  await app.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
  await app.click("Create your own crew");
  await app.type("CREW NAME", crewName);
  if (visibility === "Public") await app.click("Public");
  await app.click("Save & Continue");
  await app.waitFor("onboarding step 2", (s) => s.onboarding_step === 2);
}

/** Step 2: pick an avatar and a nickname, which creates the account and moves to step 3. */
export async function profileAtStep2(app: App, nickname: string, avatar = 0): Promise<void> {
  await app.click("Choose avatar", avatar);
  await app.type("CREW NICKNAME", nickname);
  await app.click("Continue");
  await app.waitFor("account created, step 3", (s) => s.onboarding_step === 3 && s.logged_in, 30_000);
}

/** Step 3: skip identity linking, which opens the app. */
export async function skipLinkingAtStep3(app: App): Promise<void> {
  await app.click("Skip for now");
  await app.waitFor("the app", (s) => s.screen === "app");
}

/** A fresh install that creates its own crew and reaches the app. */
export async function onboardWithNewCrew(
  app: App,
  crewName: string,
  nickname: string,
  visibility: Visibility = "Private",
): Promise<void> {
  await createCrewAtStep1(app, crewName, visibility);
  await profileAtStep2(app, nickname);
  await skipLinkingAtStep3(app);
  await app.waitFor(`crew ${crewName} in the sidebar`, (s) => s.crews.includes(crewName));
}
