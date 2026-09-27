// Shared starting points for multi-user journeys.

import type { App } from "../../../tools/mello-driver/src/app.ts";
import type { JourneyContext } from "../../../tools/mello-driver/src/journey.ts";
import { createCrewAtStep1, onboardWithNewCrew, profileAtStep2, skipLinkingAtStep3, type Visibility } from "./onboarding.ts";

export type Crew = { alice: App; bob: App; crew: string; aliceName: string; bobName: string };

/**
 * alice creates a crew and invites bob; bob joins with the link and both reach
 * the app with the crew active. Public by default: a private-crew invite does
 * not make a member today (#83).
 */
export async function twoUsersInOneCrew(ctx: JourneyContext, visibility: Visibility = "Public"): Promise<Crew> {
  const { runId, launch, step, expect } = ctx;
  const crew = `Crew ${runId}`;
  const aliceName = `alice${runId}`;
  const bobName = `bob${runId}`;

  const alice = await launch("alice");
  let code = "";
  await step("setup: alice creates a crew and reads the invite link", async () => {
    await onboardWithNewCrew(alice, crew, aliceName, visibility);
    await alice.click("Share invite link");
    const m = (await alice.find("Invite link")).value.match(/\/join\/([A-Z0-9-]+)$/);
    expect(m, "the invite modal shows a join link");
    code = m![1];
    await alice.click("Copy link");
    await alice.dismiss("invite_share"); // no close button or Escape: click outside
  });

  const bob = await launch("bob", `mello://join/${code}`);
  await step("setup: bob joins with the link and reaches the app", async () => {
    await createCrewAtStep1(bob, `Bob Placeholder ${runId}`); // #68
    await profileAtStep2(bob, bobName, 2);
    await bob.waitFor("join modal", (s) => s.join_crew_modal_open && s.join_crew_name === crew, 30_000);
    await bob.click("Join crew");
    await bob.waitFor(`${crew} in bob's crews`, (s) => s.crews.includes(crew));
    await skipLinkingAtStep3(bob);
  });

  await step("setup: both have the crew active", async () => {
    await selectCrew(alice, crew);
    await selectCrew(bob, crew);
  });
  return { alice, bob, crew, aliceName, bobName };
}

/**
 * Make `crew` the active crew. Only a crew that is not active has a card in
 * the sidebar that the user can click, so a missing card means it is active.
 */
export async function selectCrew(app: App, crew: string): Promise<void> {
  const card = (await app.controls()).find((c) => c.role === "Button" && c.label === crew);
  if (!card) return;
  const before = (await app.state()).active_crew_id;
  await app.click(crew);
  await app.waitFor(`${crew} becomes the active crew`, (s) => s.active_crew_id !== before);
}
