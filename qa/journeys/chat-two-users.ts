// CHAT-01: two members of one crew exchange messages, and each sees the
// other's message with the sender's name.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export default journey({
  id: "chat.two-users",
  flows: ["CHAT-01"],
  // #84: a new member's messages show their random username.
  knownIssues: [84],
  async run(ctx) {
    const { runId, step, expect } = ctx;
    const { alice, bob, aliceName, bobName } = await twoUsersInOneCrew(ctx);
    const hello = `hello from alice ${runId}`;
    const reply = `hi alice, bob here ${runId}`;

    await step("alice sends a message", async () => {
      await alice.type("Message crewmates", hello);
      await alice.key("\n");
      await alice.waitFor("alice sees her own message", (s) => s.messages.some((m) => m.text === hello));
    });

    await step("bob receives it, from alice", async () => {
      const s = await bob.waitFor("bob sees alice's message", (st) => st.messages.some((m) => m.text === hello));
      const m = s.messages.find((x) => x.text === hello)!;
      expect(m.sender === aliceName, `the sender shows as ${aliceName}, got "${m.sender}"`);
      await bob.checkpoint("bob-received");
    });

    await step("bob replies, alice receives it", async () => {
      await bob.type("Message crewmates", reply);
      await bob.key("\n");
      const s = await alice.waitFor("alice sees bob's reply", (st) => st.messages.some((m) => m.text === reply));
      const m = s.messages.find((x) => x.text === reply)!;
      expect(m.sender === bobName, `the sender shows as ${bobName}, got "${m.sender}"`);
      await alice.checkpoint("alice-received");
    });

    await step("no hidden errors on either side", async () => {
      for (const u of [alice, bob]) {
        const errors = (await u.events()).filter((e) => e.message).map((e) => e.message);
        expect(errors.length === 0, `${u.name} has no Error events, got: ${errors.join(" | ")}`);
      }
    });
  },
});
