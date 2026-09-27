// INV-01 + INV-03: alice shares an invite link; bob, on a fresh install,
// opens it as a deep link and joins alice's crew. Both sides are checked.
// Two journeys: a private crew (the app's default) and a public crew.

import { journey, type Journey, type JourneyContext } from "../../tools/mello-driver/src/journey.ts";
import {
  createCrewAtStep1,
  onboardWithNewCrew,
  profileAtStep2,
  skipLinkingAtStep3,
  type Visibility,
} from "./lib/onboarding.ts";

// #68: a fresh install does not offer the invited crew at step 1, so bob makes
// a crew first and the join modal opens on step 3. When #68 is fixed, these
// journeys must change to join at step 1.
function inviteJourney(visibility: Visibility, knownIssues: number[]): Journey {
  return journey({
    id: `invite.accept-deeplink-cold.${visibility.toLowerCase()}`,
    flows: ["INV-01", "INV-03", "ONB-01"],
    knownIssues,
    run: (ctx) => run(ctx, visibility),
  });
}

// #83: a private-crew invite makes a join request, not a member.
// #84: alice sees bob by his random username, not his display name.
export const privateCrew = inviteJourney("Private", [68, 83, 84]);
export const publicCrew = inviteJourney("Public", [68, 84]);
export default privateCrew;

async function run({ runId, launch, step, expect }: JourneyContext, visibility: Visibility): Promise<void> {
  const crew = `Night Owls ${runId}`;
  const aliceName = `alice${runId}`;
  const bobName = `bob${runId}`;

  const alice = await launch("alice");
  await step("alice: fresh install, creates a crew, reaches the app", async () => {
    await onboardWithNewCrew(alice, crew, aliceName, visibility);
    await alice.checkpoint("alice-in-app");
  });

  let code = "";
  await step("alice: opens the invite modal and reads the link on screen", async () => {
    await alice.click("Share invite link");
    const link = await alice.find("Invite link");
    const m = link.value.match(/\/join\/([A-Z0-9-]+)$/);
    expect(m, `the invite link field shows a join URL, got "${link.value}"`);
    code = m![1];
    await alice.checkpoint("invite-link");
  });

  const bob = await launch("bob", `mello://join/${code}`);
  await step("bob: cold start from the deep link, onboards", async () => {
    await createCrewAtStep1(bob, `Bob Placeholder ${runId}`);
    await profileAtStep2(bob, bobName, 2);
  });

  await step("bob: sees the invited crew in the join modal", async () => {
    await bob.waitFor("join modal for the invited crew", (s) => s.join_crew_modal_open && s.join_crew_name === crew, 30_000);
    await bob.checkpoint("join-modal");
  });

  await step("bob: joins, the crew is in his list", async () => {
    await bob.click("Join crew");
    await bob.waitFor(`${crew} in bob's crews`, (s) => s.crews.includes(crew));
    const errors = (await bob.events()).filter((e) => e.message);
    expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
  });

  await step("bob: finishes onboarding into the app", async () => {
    await skipLinkingAtStep3(bob);
    await bob.checkpoint("bob-in-app");
  });

  await step("alice: sees bob in her crew", async () => {
    await alice.waitFor(`${bobName} in alice's member list`, (s) => s.members.includes(bobName));
    await alice.checkpoint("alice-sees-bob");
  });
}
