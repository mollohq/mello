// FEED-11: today on the board. Three members play today: the Today tab of
// the board counts the sessions, the members and the top game, and the
// Sessions zone holds the sessions as cards. Spec features/CREW-FEED-ZONES.md.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";
import { endSession, profilesFrom, seedCrew, signInEmail } from "./lib/seed.ts";
import { readInviteCode } from "./lib/setup.ts";

export default journey({
  id: "feed.day-card",
  flows: ["FEED-11"],
  async run(ctx) {
    const { step, expect, runId, launch } = ctx;
    const crew = `Day ${runId}`;
    const bob = await launch("bob");
    let code = "";
    let token = "";
    let crewId = "";

    await step(
      "setup: bob creates a crew and reads the invite link",
      async () => {
        const creds = await onboardWithNewCrew(
          bob,
          crew,
          `bob${runId}`,
          "Public",
        );
        await bob.waitFor(
          "the solo board of a crew of one",
          (s) => s.feed_zones.board === "solo",
          30_000,
        );
        code = await readInviteCode(bob);
        token = await signInEmail(creds.email, creds.password);
        crewId = (await bob.state()).active_crew_id;
      },
    );

    await step(
      "setup: two members join over REST and three people play today",
      async () => {
        // The first two profiles post no clips, so the sessions make the day.
        const members = await seedCrew(code, profilesFrom(0, 2, runId), runId);
        const played = [
          [token, { id: "steam-730", name: "Counter-Strike 2" }, 95, 21 * 60],
          [
            members[0].token,
            { id: "steam-730", name: "Counter-Strike 2" },
            60,
            21 * 60,
          ],
          [
            members[1].token,
            { id: "steam-252950", name: "Rocket League" },
            40,
            20 * 60,
          ],
        ] as const;
        for (let i = 0; i < played.length; i++) {
          const [t, game, min, endLocal] = played[i];
          await endSession(
            t,
            crewId,
            game,
            min,
            endLocal,
            `day-${runId}-${i}`,
            0,
          );
        }
      },
    );

    await step(
      "today on the board: the sessions, the members and the top game",
      async () => {
        await bob.restart();
        const s = await bob.waitFor(
          "today with three sessions",
          (st) =>
            !st.feed_zones.today.empty && st.feed_zones.today.sessions >= 3,
          45_000,
        );
        const t = s.feed_zones.today;
        expect(t.on >= 3, `three members took part, got ${t.on}`);
        expect(t.hangout !== "", "the hangout time shows");
        expect(
          t.top_game === "Counter-Strike 2",
          `the top game of the day, got "${t.top_game}"`,
        );
        const games = s.feed_zones.sessions.filter(
          (c) => c.card_type === "game-session",
        );
        expect(
          games.length >= 1,
          `the sessions of the day are cards in the Sessions zone, got ${s.feed_zones.sessions.map((c) => c.card_type).join(",")}`,
        );
        await bob.checkpoint("day-today");
      },
    );
  },
});
