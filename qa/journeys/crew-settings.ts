// CREW-04 (the part the driver can read): crew settings.
//
// alice owns a public crew, and bob is a member. alice opens crew settings,
// changes the name and the description, turns "Open crew" off, sets the
// invite policy to "Admins", and presses Save. alice sees the new name at
// once, and bob after he starts mello again. When alice opens crew settings
// again, the fields show the saved values.
//
// Not asserted: the state of "Open crew" and of the invite policy. The switch
// has no checked state and the policy pills have no value for the driver to
// read, and the state port has no field for them.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { closeCrewSettings, fieldValue, openCrewSettings } from "./lib/crew.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "crew.settings",
  flows: ["CREW-04"],
  async run(ctx) {
    const { runId, step, expect } = ctx;
    const { alice, bob, crew } = await twoUsersInOneCrew(ctx);
    const renamed = `Renamed ${runId}`;
    const description = `Only late games ${runId}`;

    await step("alice opens crew settings: the fields show the crew", async () => {
      await openCrewSettings(alice);
      expect((await fieldValue(alice, "CREW NAME")) === crew, `the name field shows ${crew}`);
      expect((await fieldValue(alice, "DESCRIPTION")) === "", "the description field is empty");
      await alice.checkpoint("settings-before");
    });

    await step("alice changes name and description, turns Open crew off, picks Admins, and saves", async () => {
      await alice.type("CREW NAME", renamed);
      await alice.type("DESCRIPTION", description);
      await alice.click("Open crew");
      await alice.click("Admins");
      const before = (await alice.events()).filter((e) => e.type === "CrewUpdated").length;
      await alice.click("Save");
      await alice.waitFor("the crew list has the new name", (s) => s.crews.includes(renamed) && !s.crews.includes(crew), 15_000);
      const updates = (await alice.events()).filter((e) => e.type === "CrewUpdated" || e.type === "CrewUpdateFailed").slice(before);
      expect(updates.length === 1 && updates[0].type === "CrewUpdated", `one CrewUpdated, got: ${updates.map((e) => e.type).join(", ")}`);
      await alice.checkpoint("saved");
      await closeCrewSettings(alice);
    });

    // A member's running app gets no crew event for a rename: spec 11 lists
    // no such event, and bob's crew list keeps the old name until it loads
    // again. bob sees the new name when he starts mello again.
    await step("bob starts mello again and sees the new name", async () => {
      await bob.restart();
      await bob.waitFor(`${renamed} in bob's crews`, (s) => s.screen === "app" && s.crews.includes(renamed) && !s.crews.includes(crew), 20_000);
      await bob.checkpoint("bob-renamed");
    });

    await step("alice opens crew settings again: the fields show the saved values", async () => {
      await openCrewSettings(alice);
      expect((await fieldValue(alice, "CREW NAME")) === renamed, `the name field shows ${renamed}`);
      expect((await fieldValue(alice, "DESCRIPTION")) === description, "the description field shows the new description");
      await closeCrewSettings(alice);
    });

    await step("no Error events on either app", async () => {
      for (const u of [alice, bob]) {
        const errors = (await u.events()).filter((e) => e.type === "Error");
        expect(errors.length === 0, `no Error events on ${u.name}, got: ${errors.map((e) => e.message).join(" | ")}`);
      }
    });
  },
});
