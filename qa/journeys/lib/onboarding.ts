// Reusable onboarding steps. Labels are the text the user reads
// (client/src/a11y_lint.rs keeps them on every control).

import { randomBytes } from "node:crypto";

import { DriverError, type App } from "../../../tools/mello-driver/src/app.ts";

export type Visibility = "Private" | "Public";

/** The email identity a journey links at step 3. */
export type Credentials = { email: string; password: string };

/** Step 1: create a new crew with this name, which moves to step 2. Private is the app's default. */
export async function createCrewAtStep1(app: App, crewName: string, visibility: Visibility = "Private"): Promise<void> {
  await app.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
  // A fresh install loads the crew list twice and rebuilds step 1 each time
  // (#85). Act on the final grid, not the one about to be replaced.
  await app.settle(["DiscoverCrewsLoaded"]);
  await app.click("Create your own crew");
  await app.type("CREW NAME", crewName);
  if (visibility === "Public") await app.click("Public");
  await app.click("Save & Continue");
  await app.waitFor("onboarding step 2", (s) => s.onboarding_step === 2);
}

/**
 * A fresh install opened from an invite link skips step 1 (#68): it opens
 * step 2, which names the invited crew. Waits for that step 2 and checks the
 * crew name is on screen.
 */
export async function inviteAtStep2(app: App, crewName: string): Promise<void> {
  await app.waitFor(
    `onboarding step 2 that joins ${crewName}`,
    (s) => s.screen === "onboarding" && s.onboarding_step === 2 && s.onboarding_invite_crew_name === crewName,
    30_000,
  );
  if (!(await app.text(crewName))) throw new DriverError(`expectation failed: step 2 shows the invited crew "${crewName}"`);
}

/** Step 2: pick an avatar and a nickname, which creates the account and moves to step 3. */
export async function profileAtStep2(app: App, nickname: string, avatar = 0): Promise<void> {
  await app.click("Choose avatar", avatar);
  await app.type("CREW NICKNAME", nickname);
  await app.click("Continue");
  await app.waitFor("account created, step 3", (s) => s.onboarding_step === 3 && s.logged_in, 30_000);
}

/**
 * Step 3: link email + password, which opens the app. Step 3 has no skip: a
 * user must link one identity. The email is `<nickname>@example.test`; the
 * password is new for each call.
 */
export async function linkEmailAtStep3(app: App, nickname: string): Promise<Credentials> {
  const creds = { email: `${nickname.toLowerCase()}@example.test`, password: `pw-${randomBytes(12).toString("base64url")}` };
  await app.click("Email + password");
  await app.type("Email", creds.email);
  await app.type("Password", creds.password);
  await app.click("Link account");
  await app.waitFor("the app after linking email", (s) => s.screen === "app", 20_000);
  return creds;
}

/** A fresh install that creates its own crew, links email and reaches the app. */
export async function onboardWithNewCrew(
  app: App,
  crewName: string,
  nickname: string,
  visibility: Visibility = "Private",
): Promise<Credentials> {
  await createCrewAtStep1(app, crewName, visibility);
  await profileAtStep2(app, nickname);
  const creds = await linkEmailAtStep3(app, nickname);
  await app.waitFor(`crew ${crewName} in the sidebar`, (s) => s.crews.includes(crewName));
  return creds;
}

/**
 * A fresh install opened from an invite link: step 2 names the crew, step 3
 * links email, and the app opens with the invited crew as the only crew.
 */
export async function onboardFromInvite(app: App, crewName: string, nickname: string, avatar = 2): Promise<Credentials> {
  await inviteAtStep2(app, crewName);
  await profileAtStep2(app, nickname, avatar);
  const creds = await linkEmailAtStep3(app, nickname);
  await app.waitFor(`crew ${crewName} in the sidebar`, (s) => s.crews.includes(crewName));
  return creds;
}
