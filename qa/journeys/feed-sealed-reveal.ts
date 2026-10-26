// FEED-10: a belt that moved arrives sealed. A click opens it in the centre of
// the feed, the face shows, and a click on the scrim sends it back to its slot,
// open, with the unread dot cleared. Plan SEALED-REVEAL.md. bob is real, four
// members are seeded over REST, and one of them takes the_carry (the move of
// feed-stage-five).

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { CARRY_TAKER_PLAYTIME_MIN, profiles, reportLibrary } from "./lib/seed.ts";
import { bobWithSeededCrew } from "./lib/stage.ts";

export default journey({
  id: "feed.sealed-reveal",
  flows: ["FEED-10"],
  async run(ctx) {
    const { step, expect, runId } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 4);

    await step("nothing is sealed before a belt moves", async () => {
      await bob.restart();
      const s = await bob.waitFor("the wide card after a reopen", (st) => st.head2head.present && st.head2head.pairs_count === 4 && st.head2head.titles.length > 0, 30_000);
      expect(s.sealed.belts === 0, `sealed.belts is 0 before the move, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `open_center is none, got ${s.sealed.open_center}`);
    });

    await step("a member re-reports a library that takes the_carry: one belt is sealed", async () => {
      const s0 = await bob.state();
      const holder = s0.head2head.titles.find((t) => t.title_id === "the_carry")?.holder_name;
      const taker = members.find((m) => m.name !== holder);
      expect(taker !== undefined, `a seeded member other than the holder ${holder} exists`);
      const own = profiles(members.length, runId).find((p) => p.name === taker!.name)?.games ?? [];
      const library = own.filter((g) => g.id !== "steam-730").map((g) => ({ id: g.id, name: g.name, playtimeMin: g.playtimeMin }));
      library.push({ id: "steam-730", name: "Counter-Strike 2", playtimeMin: CARRY_TAKER_PLAYTIME_MIN });
      await reportLibrary(taker!.token, library);
      const s = await bob.waitFor(
        "the_carry held by the taker and one sealed belt",
        (st) => st.head2head.titles.some((t) => t.title_id === "the_carry" && t.holder_name === taker!.name) && st.sealed.belts >= 1,
        60_000,
      );
      expect(s.sealed.belts === 1, `sealed.belts is 1, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `nothing in the centre, got ${s.sealed.open_center}`);
      expect(s.head2head.pair_rows_dot === 1, `one pair row wears the dot, got ${s.head2head.pair_rows_dot}`);
      await bob.checkpoint("sealed-in-the-feed");
    });

    await step("a click on the sealed chip opens the belt in the centre", async () => {
      await bob.click("Sealed belt, open it");
      const s = await bob.waitFor("the belt in the centre", (st) => st.sealed.open_center === "belt", 5_000);
      expect(s.sealed.belts === 0, `the belt in the centre is no longer sealed in the feed, got ${s.sealed.belts}`);
      await bob.checkpoint("reveal-start");
      // The face shows from 1.03 s and the card rests from 1.25 s.
      await bob.waitFor("the card at rest, the face shown", (st) => st.sealed.resting, 5_000);
      await bob.checkpoint("reveal-face");
    });

    await step("a click on the scrim sends the card back: the slot is open and the dot is clear", async () => {
      await bob.click("Close");
      const s = await bob.waitFor(
        "the card landed, open, with no dot",
        (st) => st.sealed.open_center === "none" && st.sealed.belts === 0 && st.head2head.pair_rows_dot === 0,
        10_000,
      );
      expect(s.sealed.belts === 0, `no belt is sealed, got ${s.sealed.belts}`);
      expect(s.head2head.pair_rows_dot === 0, `the dot is clear, got ${s.head2head.pair_rows_dot}`);
      await bob.checkpoint("belt-open-in-its-slot");
    });

    await step("the belt stays open when the app starts again", async () => {
      await bob.restart();
      const s = await bob.waitFor("the wide card after a reopen", (st) => st.head2head.present && st.head2head.pairs_count === 4 && st.head2head.titles.length > 0, 30_000);
      expect(s.sealed.belts === 0, `sealed.belts is 0 after a restart, got ${s.sealed.belts}`);
    });
  },
});
