// INV-02: bob already uses mello (his own crew, app open). He clicks alice's
// invite link; the OS hands the link to his running app, which offers the
// crew. He joins without restarting.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

export default journey({
  id: "invite.accept-while-running",
  flows: ["INV-02"],
  // #84: alice sees bob by username after he joins.
  knownIssues: [84],
  async run({ runId, launch, step, expect }) {
    const crew = `Owls ${runId}`;
    const aliceName = `alice${runId}`;
    const bobName = `bob${runId}`;

    const alice = await launch("alice");
    const bob = await launch("bob");
    let code = "";

    await step("both onboard with their own crews", async () => {
      await Promise.all([
        onboardWithNewCrew(alice, crew, aliceName, "Public"),
        onboardWithNewCrew(bob, `Bob Home ${runId}`, bobName, "Public"),
      ]);
    });

    await step("alice reads the invite link", async () => {
      await alice.click("Share invite link");
      const m = (await alice.find("Invite link")).value.match(/\/join\/([A-Z0-9-]+)$/);
      expect(m, "the invite modal shows a join link");
      code = m![1];
      await alice.dismiss("invite_share");
    });

    await step("the OS hands the link to bob's running app", async () => {
      await bob.openLink(`mello://join/${code}`);
      await bob.waitFor("join modal for alice's crew", (s) => s.join_crew_modal_open && s.join_crew_name === crew);
      await bob.checkpoint("join-modal");
    });

    await step("bob joins and has both crews", async () => {
      await bob.click("Join crew");
      await bob.waitFor(`${crew} in bob's crews`, (s) => s.crews.includes(crew) && s.crews.includes(`Bob Home ${runId}`));
    });

    await step("alice sees bob in her crew", async () => {
      await alice.waitFor(`${bobName} in alice's members`, (s) => s.members.includes(bobName));
    });
  },
});
