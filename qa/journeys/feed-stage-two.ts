// FEED-04: two members. alice and bob are real apps on one machine, so both
// report the same library. Seeding gives alice games of her own, so the two
// libraries differ and titles are contested. The board holds the belts, the
// People zone has the first five with three open seats and the red invite,
// and the Sessions zone has alice's session with Worker art. alice sees that
// bob joined by her invite link, with Say hi. The art Worker serves
// Brawlhalla, Rocket League and Fall Guys (qa/fixtures/art-index). Start the
// Worker first: scripts/e2e-art-worker.sh up. Spec features/CREW-FEED-ZONES.md.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { expectFullFeed, waitForFullFeed } from "./lib/stage.ts";
import { endSession, reportLibrary, signInEmail } from "./lib/seed.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "feed.stage-two",
  flows: ["FEED-04"],
  needs: ["art"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, aliceCreds } = await twoUsersInOneCrew(ctx, "Public");
    const crewId = (await alice.state()).active_crew_id;

    await step("alice reports games of her own and one session, so the two libraries differ", async () => {
      // A report from alice replaces the one of her app: do it after the apps
      // reported, when the board has the belts of their libraries. The server
      // pushes "feed changed" for the library report and the session end, and
      // both open apps load the feed again (at most once every 5 s).
      await Promise.all([alice, bob].map((a) => a.waitFor("the belts of the app libraries", (s) => s.feed_zones.belts.length > 0, 30_000)));
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
      await step(`${app.name} sees the first five with three open seats, and contested belts`, async () => {
        const s = await app.waitFor(
          "two octagons, three open seats and belts",
          (st) => st.feed_zones.people.members === 2 && st.feed_zones.people.open_seats === 3 && st.feed_zones.belts.length > 0,
          40_000,
        );
        const z = s.feed_zones;
        expect(z.people.shown, "the People zone shows");
        expect(z.belts.every((t) => t.holder_name !== ""), "every holder is on the member list");
        expect((await app.find("Invite your crew")).role === "Button", "the red invite is a control");
      });
    }

    for (const app of [bob, alice]) {
      await step(`${app.name} sees alice's session in the Sessions zone with Worker art`, async () => {
        const s = await app.waitFor(
          "the Brawlhalla session with art",
          (st) => st.feed_zones.sessions.some((c) => c.game_name === "Brawlhalla" && c.art === "image"),
          45_000,
        );
        const c = s.feed_zones.sessions.find((x) => x.game_name === "Brawlhalla")!;
        expect(c.card_type === "game-session", `a game session card, got ${c.card_type}`);
        await app.checkpoint("two-feed-session");
      });
    }

    await step("alice sees that bob joined, bob does not see his own join", async () => {
      const a = await alice.waitFor("the join", (st) => st.feed_zones.people.joined.length > 0, 30_000);
      const p = a.feed_zones.people;
      expect(p.joined.length === 1, `one join, got ${p.joined.join(",")}`);
      expect(p.joined_line === `${p.joined[0]} joined`, `the line says who joined: "${p.joined_line}"`);
      expect(p.say_hi, "Say hi shows");
      expect((await bob.state()).feed_zones.people.joined.length === 0, "bob does not see his own join");
    });

    await step("alice's Say hi sends a greeting that mentions bob, and the button goes", async () => {
      const s0 = await alice.state();
      const name = s0.feed_zones.people.joined[0];
      await alice.click(`Say hi to ${name}`);
      // The click is the intent: the message goes at once and nothing sits in the input.
      const a = await alice.waitFor(
        "the greeting in the chat",
        (st) => st.messages.some((m) => m.sender === s0.user_name && m.text.includes(`@${name}`)),
        15_000,
      );
      expect(!a.chat_input_focused, "the greeting is not a draft in the chat input");
      await alice.waitFor("the Say hi button goes", (st) => !st.feed_zones.people.say_hi, 5_000);
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
      // Two members, no clip: the day adds to the week only when a belt moved
      // today (dailyRecapHasContent). Every move of this new crew is today, so
      // Today shows exactly when a belt on the board is sealed.
      await step(`${app.name} sees today on the board exactly when a belt moved`, async () => {
        const s = await app.waitFor(
          "today agrees with the belt moves",
          (st) => st.feed_zones.belts.some((b) => b.sealed) === !st.feed_zones.today.empty,
          30_000,
        );
        const t = s.feed_zones.today;
        if (!t.empty) {
          expect(t.hangout !== "", "the hangout time shows");
          expect(t.sessions >= 1, `the session of the day, got ${t.sessions}`);
        }
      });
    }

    await step("no zone card is empty", async () => {
      for (const app of [bob, alice]) {
        await waitForFullFeed(app);
        expectFullFeed(await app.state(), expect);
        await app.checkpoint("two-feed");
      }
    });
  },
});
