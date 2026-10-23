// FEED-08: a game in progress is a live card in the crew feed (mockup
// cold-start frames 5 and 8, next-gen-feed stage C hero order).
//
// bob is a real app. One seeded member plays Rocket League through the same
// presence_update RPC that the client's game sensor uses. bob's own game comes
// through the e2e-only fake sensor (POST /fake-game), which feeds a Started
// event into the real handler path, so presence, the UI event and
// game_session_end all run their normal code. The end of bob's game is long
// enough for the ledger, so the live card gives way to the session card.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bobWithSeededCrew } from "./lib/stage.ts";
import { setPlaying } from "./lib/seed.ts";

const MATE_GAME = { catalogueId: "rocket-league", name: "Rocket League" };
const OWN_GAME_ID = "counter-strike-2";
const OWN_GAME_NAME = "Counter-Strike 2";
// A live card shows from the first sensor report. The server curation
// (backend crew_feed.go, gameSessionQuality) scores only the ended session
// card, so a short solo session leaves no card when it ends.
const OWN_AGO_MIN = 12;

export default journey({
  id: "feed.live-session",
  flows: ["FEED-08"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 1);
    const mate = members[0];

    await step("a crewmate plays: one live card with the game, the name and since N min", async () => {
      await bob.waitFor("the full crew feed", (s) => s.head2head.present && s.head2head.member_count === 2, 30_000);
      await setPlaying(mate, MATE_GAME, 20);
      const s = await bob.waitFor("the live card of the crewmate", (st) => st.live_sessions.length === 1, 20_000);
      const live = s.live_sessions[0];
      expect(live.game_name === MATE_GAME.name, `the game is ${MATE_GAME.name}, got ${live.game_name}`);
      expect(live.member_names.length === 1 && live.member_names[0] === mate.name, `the player is the crewmate, got ${live.member_names.join(",")}`);
      expect(live.since_min >= 20 && live.since_min <= 22, `since 20 min, got ${live.since_min}`);
      expect(live.octagons === 1, `one octagon, got ${live.octagons}`);
      // Stage C hero order: a hero keeps its place, otherwise the live card leads.
      const lead = s.feed_cards[0];
      const heroLeads = s.head2head.placement === "versus";
      expect(live.index === (heroLeads ? 1 : 0), `the live card is at ${heroLeads ? "the first side cell" : "the lead"}, got index ${live.index} of ${s.feed_cards.map((c) => c.card_type).join(",")}`);
      expect(heroLeads || lead.card_type === "live-session", `the lead is the live card, got ${lead.card_type}`);
    });

    await step("bob plays too: a second card, and the pip counts from the real start", async () => {
      await bob.fakeGame("start", OWN_GAME_ID, OWN_AGO_MIN);
      const s = await bob.waitFor("two live cards", (st) => st.live_sessions.length === 2, 20_000);
      const own = s.live_sessions.find((l) => l.game_name === OWN_GAME_NAME);
      expect(own !== undefined, `bob's game is a card, got ${s.live_sessions.map((l) => l.game_name).join(",")}`);
      expect(own!.member_names.length === 1 && own!.member_names[0] === s.user_name, `the player is bob, got ${own!.member_names.join(",")}`);
      expect(own!.since_min >= OWN_AGO_MIN && own!.since_min <= OWN_AGO_MIN + 2, `since ${OWN_AGO_MIN} min, got ${own!.since_min}`);
      expect(own!.since_text === `since ${OWN_AGO_MIN} min`, `the card reads "since ${OWN_AGO_MIN} min", got "${own!.since_text}"`);
      expect(s.live_sessions.every((l) => l.index <= 2), "both cards are on the first screen");
      // No red but the pip: the card paints no red fill. The state port cannot
      // read colours, the checkpoint shows it.
      await bob.checkpoint("live-before-end");
    });

    await step("the crewmate stops: that card goes", async () => {
      await setPlaying(mate, null);
      const s = await bob.waitFor("one live card left", (st) => st.live_sessions.length === 1, 20_000);
      expect(s.live_sessions[0].game_name === OWN_GAME_NAME, `bob's card stays, got ${s.live_sessions[0].game_name}`);
    });

    await step("bob's game ends: the live card goes and is not doubled by a session card", async () => {
      await bob.fakeGame("end", OWN_GAME_ID);
      const s = await bob.waitFor("no live card", (st) => st.live_sessions.length === 0, 45_000);
      // A 12 min solo session scores below the server floor, so the ledger card
      // may be curated away. It must never appear twice.
      const sessions = s.feed_cards.filter((c) => c.card_type === "game-session");
      expect(sessions.length <= 1, `at most one session card, got ${sessions.length}`);
      await bob.checkpoint("live-after-end");
    });
  },
});
