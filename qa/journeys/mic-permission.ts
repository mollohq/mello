// The control bar on a cold run, before the user has given the microphone.
// The driver fixes the permission with MELLO_E2E_MIC_PERMISSION
// (plans/E2E-QA.md §16.7), so the OS is not asked and no OS dialog opens.
//
// - undetermined (VOICE-07): the bar asks for the microphone and has no Mute.
//   "ALLOW MICROPHONE" answers as a user who presses Allow: Mute shows and
//   the prompt goes.
// - denied (VOICE-06): the bar says that access is denied and shows
//   "OPEN SETTINGS", with no Mute. The journey does not press "OPEN SETTINGS":
//   it opens System Settings, an OS surface outside the app.
//
// Neither journey joins voice, so neither needs the SFU.

import type { App, MicPermission } from "../../tools/mello-driver/src/app.ts";
import { journey, type JourneyContext } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

const ASK = "Microphone access is needed for voice chat";
const DENIED = "Microphone access denied";
const ALLOW = "ALLOW MICROPHONE";
const SETTINGS = "OPEN SETTINGS";
/** The controls that show only with the permission. */
const VOICE_CONTROLS = ["Mute", "Unmute", "Deafen", "Undeafen"];

/** A fresh install with this permission onboards with a new crew and reaches the app. */
async function coldRun(ctx: JourneyContext, micPermission: MicPermission): Promise<App> {
  const { runId, launch, step } = ctx;
  const alice = await launch("alice", { micPermission });
  await step(`setup: alice starts with the microphone ${micPermission}, onboards and reaches the app`, async () => {
    await alice.waitFor(
      `mic permission ${micPermission} (a build without the e2e feature asks the OS instead)`,
      (s) => s.mic_permission === micPermission,
    );
    await onboardWithNewCrew(alice, `Mic ${runId}`, `alice${runId}`, "Public");
    await alice.waitFor("the app", (s) => s.screen === "app");
  });
  return alice;
}

/** The labels of the controls on screen, from the list in `wanted`. */
async function shown(app: App, wanted: string[]): Promise<string[]> {
  return (await app.controls()).map((c) => c.label).filter((l) => wanted.includes(l));
}

export const undetermined = journey({
  id: "voice.mic-permission-undetermined",
  flows: ["VOICE-07"],
  async run(ctx) {
    const { step, expect } = ctx;
    const alice = await coldRun(ctx, "undetermined");

    await step(`the control bar asks for the microphone with "${ALLOW}", and has no Mute`, async () => {
      await alice.find(ALLOW);
      expect(await alice.text(ASK), `the bar shows "${ASK}"`);
      const voice = await shown(alice, VOICE_CONTROLS);
      expect(voice.length === 0, `no voice controls before the permission, got: ${voice.join(", ")}`);
      await alice.checkpoint("asks-for-the-microphone");
    });

    await step(`alice presses "${ALLOW}": Mute shows and the prompt goes`, async () => {
      await alice.click(ALLOW);
      await alice.waitFor("mic permission granted", (s) => s.mic_permission === "granted", 5_000);
      await alice.find("Mute");
      expect(!(await alice.text(ASK)), `the bar no longer shows "${ASK}"`);
      const prompt = await shown(alice, [ALLOW, SETTINGS]);
      expect(prompt.length === 0, `no permission button after Allow, got: ${prompt.join(", ")}`);
      await alice.checkpoint("microphone-allowed");
    });

    await step("no hidden errors", async () => {
      const errors = (await alice.events()).filter((e) => e.message).map((e) => e.message);
      expect(errors.length === 0, `alice has no Error events, got: ${errors.join(" | ")}`);
    });
  },
});

export const denied = journey({
  id: "voice.mic-permission-denied",
  flows: ["VOICE-06"],
  async run(ctx) {
    const { step, expect } = ctx;
    const alice = await coldRun(ctx, "denied");

    await step(`the control bar says access is denied, shows "${SETTINGS}", and has no Mute`, async () => {
      await alice.find(SETTINGS);
      expect(await alice.text(DENIED), `the bar shows "${DENIED} …"`);
      expect(!(await alice.text(ASK)), `the bar does not ask with "${ASK}"`);
      const wrong = await shown(alice, [ALLOW, ...VOICE_CONTROLS]);
      expect(wrong.length === 0, `no "${ALLOW}" and no voice controls, got: ${wrong.join(", ")}`);
      await alice.checkpoint("microphone-denied");
    });

    await step("no hidden errors", async () => {
      const errors = (await alice.events()).filter((e) => e.message).map((e) => e.message);
      expect(errors.length === 0, `alice has no Error events, got: ${errors.join(" | ")}`);
    });
  },
});
