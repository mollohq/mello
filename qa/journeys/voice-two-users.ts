// VOICE-01, VOICE-02, VOICE-04: two members join the same voice channel, see
// each other, one mutes, then leaves. Each side checks what the other sees.
// Both calls run through the local SFU. A P2P call fails the journey.

import { voiceMembers } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";
import { joinVoice } from "./lib/voice.ts";

const CHANNEL = "General";

export default journey({
  id: "voice.two-users",
  flows: ["VOICE-01", "VOICE-02", "VOICE-04"],
  voice: true,
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, aliceName, bobName } = await twoUsersInOneCrew(ctx);

    await step(`alice joins ${CHANNEL} through the SFU`, async () => {
      await joinVoice(alice, CHANNEL);
      await alice.waitFor(`alice listed in ${CHANNEL}`, (s) => voiceMembers(s, CHANNEL).includes(aliceName));
    });

    await step(`bob sees alice in ${CHANNEL}`, async () => {
      await bob.waitFor(`alice listed in ${CHANNEL} on bob`, (s) => voiceMembers(s, CHANNEL).includes(aliceName));
    });

    await step(`bob joins ${CHANNEL} through the SFU, both see both`, async () => {
      await joinVoice(bob, CHANNEL);
      for (const u of [alice, bob]) {
        await u.waitFor(`${u.name} sees both in ${CHANNEL}`, (s) => {
          const m = voiceMembers(s, CHANNEL);
          return m.includes(aliceName) && m.includes(bobName);
        });
      }
      await alice.checkpoint("both-in-voice");
    });

    await step("alice mutes, bob sees her muted", async () => {
      await alice.click("Mute");
      await alice.waitFor("alice muted", (s) => s.mic_muted);
      await bob.waitFor("alice shows muted on bob", (s) =>
        s.voice_channels.some((c) => c.members.some((m) => m.name === aliceName && m.muted)),
      );
    });

    await step("alice leaves voice, bob sees her gone", async () => {
      await alice.click("Leave voice");
      await alice.waitFor("alice out of voice", (s) => !s.in_voice);
      await bob.waitFor("alice gone from the channel on bob", (s) => !voiceMembers(s, CHANNEL).includes(aliceName));
      await bob.checkpoint("alice-left");
    });

    await step("no hidden errors on either side", async () => {
      for (const u of [alice, bob]) {
        const errors = (await u.events()).filter((e) => e.message).map((e) => e.message);
        expect(errors.length === 0, `${u.name} has no Error events, got: ${errors.join(" | ")}`);
      }
    });
  },
});
