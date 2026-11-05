// FEED-08: a game in progress is a live card in the Sessions zone. The live
// cards lead the zone, before the sessions from the server. Poster art comes
// from the local Steam library (Brawlhalla on this Mac) or from the art
// Worker (qa/fixtures/art-index serves Rocket League a hero image). The
// journey declares the art need, so the crewmate's Rocket League card must
// have a poster. Spec features/CREW-FEED-ZONES.md.
//
// bob is a real app. One seeded member plays Rocket League through the same
// presence_update RPC that the client's game sensor uses. bob's own game comes
// through the e2e-only fake sensor (POST /fake-game), which feeds a Started
// event into the real handler path, so presence, the UI event and
// game_session_end all run their normal code.

import type { AppState } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bobWithSeededCrew } from "./lib/stage.ts";
import { setPlaying } from "./lib/seed.ts";

const MATE_GAME = { catalogueId: "rocket-league", name: "Rocket League" };
const OWN_GAME_ID = "brawlhalla";
const OWN_GAME_NAME = "Brawlhalla";
// A live card shows from the first sensor report. The server scores only the
// ended session, so a short solo session may leave no session card.
const OWN_AGO_MIN = 12;

/** The live cards lead the Sessions zone: places 0 to n-1, in order. */
function leadProblems(s: AppState): string[] {
  const kinds = s.feed_zones.sessions.map((c) => c.card_type).join(",");
  const at = s.live_sessions.map((l) => l.index).sort((a, b) => a - b);
  return at.every((i, n) => i === n) ? [] : [`the live cards lead the Sessions zone, got places ${at.join(",")} of ${kinds}`];
}

export default journey({
  id: "feed.live-session",
  flows: ["FEED-08"],
  needs: ["art"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 1);
    const mate = members[0];

    await step("a crewmate plays: one live card with the game, the name and since N min", async () => {
      await bob.waitFor("the crew of two", (s) => s.feed_zones.people.members === 2, 30_000);
      await setPlaying(mate, MATE_GAME, 20);
      // The Worker art loads after the card shows, so wait for the poster too.
      const s = await bob.waitFor("the live card of the crewmate with the Worker poster", (st) => st.live_sessions.length === 1 && st.live_sessions[0].poster, 30_000);
      const live = s.live_sessions[0];
      expect(live.game_name === MATE_GAME.name, `the game is ${MATE_GAME.name}, got ${live.game_name}`);
      expect(live.member_names.length === 1 && live.member_names[0] === mate.name, `the player is the crewmate, got ${live.member_names.join(",")}`);
      expect(live.since_min >= 20 && live.since_min <= 22, `since 20 min, got ${live.since_min}`);
      expect(live.octagons === 1, `one octagon, got ${live.octagons}`);
      // The art Worker is up, so Rocket League has a hero image and the card
      // has a poster. This proves the Worker path for the live card.
      expect(live.poster, `the art Worker serves Rocket League, so the card has a poster, got ${JSON.stringify(live)}`);
      const bad = leadProblems(s);
      expect(bad.length === 0, bad.join("; "));
      await bob.checkpoint("live-one");
    });

    await step("bob plays too: a second card, and the pip counts from the real start", async () => {
      await bob.fakeGame("start", OWN_GAME_ID, OWN_AGO_MIN);
      const s = await bob.waitFor("two live cards with posters", (st) => st.live_sessions.length === 2 && st.live_sessions.every((l) => l.poster), 30_000);
      const own = s.live_sessions.find((l) => l.game_name === OWN_GAME_NAME);
      expect(own !== undefined, `bob's game is a card, got ${s.live_sessions.map((l) => l.game_name).join(",")}`);
      expect(own!.member_names.length === 1 && own!.member_names[0] === s.user_name, `the player is bob, got ${own!.member_names.join(",")}`);
      expect(own!.since_min >= OWN_AGO_MIN && own!.since_min <= OWN_AGO_MIN + 2, `since ${OWN_AGO_MIN} min, got ${own!.since_min}`);
      expect(own!.since_text === `since ${OWN_AGO_MIN} min`, `the card reads "since ${OWN_AGO_MIN} min", got "${own!.since_text}"`);
      expect(own!.poster, "bob's game has Steam art on this Mac, so the card has a poster");
      const other = s.live_sessions.find((l) => l.game_name === MATE_GAME.name);
      expect(other !== undefined && other.poster, `the crewmate's card keeps its Worker poster, got ${JSON.stringify(other)}`);
      const bad = leadProblems(s);
      expect(bad.length === 0, bad.join("; "));
      // No red but the pip: the card paints no red fill. The state port cannot
      // read colours, the checkpoint shows it.
      await bob.checkpoint("live-two");
    });

    await step("the crewmate stops: that card goes", async () => {
      await setPlaying(mate, null);
      const s = await bob.waitFor("one live card left", (st) => st.live_sessions.length === 1, 20_000);
      expect(s.live_sessions[0].game_name === OWN_GAME_NAME, `bob's card stays, got ${s.live_sessions[0].game_name}`);
    });

    await step("bob's game ends: the live card goes and one session card takes its place", async () => {
      // The setup gave bob an older Brawlhalla session: count the cards first.
      const ownCards = (st: AppState) =>
        st.feed_zones.sessions.filter((c) => c.card_type === "game-session" && c.game_name === OWN_GAME_NAME).length;
      const before = ownCards(await bob.state());
      await bob.fakeGame("end", OWN_GAME_ID);
      const s = await bob.waitFor("no live card", (st) => st.live_sessions.length === 0, 45_000);
      const after = ownCards(s);
      expect(after <= before + 1, `the game adds one session card at most, got ${before} then ${after}`);
      await bob.checkpoint("live-after-end");
    });
  },
});
