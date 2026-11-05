// FEED-05: five members, the first five complete. The People zone has five
// octagons and no seat, so no red invite. The board holds the belts and
// today. A belt that moves between two other members is sealed and unread on
// the board until the feed is opened again. bob is real, four members are
// seeded over REST. Spec features/CREW-FEED-ZONES.md.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bobWithSeededCrew, carryBelt, expectFullFeed, moveTheCarryBetweenOthers, waitForFullFeed } from "./lib/stage.ts";

export default journey({
  id: "feed.stage-five",
  flows: ["FEED-05"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 4);

    await step("the first five are complete: five octagons, no seat, no invite", async () => {
      const s = await bob.waitFor("five octagons and no seat", (st) => st.feed_zones.people.members === 5 && st.feed_zones.people.open_seats === 0, 30_000);
      const labels = (await bob.controls()).map((c) => c.label);
      expect(!labels.includes("Invite your crew"), "no red invite once the first five are complete");
      expectFullFeed(await waitForFullFeed(bob), expect);
      await bob.waitFor("every octagon wears its avatar", (st) => st.feed_zones.people.members_with_avatar === 5, 15_000);
      expect(s.feed_zones.people.members === 5, "five octagons");
      await bob.checkpoint("five-feed");
    });

    await step("today on the board counts the seeded sessions", async () => {
      const s = await bob.waitFor("today on the board", (st) => !st.feed_zones.today.empty && st.feed_zones.today.sessions > 0, 30_000);
      const t = s.feed_zones.today;
      expect(t.hangout !== "", "the hangout time shows");
      expect(t.on >= 1, `the members who took part, got ${t.on}`);
      await bob.checkpoint("five-today");
    });

    await step("a belt moves between two other members: nothing is unread before it", async () => {
      await bob.restart();
      const s = await waitForFullFeed(bob);
      expect(s.feed_zones.belts.every((b) => !b.unread), `no unread belt before the move, got ${JSON.stringify(s.feed_zones.belts)}`);
      await bob.checkpoint("before-belt-move");
    });

    await step("a member re-reports a library that takes the_carry from another member", async () => {
      const taker = await moveTheCarryBetweenOthers(ctx, bob, members);
      const s = await bob.waitFor(
        "the_carry held by the taker, sealed and unread",
        (st) => carryBelt(st)?.holder_name === taker.name && carryBelt(st)?.unread === true,
        60_000,
      );
      const carry = carryBelt(s)!;
      expect(carry.sealed, "a belt that moved this week is sealed");
      expect(s.feed_zones.stage !== "card" || s.feed_zones.belt_hero === null, "a move between others is no belt on bob's Stage");
      await bob.checkpoint("belt-moved-unread");
    });

    await step("the unread mark clears when the feed is opened again", async () => {
      await bob.restart();
      const s = await bob.waitFor("the_carry on the board after a reopen", (st) => carryBelt(st) !== undefined, 30_000);
      expect(!carryBelt(s)!.unread, "the_carry is read after a reopen");
      await bob.checkpoint("belt-moved-read");
    });
  },
});
