// FEED-03: a user alone in a new crew sees the solo feed (mockup frame A of
// next-gen-feed): the wide head2head card with the viewer's own values and the
// open seats, the recent games card from this PC, and the skeleton trio.
//
// Sharing is on, the onboarding default, so the library report runs and the
// server returns the viewer's own values.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

/** The names that a TitleChip draws, upper case (client/src/head2head.rs). */
const TITLE_NAMES: Record<string, string> = {
  the_carry: "THE CARRY",
  night_owl: "NIGHT OWL",
  the_collector: "THE COLLECTOR",
  locked_in: "LOCKED IN",
  has_a_life: "HAS A LIFE",
  sampler: "SAMPLER",
  one_trick: "ONE TRICK",
};

export default journey({
  id: "feed.solo-head2head",
  flows: ["FEED-03"],
  async run({ runId, launch, step, expect }) {
    const crew = `Solo ${runId}`;
    const bob = await launch("bob");

    await step("bob onboards alone in a new crew with sharing on", async () => {
      await onboardWithNewCrew(bob, crew, `bob${runId}`);
      await bob.waitFor("the app, logged in", (s) => s.screen === "app" && s.logged_in, 20_000);
    });

    await step("the head2head card arrives as the wide card for a crew of one", async () => {
      const s = await bob.waitFor("the head2head card", (st) => st.head2head.present, 30_000);
      const h = s.head2head;
      expect(h.placement === "wide", `placement is wide, got ${h.placement}`);
      expect(h.member_count === 1, `member_count is 1, got ${h.member_count}`);
      expect(h.open_seats === 4, `open_seats is 4, got ${h.open_seats}`);
      expect(h.titles.length === 0, `no contested titles, got ${JSON.stringify(h.titles)}`);
      expect(h.own_values.length > 0, "the card has own values");
    });

    await step("the card draws one chip for each own value, and four OPEN seats", async () => {
      const s = await bob.state();
      const texts = await bob.ui.texts();
      const names = Object.values(TITLE_NAMES);
      const drawn = texts.filter((t) => names.includes(t));
      expect(
        drawn.length === s.head2head.own_values.length,
        `${s.head2head.own_values.length} own values, ${drawn.length} chips drawn: ${drawn.join(", ")}`,
      );
      for (const v of s.head2head.own_values) {
        const name = TITLE_NAMES[v.title_id];
        expect(name !== undefined && drawn.includes(name), `a chip for ${v.title_id}`);
        expect(v.display !== "", `${v.title_id} has a value`);
      }
      expect(texts.filter((t) => t === "OPEN").length === 4, "four seats are labelled OPEN");
      expect(texts.includes("NOBODY TO COMPARE WITH YET"), "the empty-compare line shows");
    });

    await step("the invite button is a control", async () => {
      const c = await bob.find("Invite your crew");
      expect(c.label === "Invite your crew", `label is "Invite your crew", got "${c.label}"`);
    });

    await step("recent games come from this PC", async () => {
      const s = await bob.waitFor("recent games tiles", (st) => st.recent_games.tiles.length > 0, 30_000);
      const rg = s.recent_games;
      expect(rg.header_note === "From this PC", `header note is "From this PC", got "${rg.header_note}"`);
      for (const t of rg.tiles) {
        expect(t.name !== "", "a tile has a name");
        expect(t.hours_text !== "" || t.launcher !== "", `${t.name} shows hours or a launcher`);
      }
      expect(!!(await bob.text("RECENT GAMES")), "the RECENT GAMES label shows");
    });

    await step("the skeleton trio shows three cells and no card is empty", async () => {
      const s = await bob.state();
      expect(s.skeleton_trio === 3, `skeleton_trio is 3, got ${s.skeleton_trio}`);
      expect(s.head2head.own_values.length > 0, "the head2head card has content");
      expect(s.recent_games.tiles.length > 0, "the recent games card has content");
      for (const c of s.feed_cards) {
        expect(c.card_type !== "", `feed card ${c.id} has a type`);
      }
      await bob.checkpoint("solo-feed");
    });
  },
});
