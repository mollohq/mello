// CREW-06 (alice's side): promote, demote and kick a member.
//
// alice owns the crew, and bob is a member. On the MEMBERS tab of crew
// settings, alice promotes bob, demotes him, and kicks him. Each role change
// flips the label of the button ("Promote …" and "Demote …"). After the
// kick, bob is not in alice's member list, and bob's crews no longer have
// the crew.
//
// Not asserted: what bob's running app shows after the kick (an open
// question). bob's crew list is read after he starts mello again.

import type { App } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { openCrewSettings } from "./lib/crew.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

/** Wait until a control with this label is on screen and the other is not. */
async function flipsTo(app: App, shown: string, gone: string): Promise<void> {
  await app.find(shown, 0, 10_000);
  const labels = (await app.controls()).map((c) => c.label);
  if (labels.includes(gone)) throw new Error(`${app.name}: "${gone}" is still on screen next to "${shown}"`);
}

/** The events of these types after the first `skip` of them. */
async function eventsAfter(app: App, types: string[], skip: number): Promise<string[]> {
  return (await app.events())
    .filter((e) => types.includes(e.type))
    .slice(skip)
    .map((e) => e.type);
}

const ROLE = ["CrewRoleChanged", "CrewRoleChangeFailed"];
const KICK = ["CrewMemberKicked", "CrewMemberKickFailed"];

export default journey({
  id: "crew.members",
  flows: ["CREW-06"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, crew, bobName } = await twoUsersInOneCrew(ctx);
    const promote = `Promote ${bobName}`;
    const demote = `Demote ${bobName}`;
    const kick = `Kick ${bobName}`;

    await step("alice opens the MEMBERS tab: bob is a member", async () => {
      await alice.waitFor(`${bobName} in alice's members`, (s) => s.members.includes(bobName));
      await openCrewSettings(alice, "MEMBERS");
      await flipsTo(alice, promote, demote);
      await alice.find(kick);
      await alice.checkpoint("members");
    });

    await step(`alice promotes bob: the button says "${demote}"`, async () => {
      await alice.click(promote);
      await flipsTo(alice, demote, promote);
      const got = await eventsAfter(alice, ROLE, 0);
      expect(got.length === 1 && got[0] === "CrewRoleChanged", `one CrewRoleChanged, got: ${got.join(", ")}`);
      await alice.checkpoint("promoted");
    });

    await step(`alice demotes bob: the button says "${promote}"`, async () => {
      await alice.click(demote);
      await flipsTo(alice, promote, demote);
      const got = await eventsAfter(alice, ROLE, 1);
      expect(got.length === 1 && got[0] === "CrewRoleChanged", `one more CrewRoleChanged, got: ${got.join(", ")}`);
      await alice.checkpoint("demoted");
    });

    await step("alice kicks bob: he leaves her member list", async () => {
      await alice.click(kick);
      await alice.waitFor(`${bobName} not in alice's members`, (s) => !s.members.includes(bobName), 10_000);
      const got = await eventsAfter(alice, KICK, 0);
      expect(got.length === 1 && got[0] === "CrewMemberKicked", `one CrewMemberKicked, got: ${got.join(", ")}`);
      const labels = (await alice.controls()).map((c) => c.label);
      expect(![promote, demote, kick].some((l) => labels.includes(l)), "no member buttons for bob");
      await alice.checkpoint("kicked");
    });

    await step("bob starts mello again: his crews no longer have the crew", async () => {
      await bob.restart();
      const s = await bob.waitFor("bob's app after the restart, crews loaded", (st) => st.screen === "app" && st.logged_in, 20_000);
      await bob.settle(["CrewsLoaded"]);
      const after = await bob.state();
      expect(!after.crews.includes(crew), `${crew} is not in bob's crews, got: ${after.crews.join(", ")}`);
      expect(s.user_name === bobName, `still bob, got "${s.user_name}"`);
      await bob.checkpoint("bob-after-kick");
    });

    await step("no Error events on alice", async () => {
      const errors = (await alice.events()).filter((e) => e.type === "Error");
      expect(errors.length === 0, `no Error events on alice, got: ${errors.map((e) => e.message).join(" | ")}`);
    });
  },
});
