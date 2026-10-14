// Journeys for the guard test. They run against ../events/fake-app.mjs.

import { journey } from "../../../src/journey.ts";

/** A journey that needs only Nakama. */
export const plain = journey({
  id: "fake.plain",
  flows: [],
  async run({ launch }) {
    await launch("alice");
  },
});

/** A journey that needs the lounge: the runner checks it before it starts. */
export const lounge = journey({
  id: "fake.lounge",
  flows: [],
  needs: ["lounge"],
  async run({ launch }) {
    await launch("alice");
  },
});
