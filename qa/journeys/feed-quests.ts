// FEED-13: crew quests. A new crew gets the quest card on the Stage. The
// owner opens the first gift and sets a game art banner, opens the second and
// sets a custom invite link, which a third user joins through. A call on the
// local SFU unlocks crew emoji: a member adds one, sends it, and the owner
// reacts with it. Plans: crew-quests.md.

import { readFileSync } from "node:fs";

import type { App } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardFromInvite } from "./lib/onboarding.ts";
import { selectCrew, twoUsersInOneCrew } from "./lib/setup.ts";
import { joinVoice } from "./lib/voice.ts";

const CHANNEL = "General";
const GAME = "Brawlhalla";
const EMOJI = "gg";
const EMOJI_PNG = readFileSync(new URL("../fixtures/crew-emoji.png", import.meta.url));

type Gift = { label: string; state: "locked" | "ready" | "open"; can_open: boolean };

async function gifts(app: App): Promise<Gift[]> {
  return (await app.state()).feed_zones.quests?.gifts ?? [];
}

/** Open the first ready gift in the centre and wait for its face at rest. */
async function openGift(app: App, step: string): Promise<void> {
  await app.scrollToControl("Sealed gift, open it");
  await app.click("Sealed gift, open it");
  await app.waitFor(`${step}: the gift in the centre`, (s) => s.sealed.open_center === "quest", 5_000);
  await app.waitFor(`${step}: the face at rest`, (s) => s.sealed.resting, 5_000);
}

