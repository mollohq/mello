// Voice channels with two users in one crew.
//
// - createChannel (CREW-05): alice adds a channel through the crew menu; both
//   users see it, and bob joins it.
// - rapidSwitch (VOICE-03): bob clicks three channels back to back. The switch
//   ends in the last channel, both users see bob only there, and no earlier
//   join shows again after the last one (a late replay). A tester saw quick
//   clicks join one after another, seconds later; the cause was a social
//   sign-in wait that blocked the core command loop (#88, fixed in 0.5.8).

import { voiceMembers, type App, type AppState } from "../../tools/mello-driver/src/app.ts";
import { journey, type JourneyContext } from "../../tools/mello-driver/src/journey.ts";
import { addVoiceChannel, waitForChannels } from "./lib/channels.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";

export const createChannel = journey({
  id: "voice.create-channel",
  flows: ["CREW-05"],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, bobName } = await twoUsersInOneCrew(ctx);
    const CH = "Bravo";

    await step(`alice adds the voice channel ${CH} from the crew menu`, async () => {
      await addVoiceChannel(alice, CH);
    });

    await step(`bob sees ${CH}`, async () => {
      await waitForChannels(bob, ["General", CH]);
    });

    await step(`bob joins ${CH}, both see him there`, async () => {
      await bob.click(CH);
      await bob.waitFor("bob is in voice", (s) => s.in_voice, 20_000);
      for (const u of [bob, alice]) {
        await u.waitFor(`${u.name} sees bob in ${CH}`, (s) => voiceMembers(s, CH).includes(bobName));
      }
      expect((await bob.state()).voice_channels.find((c) => c.name === CH)?.active, `${CH} is bob's active channel`);
      await alice.checkpoint("alice-sees-bob-in-new-channel");
    });

    await step("no hidden errors on either side", () => noHiddenErrors(ctx, [alice, bob]));
  },
});

const A = "General";
const B = "Bravo";
const C = "Charlie";

/**
 * From the first click to both users showing bob in C only. The core runs
 * the three joins one after another, and each JoinVoice holds the command
 * loop for 0.6 to 1.6 s (an SFU attempt, then the P2P fallback: #104; 38
 * joins measured on macOS against the local stack). Three joins take at most
 * about 5 s; the limit is twice that. See plans/E2E-QA.md §16.2.
 */
const SETTLE_LIMIT_MS = 10_000;
/** After both users show bob in C only, the state must hold this long. */
const WATCH_MS = 3_000;
const SAMPLE_MS = 50;

type Side = { app: App; own: boolean; firstInC: number | null };

/** Why this user's view is not "bob in C only", or "" when it is. */
function notSettled(s: AppState, side: Side, bobName: string): string {
  if (side.own && !s.in_voice) return "bob is not in voice";
  if (!voiceMembers(s, C).includes(bobName)) return `bob is not listed in ${C}`;
  const elsewhere = [A, B].filter((ch) => voiceMembers(s, ch).includes(bobName));
  if (elsewhere.length) return `bob is also listed in ${elsewhere.join(", ")}`;
  if (side.own && !s.voice_channels.find((c) => c.name === C)?.active) return `${C} is not the active channel`;
  return "";
}

/**
 * Sample both users until each shows bob in C only, then for WATCH_MS more.
 * Returns the settle time from `t0`. Fails when the switch does not settle
 * within SETTLE_LIMIT_MS, when bob shows in A or B after he first showed in C,
 * or when the final state does not hold for the watch window.
 */
async function watchSwitch(ctx: JourneyContext, sides: Side[], bobName: string, t0: number): Promise<number> {
  let settledAt: number | null = null;
  for (;;) {
    const states = await Promise.all(sides.map((sd) => sd.app.state()));
    const at = Date.now() - t0;
    const why = states.map((s, i) => notSettled(s, sides[i], bobName));
    states.forEach((s, i) => {
      const sd = sides[i];
      if (sd.firstInC === null && voiceMembers(s, C).includes(bobName)) sd.firstInC = at;
      if (sd.firstInC === null) return;
      const late = [A, B].filter((ch) => voiceMembers(s, ch).includes(bobName));
      ctx.expect(
        late.length === 0,
        `${sd.app.name} shows bob in ${late.join(", ")} at ${at} ms, after bob first showed in ${C} at ${sd.firstInC} ms (a late replay)`,
      );
    });
    if (settledAt === null) {
      if (why.every((w) => w === "")) settledAt = at;
      else if (at > SETTLE_LIMIT_MS) {
        const detail = sides.map((sd, i) => `${sd.app.name}: ${why[i] || "ok"}`).join("; ");
        throw new Error(`the switch did not settle within ${SETTLE_LIMIT_MS} ms (${detail})`);
      }
    } else {
      const broken = sides.map((sd, i) => (why[i] ? `${sd.app.name}: ${why[i]}` : "")).filter(Boolean);
      ctx.expect(broken.length === 0, `the final state broke at ${at} ms, ${at - settledAt} ms after it settled (${broken.join("; ")})`);
      if (at - settledAt >= WATCH_MS) return settledAt;
    }
    await new Promise((r) => setTimeout(r, SAMPLE_MS));
  }
}

export const rapidSwitch = journey({
  id: "voice.rapid-switch",
  flows: ["VOICE-03"],
  // After bob joins A, his crew card leaves the accessibility tree for
  // seconds, so the clicks on B and C wait or fail.
  knownIssues: [96],
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, bobName } = await twoUsersInOneCrew(ctx);

    await step(`alice adds the voice channels ${B} and ${C}`, async () => {
      await addVoiceChannel(alice, B);
      await addVoiceChannel(alice, C);
    });

    await step(`bob sees ${A}, ${B} and ${C}`, async () => {
      await waitForChannels(bob, [A, B, C]);
      expect(!(await bob.state()).in_voice, "bob is not in voice before the clicks");
    });

    await step(`bob clicks ${A}, ${B}, ${C} back to back and ends in ${C}`, async () => {
      const t0 = Date.now();
      await bob.click(A);
      await bob.click(B);
      await bob.click(C);
      const clicksMs = Date.now() - t0;
      const sides: Side[] = [
        { app: bob, own: true, firstInC: null },
        { app: alice, own: false, firstInC: null },
      ];
      const settleMs = await watchSwitch(ctx, sides, bobName, t0);
      // One line for each run, to measure the switch over many runs.
      process.stdout.write(
        `    switch: clicks ${clicksMs} ms, settled ${settleMs} ms after the first click` +
          ` (bob first in ${C} at ${sides[0].firstInC} ms, on alice at ${sides[1].firstInC} ms)\n`,
      );
      await bob.checkpoint("bob-in-charlie");
      await alice.checkpoint("alice-sees-bob-in-charlie");
    });

    await step("no hidden errors on either side", () => noHiddenErrors(ctx, [alice, bob]));
  },
});

/** The same check as the other journeys: core Error events never reach the UI. */
async function noHiddenErrors(ctx: JourneyContext, users: App[]): Promise<void> {
  for (const u of users) {
    const errors = (await u.events()).filter((e) => e.message).map((e) => e.message);
    ctx.expect(errors.length === 0, `${u.name} has no Error events, got: ${errors.join(" | ")}`);
  }
}
