// INV-08: bob opens alice's invite link on a fresh install and presses
// "Not now" on the welcome screen. Step 1 opens and the invite is forgotten:
// bob creates his own crew, and finishing onboarding does not join alice's
// crew. Both sides are checked.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { createCrewAtStep1, inviteWelcome, linkEmailAtStep3, onboardWithNewCrew, profileAtStep2 } from "./lib/onboarding.ts";

export default journey({
  id: "invite.not-now-cold",
  flows: ["INV-08", "ONB-01"],
  async run({ runId, launch, step, expect }) {
    const aliceCrew = `Night Owls ${runId}`;
    const bobCrew = `Own Way ${runId}`;
    const aliceName = `alice${runId}`;
    const bobName = `bob${runId}`;

    const alice = await launch("alice");
    let code = "";
    await step("alice: creates a crew and reads the invite link", async () => {
      await onboardWithNewCrew(alice, aliceCrew, aliceName);
      await alice.click("Share invite link");
      const m = (await alice.find("Invite link")).value.match(/\/join\/([A-Z0-9-]+)$/);
      expect(m, "the invite modal shows a join link");
      code = m![1];
      await alice.checkpoint("invite-link");
    });

    const bob = await launch("bob", `mello://join/${code}`);
    await step("bob: cold start from the deep link shows the welcome screen", async () => {
      await inviteWelcome(bob, aliceCrew, aliceName);
      await bob.checkpoint("welcome");
    });

    await step("bob: Not now opens step 1 and forgets the invite", async () => {
      await bob.click("Not now — show me other crews");
      const s = await bob.waitFor(
        "onboarding step 1",
        (st) => st.screen === "onboarding" && st.onboarding_step === 1 && !st.invite_welcome,
      );
      expect(s.onboarding_invite_crew_name === "", `no invited crew, got "${s.onboarding_invite_crew_name}"`);
      expect(!s.onboarding_invite_path, "three steps again: step 1 is not skipped");
      expect(!s.join_crew_modal_open, "no join modal");
      expect(await bob.text("STEP 01 / 03"), `step 1 shows "STEP 01 / 03"`);
      await bob.checkpoint("step1-after-not-now");
    });

    await step("bob: creates his own crew and finishes onboarding", async () => {
      await createCrewAtStep1(bob, bobCrew);
      let s = await bob.state();
      expect(s.onboarding_invite_crew_name === "", "step 2 does not name alice's crew");
      expect(await bob.text("STEP 02 / 03"), `step 2 shows "STEP 02 / 03"`);
      await profileAtStep2(bob, bobName, 1);
      s = await bob.state();
      expect(!s.join_crew_modal_open, "no join modal on step 3");
      await linkEmailAtStep3(bob, bobName);
    });

    await step("bob: his own crew is his only crew; alice's crew is not joined", async () => {
      const s = await bob.waitFor(`${bobCrew} in bob's crews`, (st) => st.crews.includes(bobCrew), 20_000);
      // Crews load in one answer: the list is complete once bob's crew is in it.
      expect(s.crews.length === 1, `only bob's crew, got: ${s.crews.join(", ")}`);
      expect(!s.crews.includes(aliceCrew), "bob is not a member of alice's crew");
      expect(!s.join_crew_modal_open, "the declined invite does not come back as a join modal");
      const errors = (await bob.events()).filter((e) => e.message);
      expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
      await bob.checkpoint("bob-in-app");
    });

    await step("alice: bob is not in her crew", async () => {
      const s = await alice.state();
      expect(!s.members.includes(bobName), `bob is not in alice's member list, got: ${s.members.join(", ")}`);
      await alice.checkpoint("alice-without-bob");
    });
  },
});
