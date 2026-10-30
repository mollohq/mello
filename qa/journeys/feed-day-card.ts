// FEED-11: the day card. A daily recap with sessions to list is two columns
// wide with the day's sessions as rows (spec 19 section 3.5a, mockup frame H).
// A voice session is a row there, never a card.

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
          "the head2head card of a crew of one",
          (s) => s.head2head.present,
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
      "the day card: today so far, two columns, the sessions as rows",
      async () => {
        await bob.restart();
        const s = await bob.waitFor(
          "the day card",
          (st) => st.daily_recap.present && st.daily_recap.wide,
          45_000,
        );
        const d = s.daily_recap;
        expect(
          d.day === "Today so far",
          `the day that is not over, got "${d.day}"`,
        );
        expect(
          d.rows >= 1 && d.rows <= 2,
          `one or two session rows on the card, got ${d.rows}`,
        );
        expect(d.sessions >= 3, `three sessions counted, got ${d.sessions}`);
        expect(d.members >= 3, `three members took part, got ${d.members}`);
        expect(
          s.feed_cards.filter((c) => c.card_type === "daily-recap").length ===
            1,
          "one daily recap card",
        );
        expect(
          s.feed_cards.some((c) => c.card_type === "recent-games"),
          "the recent games card stays",
        );
        await bob.checkpoint("day-card");
      },
    );
  },
});
