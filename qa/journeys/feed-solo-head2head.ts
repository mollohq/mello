// FEED-03: a user alone in a new crew sees the solo feed: the Stage with the
// viewer's own chips, the solo board with the games of this PC, and the
// People zone with the first five: the viewer and four open seats with the
// red invite. Spec features/CREW-FEED-ZONES.md.
//
// Sharing is on, the onboarding default, so the library report runs and the
// server returns the viewer's own values.
//
// Day 0 reveal (specs/features/SEALED-REVEAL.md section 9): the own chips of a
// new crew arrive sealed, each under its own key. The control is labelled
// "Sealed belt, open it", the same label as a belt that moved. A click opens
// one chip in the centre with the face "You", and the return leaves that chip
// open in its slot.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

/** The names that a TitleChip draws, upper case (client/src/head2head.rs). */
const TITLE_NAMES = ["THE CARRY", "NIGHT OWL", "THE COLLECTOR", "LOCKED IN", "HAS A LIFE", "SAMPLER", "ONE TRICK"];

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

    await step("the Stage has the own chips, the board is solo, four seats are open", async () => {
      // The feed shows before the library report returns the own values and
      // the games, so wait for them.
      const s = await bob.waitFor(
        "the own chips and the solo games",
        (st) => st.feed_zones.stage === "own" && st.feed_zones.own_chips.length > 0 && st.feed_zones.solo_games.length > 0,
        15_000,
      );
      const z = s.feed_zones;
      expect(z.board === "solo", `the solo board, got ${z.board}`);
      expect(z.belts.length === 0, `no contested belt, got ${JSON.stringify(z.belts)}`);
      expect(z.people.members === 1 && z.people.open_seats === 4, `one octagon and four seats, got ${z.people.members} and ${z.people.open_seats}`);
      expect(z.own_chips.length <= 4, `four own chips at most, got ${z.own_chips.length}`);
      for (const c of z.own_chips) expect(c.value !== "", `${c.title} has a value`);
    });

    /** The texts on screen, one entry for each place. The element query can list an element twice. */
    const screenTexts = async (): Promise<string[]> => {
      const seen = new Set<string>();
      const els = (await bob.ui.textElements()).filter((e) => {
        const key = `${e.text}@${e.x},${e.y}`;
        if (seen.has(key)) return false;
        seen.add(key);
        return true;
      });
      return els.map((e) => e.text);
    };
    let chips = 0;

    await step("the own chips arrive sealed", async () => {
      const s = await bob.state();
      chips = s.feed_zones.own_chips.length;
      expect(s.feed_zones.own_chips.every((c) => c.sealed), "every own chip is sealed on day 0");
      expect(s.sealed.own === chips, `sealed.own is ${chips}, got ${s.sealed.own}`);
      expect(s.sealed.belts === 0, `no belt that moved is sealed, got ${s.sealed.belts}`);
      const texts = await screenTexts();
      expect(texts.filter((t) => TITLE_NAMES.includes(t)).length === 0, "a sealed chip draws no title");
      expect(texts.includes("Go head2head with your crew"), "the line under the chips shows");
      await bob.checkpoint("solo-feed-sealed");
    });

    await step("a click on one sealed own chip opens it in the centre and the face reads You", async () => {
      await bob.click("Sealed belt, open it");
      const s = await bob.waitFor("the own chip in the centre", (st) => st.sealed.open_center === "belt", 5_000);
      expect(s.sealed.own === chips - 1, `the chip in the centre is no longer sealed in the feed, got ${s.sealed.own}`);
      await bob.checkpoint("own-reveal-start");
      // The face shows from 1.03 s and the card rests from 1.25 s.
      await bob.waitFor("the card at rest, the face shown", (st) => st.sealed.resting, 5_000);
      const texts = await screenTexts();
      expect(texts.includes("You"), `the face names the viewer as You: ${texts.join(" | ")}`);
      expect(!texts.includes("took it from"), "an own value took nothing from anyone");
      await bob.checkpoint("own-reveal-face");
    });

    await step("a click on the scrim sends the chip back open, the other chips stay sealed", async () => {
      await bob.click("Close");
      const s = await bob.waitFor("the chip landed, open", (st) => st.sealed.open_center === "none", 10_000);
      expect(s.sealed.own === chips - 1, `${chips - 1} own chips are still sealed, got ${s.sealed.own}`);
      const open = s.feed_zones.own_chips.filter((c) => !c.sealed);
      expect(open.length === 1, `one chip is open, got ${JSON.stringify(s.feed_zones.own_chips)}`);
      const texts = await screenTexts();
      const drawn = texts.filter((t) => TITLE_NAMES.includes(t));
      expect(drawn.length === 1, `the opened chip draws its title in the slot, got ${drawn.join(", ")}`);
      expect(texts.includes(open[0].value), `the opened chip draws its value ${open[0].value}`);
      await bob.checkpoint("own-chip-open-in-its-slot");
    });

    await step("the viewer octagon wears the avatar the app loaded, not initials", async () => {
      const s = await bob.waitFor("the avatar on the viewer octagon", (st) => st.feed_zones.you_avatar, 20_000);
      expect(s.feed_zones.you_avatar, "the octagon shows an image");
    });

    await step("the invite button is a control", async () => {
      const c = await bob.find("Invite your crew");
      expect(c.label === "Invite your crew", `label is "Invite your crew", got "${c.label}"`);
    });

    await step("the solo board lists the games of this PC", async () => {
      const s = await bob.state();
      expect(s.feed_zones.solo_games.every((g) => g !== ""), "every game has a name");
      expect(s.feed_zones.solo_games.length <= 4, `four games at most, got ${s.feed_zones.solo_games.length}`);
      await bob.checkpoint("solo-feed");
    });
  },
});
