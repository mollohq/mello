// FEED-04: two members (mockup frames B2 and C). alice and bob are real apps
// on one machine, so both report the same Brawlhalla. Seeding gives alice
// games of her own, so the two libraries differ and titles are contested.
// Head2head shows one pair, three open seats and the red invite. Two PCs own
// games, so recent games is a poster grid of at most five tiles with owner pips,
// "From 2 PCs" on its header. The skeleton trio stays (frame B2). alice sees the
// joined line for bob, who came by her invite link.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { expectFullFeed, waitForFullFeed } from "./lib/stage.ts";
import { endSession, reportLibrary, signInEmail } from "./lib/seed.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "feed.stage-two",
  flows: ["FEED-04"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, aliceCreds } = await twoUsersInOneCrew(ctx, "Public");
    const crewId = (await alice.state()).active_crew_id;

    await step("alice reports games of her own and one session, so the two libraries differ", async () => {
      // The apps have reported their own libraries by now. A report from alice
      // replaces hers: do it after the apps. The server pushes "feed changed"
      // for the library report and the session end, and both open apps load
      // the feed again (at most once every 5 s).
      await Promise.all([alice, bob].map((a) => a.waitFor("the library report of the app", (s) => s.recent_games.tiles.length > 0, 30_000)));
      const token = await signInEmail(aliceCreds.email, aliceCreds.password);
      await reportLibrary(token, [
        { id: "steam-291550", name: "Brawlhalla", playtimeMin: 2_500 },
        { id: "steam-252950", name: "Rocket League", playtimeMin: 18_000 },
        { id: "steam-1097150", name: "Fall Guys", playtimeMin: 600 },
        { id: "steam-1245620", name: "Elden Ring", playtimeMin: 9_000 },
      ]);
      await endSession(token, crewId, { id: "steam-291550", name: "Brawlhalla" }, 40, 23 * 60);
    });

    for (const app of [bob, alice]) {
      await step(`${app.name} sees one pair, three open seats and contested titles`, async () => {
        const s = await app.waitFor(
          "head2head for two members",
          (st) => st.head2head.member_count === 2 && st.head2head.pairs_count === 1 && st.head2head.titles.length > 0 && st.recent_games.tiles.length > 1,
          40_000,
        );
        const h = s.head2head;
        expect(h.placement === "cell" || h.placement === "versus", `placement is cell or versus, got ${h.placement}`);
        expect(h.open_seats === 3, `open_seats is 3, got ${h.open_seats}`);
        expect(h.pairs_count === 1, `pairs_count is 1, got ${h.pairs_count}`);
        expect(h.titles.every((t) => t.holder_name !== ""), "every holder is on the member list");
        expect(!s.invite_card.present, "no invite card under five members");
      });
    }

    for (const app of [bob, alice]) {
      await step(`${app.name} sees recent games as a poster grid with owner pips from 2 PCs`, async () => {
        const s = await app.waitFor("the poster grid", (st) => st.recent_games.shape === "grid" && st.recent_games.tiles.length > 1, 30_000);
        const rg = s.recent_games;
        expect(rg.pc_count === 2, `pc_count is 2, got ${rg.pc_count}`);
        expect(rg.header_note === "From 2 PCs", `header note is "From 2 PCs", got "${rg.header_note}"`);
        expect(rg.tiles.length <= 5, `at most five tiles in the row, got ${rg.tiles.length}`);
        expect(rg.tiles.every((t) => t.owners >= 1), "every tile carries an owner pip");
        expect(rg.tiles.some((t) => t.owners === 2), "a game both members own carries two pips");
      });
    }

    await step("alice sees bob on the joined line, bob does not see his own join", async () => {
      const a = await alice.waitFor("the joined line", (st) => st.joined_rows.length > 0, 30_000);
      expect(a.joined_rows.length === 1, `one joined row, got ${a.joined_rows.length}`);
      expect(/ joined, invited by /.test(a.joined_rows[0].line), `the line names the inviter: "${a.joined_rows[0].line}"`);
      expect(/^(Today|Yesterday) \d\d:\d\d$/.test(a.joined_rows[0].when), `the time reads Today hh:mm: "${a.joined_rows[0].when}"`);
      expect((await bob.state()).joined_rows.length === 0, "bob does not see his own join");
    });

    await step("the skeleton trio stays at two members and no card is empty", async () => {
      // Skeleton rule (client/src/handlers/clip.rs, skeleton_trio_for): a cell
      // hides when its card type exists, and all three hide from three members.
      // This crew has no stream, clip or recap, so all three show.
      for (const app of [bob, alice]) {
        await waitForFullFeed(app, { skeletons: 3 });
        expectFullFeed(await app.state(), expect, { skeletons: 3 });
        await app.checkpoint("two-feed");
      }
    });
  },
});