export default journey({
  id: "feed.quests",
  flows: ["FEED-13"],
  needs: ["art"],
  voice: true,
  async run(ctx) {
    const { step, expect, launch, runId } = ctx;
    const { alice, bob, crew, bobName } = await twoUsersInOneCrew(ctx);
    const link = `q${runId}`.toLowerCase().replace(/[^a-z0-9-]/g, "").slice(0, 24);

    await step("the new crew shows the quest card, the owner can open two gifts", async () => {
      const s = await alice.waitFor(
        "the card at 2 / 5 after bob joined",
        (st) => st.feed_zones.stage === "quests" && st.feed_zones.quests?.count === "2 / 5",
        20_000,
      );
      const g = s.feed_zones.quests!.gifts;
      expect(g.length === 5, `five gifts, got ${g.length}`);
      expect(g[0].state === "ready" && g[0].can_open, `gift 1 ready for the owner, got ${JSON.stringify(g[0])}`);
      expect(g[1].state === "ready" && g[1].can_open, `gift 2 ready for the owner, got ${JSON.stringify(g[1])}`);
      expect(g.slice(2).every((x) => x.state === "locked"), "gifts 3 to 5 locked");
      expect(g.every((x) => x.label.startsWith("DROP")), `sealed gifts are DROP 1 to 5, got ${g.map((x) => x.label)}`);
      const b = await bob.waitFor("bob sees the card", (st) => st.feed_zones.stage === "quests", 20_000);
      expect(b.feed_zones.quests!.gifts.every((x) => !x.can_open), "a member opens no gift");
      await alice.checkpoint("quest-card");
    });

    await step(`alice opens gift 1 and sets ${GAME} as the banner`, async () => {
      await openGift(alice, "gift 1");
      await alice.checkpoint("banner-face");
      await alice.click("Game art");
      await alice.waitFor("the banner picker", (s) => s.open_modals.includes("banner_picker"));
      await alice.type("Search a game", GAME);
      await alice.click(GAME);
      await alice.click("Save");
      const s = await alice.waitFor(
        "the crew header draws the banner",
        (st) => st.crew_gifts.banner_game !== "" && st.crew_gifts.banner_shown,
        30_000,
      );
      expect(!s.open_modals.includes("banner_picker"), "the picker closed");
      expect((await gifts(alice))[0]?.label === "BANNER", "gift 1 shows its name once open");
      await alice.checkpoint("banner-in-header");
    });

    await step("bob sees the banner after a restart", async () => {
      await bob.restart();
      await selectCrew(bob, crew);
      await bob.waitFor("bob's header draws the banner", (s) => s.crew_gifts.banner_shown, 30_000);
    });

    await step(`alice opens gift 2 and sets the link ${link}`, async () => {
      await openGift(alice, "gift 2");
      await alice.click("Set the link");
      await alice.waitFor("the invite name screen", (s) => s.open_modals.includes("invite_name"));
      await alice.type("Invite name", link);
      await alice.click("Save");
      await alice.waitFor(`the crew's invite name is ${link}`, (s) => s.crew_gifts.invite_name === link, 15_000);
    });

    await step(`cara joins through mello://join/${link}`, async () => {
      const cara = await launch("cara", `mello://join/${link}`);
      await onboardFromInvite(cara, crew, `cara${runId}`);
      await cara.waitFor(`cara's crews include ${crew}`, (st) => st.crews.includes(crew), 15_000);
    });

    await step(`alice and bob call in ${CHANNEL}: gift 3 is ready`, async () => {
      await joinVoice(alice, CHANNEL);
      await joinVoice(bob, CHANNEL);
      await alice.waitFor(
        "gift 3 ready for the owner",
        (s) => s.feed_zones.quests?.gifts[2]?.state === "ready" && s.feed_zones.quests!.gifts[2].can_open,
        20_000,
      );
      await alice.checkpoint("call-done");
    });

    await step("alice opens gift 3, bob adds a crew emoji from the picker", async () => {
      await openGift(alice, "gift 3");
      await alice.checkpoint("emoji-face");
      await alice.click("Close");
      await alice.waitFor("the gift back in its slot", (s) => s.sealed.open_center === "none", 10_000);

      bob.setPickFile(EMOJI_PNG);
      await bob.waitFor("the emoji gift is open on bob", (s) => s.crew_gifts.emoji_open, 15_000);
      await bob.click("Emoji");
      await bob.click("Add emoji");
      await bob.waitFor("the add emoji screen", (s) => s.open_modals.includes("crew_emoji"));
      await bob.click("Choose image");
      await bob.type("Emoji name", EMOJI);
      await bob.click("Save");
      await bob.waitFor(`:${EMOJI}: is a crew emoji`, (s) => s.crew_gifts.emoji_loaded.includes(EMOJI), 20_000);
      await alice.waitFor(`alice has :${EMOJI}:`, (s) => s.crew_gifts.emoji_loaded.includes(EMOJI), 20_000);
    });

    await step(`bob sends :${EMOJI}:, it draws big, alice reacts with it`, async () => {
      await bob.key("Escape");
      await bob.type("Message crewmates", `:${EMOJI}:`);
      await bob.key("\n");
      const s = await alice.waitFor(
        "alice sees the crew emoji message",
        (st) => st.messages.some((m) => m.text === `:${EMOJI}:` && m.crew_emoji === 1),
        20_000,
      );
      const m = s.messages.find((x) => x.text === `:${EMOJI}:`)!;
      expect(m.sender === bobName, `the sender is ${bobName}, got ${m.sender}`);
      await alice.clickInRow("Message actions", `:${EMOJI}:`);
      await alice.clickInRow("React", `:${EMOJI}:`);
      await alice.click(`:${EMOJI}:`);
      for (const u of [alice, bob]) {
        await u.waitFor(
          `${u.name} sees the crew emoji reaction`,
          (st) => st.messages.some((x) => x.text === `:${EMOJI}:` && x.image_reactions === 1),
          15_000,
        );
      }
      await alice.checkpoint("crew-emoji-reaction");
    });

    await step("no hidden errors on either side", async () => {
      for (const u of [alice, bob]) {
        const errors = (await u.events()).filter((e) => e.message).map((e) => e.message);
        expect(errors.length === 0, `${u.name} has no Error events, got: ${errors.join(" | ")}`);
      }
    });
  },
});
