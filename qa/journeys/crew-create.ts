// CREW-01: create a crew from the app.
//
// alice uses mello with one crew. She presses "New crew", types a name and a
// description, picks Public and presses "Establish Crew". The modal shows the
// invite link, and "Done" closes it. The new crew is active: crew settings
// show its name and description.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { closeCrewSettings, fieldValue, openCrewSettings } from "./lib/crew.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

export default journey({
  id: "crew.create",
  flows: ["CREW-01"],
  async run({ runId, launch, step, expect }) {
    const first = `First ${runId}`;
    const crew = `Second ${runId}`;
    const description = `Late games ${runId}`;
    const alice = await launch("alice");

    await step("alice onboards with her first crew", async () => {
      await onboardWithNewCrew(alice, first, `alice${runId}`);
    });

    await step("alice opens New crew and fills in the form", async () => {
      await alice.click("New crew");
      await alice.waitFor("the new crew modal", (s) => s.open_modals.includes("new_crew"), 5_000);
      await alice.type("CREW NAME", crew);
      await alice.type("DESCRIPTION", description);
      await alice.click("Public");
      expect((await fieldValue(alice, "CREW NAME")) === crew, "the name field holds the name");
      expect((await fieldValue(alice, "DESCRIPTION")) === description, "the description field holds the description");
      await alice.checkpoint("form");
    });

    await step("Establish Crew: the modal shows the invite link", async () => {
      await alice.click("Establish Crew");
      const link = (await alice.find("Invite link", 0, 15_000)).value;
      expect(/\/join\/[A-Z0-9-]+$/.test(link), `the invite link is a join link, got "${link}"`);
      await alice.waitFor(`${crew} in alice's crews`, (s) => s.crews.includes(crew) && s.crews.includes(first));
      await alice.checkpoint("invite-link");
    });

    await step("Done closes the modal, and the new crew is active", async () => {
      await alice.click("Done");
      const s = await alice.waitFor("the new crew modal closed", (st) => !st.open_modals.includes("new_crew"), 5_000);
      expect(s.crews.length === 2, `two crews, got: ${s.crews.join(", ")}`);
      // Crew settings belong to the active crew.
      await openCrewSettings(alice);
      expect((await fieldValue(alice, "CREW NAME")) === crew, `crew settings show ${crew}`);
      expect((await fieldValue(alice, "DESCRIPTION")) === description, "crew settings show the description");
      await closeCrewSettings(alice);
      const errors = (await alice.events()).filter((e) => e.type === "Error");
      expect(errors.length === 0, `no Error events, got: ${errors.map((e) => e.message).join(" | ")}`);
      await alice.checkpoint("active");
    });
  },
});
