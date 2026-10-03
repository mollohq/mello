// Journeys for the events test. They run against fake-app.mjs.

import { setTimeout as sleep } from "node:timers/promises";

import { journey } from "../../../src/journey.ts";

export const passing = journey({
  id: "fake.passing",
  flows: [],
  async run({ launch, step }) {
    const alice = await launch("alice");
    await step("click the button", async () => {
      for (let i = 0; i < 3; i++) await alice.click("Go");
    });
    await step("wait with no action", async () => {
      await sleep(600);
    });
  },
});

export const failing = journey({
  id: "fake.failing",
  flows: [],
  async run({ launch, step, expect }) {
    const alice = await launch("alice");
    await step("click the button", async () => {
      await alice.click("Go");
    });
    await step("expect something false", async () => {
      await sleep(300);
      expect(false, "the button changed the screen");
    });
  },
});

/** Waits for a long time: the test sends a signal while it waits. */
export const hanging = journey({
  id: "fake.hanging",
  flows: [],
  async run({ launch, step }) {
    await launch("alice");
    await step("wait for a signal", async () => {
      await sleep(60_000);
    });
  },
});
