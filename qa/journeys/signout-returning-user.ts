// AUTH-02 (#70, R1, R5): a device user onboards and then logs out. Step 1
// shows the device card's "Sign in" and, under it, "Use another account".
// It shows no "I already have an account" and no second "Sign in".
// "Use another account" opens the sign-in panel. "Sign in" opens the app
// again as the same user.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

const SIGN_IN_LABELS = ["Sign in", "Use another account", "I already have an account"];

export default journey({
  id: "auth.signout-returning-user",
  flows: ["AUTH-02"],
  async run({ runId, launch, step, expect }) {
    const crew = `Return ${runId}`;
    const name = `hank${runId}`;
    const hank = await launch("hank");

    await step("hank onboards and reaches the app", async () => {
      await onboardWithNewCrew(hank, crew, name, "Public");
    });

    await step("hank logs out", async () => {
      await hank.click("Account menu");
      await hank.activate("Log Out"); // a popup item: see App.activate
      await hank.waitFor("step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1 && !s.logged_in);
    });

    await step("step 1 shows the device card and another account", async () => {
      // The device card shows when device auth answers.
      await hank.find("Sign in", 0, 15_000);
      await hank.settle(["DiscoverCrewsLoaded", "DeviceAuthed"]);
      // One control per place on screen. After step 1 re-renders, Slint MCP
      // can answer the query with several handles for one element (same
      // label, same rectangle). The headless harness counts real instances
      // (flow_tests.rs, a_returning_user_sees_the_device_card_and_another_account).
      const hits = (await hank.controls()).filter((c) => SIGN_IN_LABELS.includes(c.label));
      const found = [...new Set(hits.map((c) => `${c.label} at (${c.x},${c.y}) ${c.width}x${c.height}`))];
      const labels = found.map((f) => f.slice(0, f.indexOf(" at (")));
      expect(
        found.length === 2 && labels.includes("Sign in") && labels.includes("Use another account"),
        `"Sign in" and "Use another account" and nothing else, got: [${found.join("; ")}]`,
      );
      await hank.checkpoint("step1-returning");
    });

    await step("\"Use another account\" opens the sign-in panel, and Back returns to step 1", async () => {
      await hank.click("Use another account");
      await hank.waitFor("the sign-in panel", (s) => s.screen === "sign_in");
      await hank.click("Back");
      await hank.waitFor("step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1 && !s.logged_in);
      await hank.find("Sign in");
    });

    await step("the device card opens the app as hank, with his crew", async () => {
      await hank.click("Sign in");
      const s = await hank.waitFor("the app", (st) => st.screen === "app" && st.logged_in, 20_000);
      expect(s.user_name === name, `signed in as ${name}, got "${s.user_name}"`);
      await hank.waitFor(`${crew} in the sidebar`, (st) => st.crews.includes(crew));
    });
  },
});
