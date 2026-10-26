// FEED-05: five members, week 4 (mockup frame D). Head2head has no seats, the
// invite is its own calm card, and no skeleton shows. bob is real, four
// members are seeded over REST.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { CARRY_TAKER_PLAYTIME_MIN, profiles, reportLibrary } from "./lib/seed.ts";
import { bobWithSeededCrew, expectFullFeed, waitForFullFeed } from "./lib/stage.ts";

export default journey({
  id: "feed.stage-five",
  flows: ["FEED-05"],
  async run(ctx) {
    const { step, expect, runId } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 4);

    await step("head2head is the wide card with no seats, over five members", async () => {
      const s = await bob.waitFor("head2head for five members", (st) => st.head2head.member_count === 5 && st.head2head.pairs_count === 4, 30_000);
      const h = s.head2head;
      expect(h.placement === "wide" || h.placement === "versus", `placement is wide or versus, got ${h.placement}`);
      expect(h.open_seats === 0, `open_seats is 0, got ${h.open_seats}`);
      expect(h.pairs_count === 4, `pairs_count is 4, got ${h.pairs_count}`);
      expect(h.pair_rows === 4 && h.more_pairs === 0, `frame D: four pairs and no +N more, got ${h.pair_rows} and ${h.more_pairs}`);
    });

    await step("the invite card is there with five octagons and no seat", async () => {
      const s = await bob.waitFor("the invite card with five octagons", (st) => st.invite_card.present && st.invite_card.octagons === 5, 15_000);
      const full = await waitForFullFeed(bob);
      expect(s.invite_card.present, "the invite card shows");
      expect(s.invite_card.octagons === 5, `five octagons, got ${s.invite_card.octagons}`);
      expect(s.invite_card.more_count === 0, `no +N, got ${s.invite_card.more_count}`);
      expectFullFeed(full, expect);
      await bob.waitFor("every octagon wears its avatar", (st) => st.invite_card.with_avatar === 5, 15_000);

      await bob.checkpoint("five-feed");
    });

    await step("one daily recap stands in the grid for the seeded sessions, no catch-up card", async () => {
      const s = await bob.waitFor("the daily recap", (st) => st.daily_recap.present, 30_000);
      const d = s.daily_recap;
      expect(d.day !== "", `the card names its day, got "${d.day}"`);
      expect(d.sessions > 0, `sessions of the day, got ${d.sessions}`);
      expect(d.hangout !== "", "the hangout time shows");
      expect(d.members >= 1, `member octagons, got ${d.members}`);
      expect(d.belts.length <= 2, `two belts at most, got ${d.belts.length}`);
      expect(!s.feed_cards.some((c) => c.card_type === "catchup"), "no catch-up card");
      expect(s.feed_cards.filter((c) => c.card_type === "daily-recap").length === 1, "one daily recap card");
      await bob.checkpoint("five-daily-recap");
    });

    // A belt that moves between two other members is not the viewer's: no
    // hero, the wide card shows a red dot on that pair until the feed is opened
    // again (client/src/head2head.rs, spec HEAD2HEAD.md "Unread").
    await step("a belt moves between two other members: nothing is unread before it", async () => {
      await bob.restart();
      const s = await bob.waitFor("the wide card after a reopen", (st) => st.head2head.present && st.head2head.pairs_count === 4 && st.head2head.titles.length > 0, 30_000);
      expect(s.head2head.placement === "wide", `placement is wide, got ${s.head2head.placement}`);
      expect(s.head2head.pair_rows_dot === 0, `pair_rows_dot is 0 before the move, got ${s.head2head.pair_rows_dot}`);
      await bob.checkpoint("before-belt-move");
    });

    await step("a member re-reports a library that takes the_carry from another member", async () => {
      const s0 = await bob.state();
      const holder = s0.head2head.titles.find((t) => t.title_id === "the_carry")?.holder_name;
      const taker = bobTakerFor(holder, members);
      expect(taker !== undefined, `a seeded member other than the holder ${holder} exists`);
      // The taker keeps the library the seed gave them and plays the shared
      // top game far more than anyone. Every belt that moves lands on them,
      // so one pair row wears the dot.
      const own = profiles(members.length, runId).find((p) => p.name === taker!.name)?.games ?? [];
      const library = own.filter((g) => g.id !== "steam-730").map((g) => ({ id: g.id, name: g.name, playtimeMin: g.playtimeMin }));
      library.push({ id: "steam-730", name: "Counter-Strike 2", playtimeMin: CARRY_TAKER_PLAYTIME_MIN });
      await reportLibrary(taker!.token, library);
      const s = await bob.waitFor(
        "the_carry held by the taker and one pair row with the dot",
        (st) => st.head2head.titles.some((t) => t.title_id === "the_carry" && t.holder_name === taker!.name) && st.head2head.pair_rows_dot === 1,
        60_000,
      );
      expect(s.head2head.placement === "wide", `no hero for a move between others, placement ${s.head2head.placement}`);
      expect(s.head2head.pair_rows_dot === 1, `pair_rows_dot is 1, got ${s.head2head.pair_rows_dot}`);
      await bob.checkpoint("belt-moved-dot");
    });

    await step("the dot clears when the feed is opened again", async () => {
      await bob.restart();
      const s = await bob.waitFor("the wide card after the second reopen", (st) => st.head2head.present && st.head2head.pairs_count === 4 && st.head2head.titles.length > 0, 30_000);
      expect(s.head2head.pair_rows_dot === 0, `pair_rows_dot is 0 after a reopen, got ${s.head2head.pair_rows_dot}`);
      await bob.checkpoint("belt-moved-read");
    });
  },
});

/** A seeded member who is not the current holder of the title. */
function bobTakerFor<T extends { name: string }>(holder: string | undefined, members: T[]): T | undefined {
  return members.find((m) => m.name !== holder);
}
