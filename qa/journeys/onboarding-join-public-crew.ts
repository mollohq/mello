// ONB-02: a fresh install joins a public crew at step 1.
//
// The shared stack has hundreds of open crews, and discovery has no order
// yet. The journey therefore does not expect a given crew on step 1. It
// reads the first crew card that step 1 shows and that a user can join,
// picks it, finishes step 2 and step 3, and checks that this crew is
// active in the app. alice makes a public crew first, so the stack has at
// least one open crew that is not full. A full crew's card cannot be picked
// (ONB-06), so the journey picks the first card that offers "JOIN".

import type { App, AppState } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { linkEmailAtStep3, onboardWithNewCrew, profileAtStep2 } from "./lib/onboarding.ts";

/** The controls on step 1 that are not crew cards. A crew card is a button labelled with the crew name. */
const STEP1_CONTROLS = new Set([
  "I already have an account",
  "Sign in",
  "Create your own crew",
  "Open invite",
  "Checking…",
  "Retry",
  "Go to step 1",
  "Go to step 2",
  "Go to step 3",
]);

/**
 * The name on the first crew card of step 1 that a user can join. Cards go
 * top to bottom, then left to right. A full crew's card says "FULL" and does
 * nothing on a click (the seeded crews on the shared stack are full); a card
 * a user can join says "JOIN". The driver reads that word as the user does:
 * it is the first "FULL" or "JOIN" after the crew name in the card's text.
 */
async function firstJoinableCard(app: App): Promise<{ name: string; seen: string[] }> {
  const cards = (await app.controls())
    .filter((c) => c.role === "Button" && !STEP1_CONTROLS.has(c.label) && c.width > 0 && c.height > 0)
    .sort((a, b) => a.y - b.y || a.x - b.x);
  const names = [...new Set(cards.map((c) => c.label))];
  const texts = await app.ui.texts();
  const seen: string[] = [];
  for (const name of names) {
    const at = texts.indexOf(name);
    const mark = texts.slice(at + 1).find((t) => t === "FULL" || t === "JOIN");
    seen.push(`${name}: ${mark ?? "?"}`);
    if (at >= 0 && mark === "JOIN") return { name, seen };
  }
  return { name: "", seen };
}

export default journey({
  id: "onboarding.join-public-crew",
  flows: ["ONB-02"],
  async run({ runId, launch, step, expect }) {
    const aliceCrew = `Open Door ${runId}`;
    const bobName = `bob${runId}`;

    const alice = await launch("alice");
    await step("alice makes a public crew, so the stack has an open crew that is not full", async () => {
      await onboardWithNewCrew(alice, aliceCrew, `alice${runId}`, "Public");
    });

    const bob = await launch("bob");
    let picked = "";
    await step("bob (a fresh install) reads the first crew card on step 1 that offers JOIN", async () => {
      await bob.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
      // A fresh install loads the crew list twice and rebuilds step 1 (#85).
      await bob.settle(["DiscoverCrewsLoaded"]);
      const first = await firstJoinableCard(bob);
      picked = first.name;
      expect(picked !== "", `step 1 shows a crew card that offers JOIN, got: ${first.seen.join("; ")}`);
      await bob.checkpoint("step1");
    });

    await step("bob picks that crew: step 2", async () => {
      await bob.click(picked);
      const s = await bob.waitFor("onboarding step 2", (st) => st.onboarding_step === 2);
      expect(s.onboarding_invite_crew_name === "", `no invite on this path, got "${s.onboarding_invite_crew_name}"`);
      expect(!s.onboarding_invite_path, "three steps: the crew came from step 1, not from an invite");
      expect(await bob.text("STEP 02 / 03"), `step 2 shows "STEP 02 / 03"`);
    });

    await step("bob finishes step 2 and step 3", async () => {
      await profileAtStep2(bob, bobName, 1);
      await linkEmailAtStep3(bob, bobName);
    });

    await step("bob is in the app, and the picked crew is his only crew and the active one", async () => {
      // The active crew is checked by its id, not by active_crew_name: that
      // name stays empty after onboarding (a separate bug in the name that
      // Discover shows on "BACK TO"). With one crew, an active crew id and
      // bob in its member list, the active crew is the picked crew.
      const inCrew = (s: AppState) => s.crews.length > 0 && s.active_crew_id !== "" && s.members.includes(bobName);
      const s = await bob.waitFor(`${picked} is bob's active crew`, inCrew, 20_000);
      expect(s.crews.length === 1 && s.crews[0] === picked, `only the picked crew ${picked}, got: ${s.crews.join(", ")}`);
      expect(await bob.text(picked), `the crew name ${picked} is on screen`);
      const errors = (await bob.events()).filter((e) => e.type === "Error");
      expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
      await bob.checkpoint("in-app");
    });
  },
});
