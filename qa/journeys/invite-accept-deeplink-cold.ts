// INV-01 + INV-03: alice shares an invite link; bob, on a fresh install,
// opens it as a deep link. Step 1 is skipped (#68): the welcome screen says
// that alice invited him and names her crew. "Join" opens step 2 ("STEP 01 /
// 02"), and bob is a member of the crew when onboarding ends. Both sides are
// checked. Two journeys: a private crew (the app's default) and a public crew.

import { journey, type Journey, type JourneyContext } from "../../tools/mello-driver/src/journey.ts";
import {
  inviteWelcome,
  joinFromWelcome,
  linkEmailAtStep3,
  onboardWithNewCrew,
  profileAtStep2,
  type Visibility,
} from "./lib/onboarding.ts";

function inviteJourney(visibility: Visibility): Journey {
  return journey({
    id: `invite.accept-deeplink-cold.${visibility.toLowerCase()}`,
    flows: ["INV-01", "INV-03", "ONB-01"],
    run: (ctx) => run(ctx, visibility),
  });
}

export const privateCrew = inviteJourney("Private");
export const publicCrew = inviteJourney("Public");
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
  await step("bob: cold start from the deep link shows the welcome screen: alice invited him to her crew", async () => {
    await inviteWelcome(bob, crew, aliceName);
    const s = await bob.state();
    expect(!s.join_crew_modal_open, "no join modal: onboarding joins the crew");
    await bob.checkpoint("welcome");
  });

  await step("bob: Join opens step 2, step 1 of 2, which names alice's crew", async () => {
    await joinFromWelcome(bob, crew);
    const s = await bob.state();
    expect(s.onboarding_invite_path, "the invite path counts two steps");
    expect(!s.join_crew_modal_open, "no join modal: onboarding joins the crew");
    await bob.checkpoint("step2-invited-crew");
  });

  await step("bob: finishes onboarding (profile, then email at step 3)", async () => {
    await profileAtStep2(bob, bobName, 2);
    const s = await bob.state();
    expect(!s.join_crew_modal_open, "no join modal on step 3");
    expect(await bob.text("STEP 02 / 02"), `step 3 shows "STEP 02 / 02"`);
    await linkEmailAtStep3(bob, bobName);
  });

  await step("bob: alice's crew is his only crew, and it is active", async () => {
    const s = await bob.waitFor(`${crew} in bob's crews`, (st) => st.crews.includes(crew));
    expect(s.crews.length === 1, `only the invited crew, no other crew, got: ${s.crews.join(", ")}`);
    expect(!s.join_crew_modal_open, "no join modal in the app");
    const crewId = (await alice.state()).active_crew_id;
    await bob.waitFor(`${crew} is bob's active crew`, (st) => st.active_crew_id === crewId);
    const errors = (await bob.events()).filter((e) => e.message);
    expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
    await bob.checkpoint("bob-in-app");
  });

  await step("alice: sees bob in her crew", async () => {
    await alice.waitFor(`${bobName} in alice's member list`, (s) => s.members.includes(bobName));
    await alice.checkpoint("alice-sees-bob");
  });
}
