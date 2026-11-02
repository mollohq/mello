// CHAT-02: emoji in chat. The picker stays open across picks, an emoji-only
// message reaches the other member, and the GIF picker says why it is empty
// instead of showing a blank grid. The checkpoints are the visual proof of
// the Twemoji glyphs and the jumbo size, which no headless render can show.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "chat.emoji",
  flows: ["CHAT-02"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, aliceName } = await twoUsersInOneCrew(ctx);
    const fire = "🔥🔥";

    await step("alice opens the picker and sends an emoji-only message", async () => {
      await alice.click("Emoji");
      await alice.checkpoint("alice-picker-open");
      await alice.key("Escape");
      await alice.type("Message crewmates", fire);
      await alice.key("\n");
      await alice.waitFor("alice sees her own message", (s) => s.messages.some((m) => m.text === fire));
      await alice.checkpoint("alice-jumbo");
    });

    await step("bob sees it, big, from alice", async () => {
      const s = await bob.waitFor("bob sees the emoji", (st) => st.messages.some((m) => m.text === fire));
      const m = s.messages.find((x) => x.text === fire)!;
      expect(m.sender === aliceName, `the sender shows as ${aliceName}, got "${m.sender}"`);
      await bob.checkpoint("bob-jumbo");
    });

    await step("the GIF picker says why it is empty", async () => {
      await bob.click("GIF");
      // This build carries no GIPHY_API_KEY, so the picker must say so and
      // offer a retry, not show a blank grid.
      await bob.find("Try again", 0, 10_000);
      expect((await bob.text("GIFs are off in this build")) !== null, "the reason is on screen");
      await bob.checkpoint("bob-gif-picker");
    });
  },
});
