// FEED-04: two members (mockup frames B2 and C). alice and bob are real apps
// on one machine, so both report the same Brawlhalla. Seeding gives alice
// games of her own, so the two libraries differ and titles are contested.
// Head2head shows one pair, three open seats and the red invite. Two PCs own
// games, so recent games is a poster grid of at most five tiles with owner pips,
// "From 2 PCs" on its header. The skeleton trio stays (frame B2). alice sees the
// join card for bob, who came by her invite link: a standard cell after the
// first row, with "Say hi". The art Worker serves Brawlhalla, Rocket League and
// Fall Guys (qa/fixtures/art-index): their tiles show art. Start the Worker
// first: scripts/e2e-art-worker.sh up.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { DEFAULT_ART_BASE } from "../../tools/mello-driver/src/app.ts";
import { expectFullFeed, waitForFullFeed } from "./lib/stage.ts";
import { endSession, reportLibrary, signInEmail } from "./lib/seed.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "feed.stage-two",
  flows: ["FEED-04"],
  async run(ctx) {
    const { step, expect } = ctx;
    await step("the local art Worker answers", async () => {
      const base = process.env.MELLO_E2E_ART_BASE ?? DEFAULT_ART_BASE;
      const ok = await fetch(`${base}/health`, { signal: AbortSignal.timeout(3_000) }).then((r) => r.ok, () => false);
      expect(ok, `no art Worker at ${base}; run scripts/e2e-art-worker.sh up`);
    });
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
        expect(h.placement === "wide" || h.placement === "versus", `placement is wide or versus, got ${h.placement}`);
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
        expect(!rg.header_line.startsWith("From "), `the header carries no note, got "${rg.header_line}"`);
        expect(rg.tiles.length <= 5, `at most five tiles in the row, got ${rg.tiles.length}`);
        expect(rg.tiles.every((t) => t.owners >= 1), "every tile carries an owner pip");
        expect(rg.tiles.some((t) => t.owners === 2), "a game both members own carries two pips");
      });
    }

    for (const app of [bob, alice]) {
      await step(`${app.name} sees Worker art on the tiles of the indexed games`, async () => {
        const s = await app.waitFor(
          "art on the tiles of the indexed games",
          (st) =>
            ["Brawlhalla", "Rocket League", "Fall Guys"].every((n) => {
              const t = st.recent_games.tiles.find((x) => x.name === n);
              return !t || t.art === "image";
            }) && st.recent_games.tiles.some((t) => t.name === "Brawlhalla" && t.art === "image"),
          30_000,
        );
        for (const name of ["Brawlhalla", "Rocket League", "Fall Guys"]) {
          const t = s.recent_games.tiles.find((x) => x.name === name);
          if (t) expect(t.art === "image", `${name} is in the art index and shows art, got ${t.art}`);
        }
        // The list variant is a 48 px square: it draws the icon slot, so the
        // tile holds icon art beside the header art of the grid.
        const brawl = s.recent_games.tiles.find((x) => x.name === "Brawlhalla");
        expect(brawl?.has_header === true, "Brawlhalla holds header art for the grid");
        await app.waitFor("icon art on the list variant", (st) => st.recent_games.tiles.some((t) => t.name === "Brawlhalla" && t.has_icon), 30_000);
        await app.checkpoint("two-feed-art");
      });
    }

    await step("alice sees bob as a join card in the grid, bob does not see his own join", async () => {
      const a = await alice.waitFor("the join card", (st) => st.joined.length > 0, 30_000);
      expect(a.joined.length === 1, `one join card, got ${a.joined.length}`);
      const j = a.joined[0];
      expect(/ joined$/.test(j.title), `the card says who joined: "${j.title}"`);
      expect(/^invited by /.test(j.invited), `the card names the inviter: "${j.invited}"`);
      expect(/^(Today|Yesterday) \d\d:\d\d$/.test(j.when), `the time reads Today hh:mm: "${j.when}"`);
      expect(j.index > 0, `the card is in the grid flow, not the first card, got index ${j.index}`);
      const sessions = a.feed_cards.map((c, i) => (c.card_type === "game-session" ? i : -1)).filter((i) => i >= 0);
      expect(sessions.every((i) => j.index < i), "the card stands before the sessions");
      expect((await bob.state()).joined.length === 0, "bob does not see his own join");
    });

    await step("alice's Say hi sends a greeting that mentions bob, and the button reads Said hi", async () => {
      const s0 = await alice.state();
      const name = s0.joined[0].name;
      await alice.click(`Say hi to ${name}`);
      // The click is the intent: the message goes at once and nothing sits in the input.
      const a = await alice.waitFor(
        "the greeting in the chat",
        (st) => st.messages.some((m) => m.sender === s0.user_name && m.text.includes(`@${name}`)),
        15_000,
      );
      expect(!a.chat_input_focused, "the greeting is not a draft in the chat input");
      const said = await alice.find(`Said hi to ${name}`);
      expect(said.enabled === false, "the Said hi button is off");
      await bob.waitFor("the greeting mentions bob", (st) => st.messages.some((m) => m.mentions_me), 15_000);
      await alice.checkpoint("two-feed-say-hi");
    });

    // The server writes stats for the account that ends the session, so only
    // alice has them. The app loads them when it selects the crew, which was
    // before the session ended: a restart selects the crew again. The strip
    // shows Brawlhalla with its square icon from the art Worker.
    await step("alice sees the Worker icon on the stats strip", async () => {
      await alice.restart();
      const s = await alice.waitFor("the stats strip with an icon", (st) => st.you_strip.has_stats && st.you_strip.art === "image", 45_000);
      expect(s.you_strip.game_name === "Brawlhalla", `the strip shows Brawlhalla, got ${s.you_strip.game_name}`);
      await alice.checkpoint("two-stats-strip-icon");
    });

    for (const app of [bob, alice]) {
      // One session of one member, no clip and no belt move: the day adds
      // nothing to the weekly recap, so the server sends no daily recap
      // (CLIPS.md section 7, dailyRecapHasContent).
      await step(`${app.name} sees no daily recap for a day that adds nothing, and no catch-up card`, async () => {
        const s = await waitForFullFeed(app, { skeletons: 3 });
        expect(!s.daily_recap.present, "no daily recap for one session of one member");
        expect(!s.feed_cards.some((c) => c.card_type === "daily-recap"), "no daily recap card");
        expect(!s.feed_cards.some((c) => c.card_type === "catchup"), "no catch-up card");
      });
    }

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
