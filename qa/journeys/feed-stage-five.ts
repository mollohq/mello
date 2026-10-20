// FEED-05: five members, week 4 (mockup frame D). Head2head has no seats, the
// invite is its own calm card, and no skeleton shows. bob is real, four
// members are seeded over REST.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bobWithSeededCrew, expectFullFeed } from "./lib/stage.ts";

export default journey({
  id: "feed.stage-five",
  flows: ["FEED-05"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { bob } = await bobWithSeededCrew(ctx, 4);

    await step("head2head is a cell with no seats, over five members", async () => {
      const s = await bob.waitFor("head2head for five members", (st) => st.head2head.member_count === 5 && st.head2head.pairs_count === 4, 30_000);
      const h = s.head2head;
      expect(h.placement === "cell" || h.placement === "versus", `placement is cell or versus, got ${h.placement}`);
      expect(h.open_seats === 0, `open_seats is 0, got ${h.open_seats}`);
      expect(h.pairs_count === 4, `pairs_count is 4, got ${h.pairs_count}`);
      expect(h.pair_rows === 4 && h.more_pairs === 0, `frame D: four pairs and no +N more, got ${h.pair_rows} and ${h.more_pairs}`);
    });

    await step("the invite card is there with five octagons and no seat", async () => {
      const s = await bob.state();
      expect(s.invite_card.present, "the invite card shows");
      expect(s.invite_card.octagons === 5, `five octagons, got ${s.invite_card.octagons}`);
      expect(s.invite_card.more_count === 0, `no +N, got ${s.invite_card.more_count}`);
      expectFullFeed(s, expect);
      await bob.waitFor("every octagon wears its avatar", (st) => st.invite_card.with_avatar === 5, 15_000);
      
      await bob.checkpoint("five-feed");
    });
  },
});
