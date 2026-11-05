// FEED-10: a belt that moved arrives sealed on the board. A click opens it in
// the centre of the feed, the face shows, and a click on the scrim sends it
// back to its slot, open. Spec SEALED-REVEAL.md. bob is real, four members
// are seeded over REST, and one of them takes the_carry (the move of
// feed-stage-five).

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { generateRecap } from "./lib/seed.ts";
import { bobWithSeededCrew, carryBelt, moveTheCarryBetweenOthers, waitForFullFeed } from "./lib/stage.ts";

export default journey({
  id: "feed.sealed-reveal",
  flows: ["FEED-10"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 4);

    await step("nothing is sealed before a belt moves", async () => {
      await bob.restart();
      const s = await waitForFullFeed(bob);
      expect(s.sealed.belts === 0, `sealed.belts is 0 before the move, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `open_center is none, got ${s.sealed.open_center}`);
    });

    await step("a member re-reports a library that takes the_carry: one belt is sealed", async () => {
      const taker = await moveTheCarryBetweenOthers(ctx, bob, members);
      const s = await bob.waitFor(
        "the_carry held by the taker and one sealed belt",
        (st) => carryBelt(st)?.holder_name === taker.name && st.sealed.belts >= 1,
        60_000,
      );
      expect(s.sealed.belts === 1, `sealed.belts is 1, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `nothing in the centre, got ${s.sealed.open_center}`);
      expect(carryBelt(s)!.sealed && carryBelt(s)!.unread, "the_carry is sealed and unread on the board");
      await bob.checkpoint("sealed-in-the-feed");
    });

    await step("a click on the sealed belt opens it in the centre", async () => {
      await bob.scrollToControl("Sealed belt, open it");
      await bob.click("Sealed belt, open it");
      const s = await bob.waitFor("the belt in the centre", (st) => st.sealed.open_center === "belt", 5_000);
      expect(s.sealed.belts === 0, `the belt in the centre is no longer sealed in the feed, got ${s.sealed.belts}`);
      await bob.checkpoint("reveal-start");
      // The face shows from 1.03 s and the card rests from 1.25 s.
      await bob.waitFor("the card at rest, the face shown", (st) => st.sealed.resting, 5_000);
      await bob.checkpoint("reveal-face");
    });

    await step("a click on the scrim sends the card back: the slot is open", async () => {
      await bob.click("Close");
      const s = await bob.waitFor(
        "the card landed, open",
        (st) => st.sealed.open_center === "none" && st.sealed.belts === 0 && carryBelt(st)?.sealed === false,
        10_000,
      );
      expect(s.sealed.belts === 0, `no belt is sealed, got ${s.sealed.belts}`);
      await bob.checkpoint("belt-open-in-its-slot");
    });

    await step("the belt stays open when the app starts again", async () => {
      await bob.restart();
      const s = await waitForFullFeed(bob);
      expect(s.sealed.belts === 0, `sealed.belts is 0 after a restart, got ${s.sealed.belts}`);
      expect(carryBelt(s)?.sealed === false, "the_carry stays open");
    });

    // The weekly recap stands sealed on the board and opens in the centre.
    // It needs a recap, which only dev_fault writes. Without dev_fault the
    // step has nothing to open.
    await step("the weekly recap opens in the centre and the board shows the week", async () => {
      const crewId = (await bob.state()).active_crew_id;
      if (!(await generateRecap(members[0].token, crewId))) {
        console.log("dev_fault is off on this stack: no recap to open, step skipped");
        return;
      }
      await bob.restart();
      await bob.waitFor("the sealed recap on the board", (st) => st.sealed.recap && st.feed_zones.board === "sealed", 30_000);
      await bob.click("Sealed recap, open it");
      await bob.waitFor("the recap in the centre", (st) => st.sealed.open_center === "recap", 5_000);
      await bob.checkpoint("recap-centre");
      await bob.waitFor("the recap at rest, the face shown", (st) => st.sealed.resting, 5_000);
      await bob.checkpoint("recap-face");
      await bob.click("Close");
      const s = await bob.waitFor("the recap landed", (st) => st.sealed.open_center === "none", 10_000);
      expect(s.feed_zones.board === "tabs", `the opened week is the board's Week tab, got ${s.feed_zones.board}`);
    });
  },
});
