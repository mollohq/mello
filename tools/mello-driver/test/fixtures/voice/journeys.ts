// Journeys for the voice test. They run against ../events/fake-app.mjs.

import { journey } from "../../../src/journey.ts";
import { expectSfuVoice } from "../../../src/voice.ts";

/** A voice journey: the runner checks the local SFU before it starts. */
export const inVoice = journey({
  id: "fake.voice",
  flows: [],
  voice: true,
  async run({ launch, step }) {
    const alice = await launch("alice");
    await step("alice's call runs through the SFU", () => expectSfuVoice(alice, 1_000));
  },
});

/** A journey with no voice: it does not need the SFU. */
export const noVoice = journey({
  id: "fake.no-voice",
  flows: [],
  async run({ launch }) {
    await launch("alice");
  },
});
