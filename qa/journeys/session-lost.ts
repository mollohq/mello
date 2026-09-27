// AUTH-02 (#71, R4): a user finishes onboarding, quits, and loses the saved
// session (for example a new build cannot read the keychain). On the next
// start, device auth finds the same account. The app opens directly with the
// user's crew: no onboarding step 1.

import { rmSync } from "node:fs";

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

export default journey({
  id: "auth.session-lost",
  flows: ["AUTH-02"],
  async run({ runId, launch, step, expect }) {
    const crew = `Lost ${runId}`;
    const name = `ivy${runId}`;
    const ivy = await launch("ivy");

    await step("ivy onboards and reaches the app", async () => {
      await onboardWithNewCrew(ivy, crew, name, "Public");
    });

    await step("ivy quits and the saved session is lost", async () => {
      await ivy.kill();
      rmSync(ivy.sessionFile, { force: true });
    });

    await step("ivy starts the app: it opens directly, same user, same crew", async () => {
      await ivy.launch();
      let sawOnboarding = false;
      const s = await ivy.waitFor(
        "the app",
        (st) => {
          if (st.screen === "onboarding") sawOnboarding = true;
          return st.screen === "app" && st.logged_in;
        },
        20_000,
      );
      expect(!sawOnboarding, "onboarding did not show");
      expect(s.user_name === name, `user is ${name}, got "${s.user_name}"`);
      await ivy.waitFor(`${crew} in the sidebar`, (st) => st.crews.includes(crew));
      await ivy.checkpoint("reopened");
    });

    await step("the restore failed and device auth found the same account", async () => {
      const types = (await ivy.events()).map((e) => e.type);
      expect(types.includes("LoginFailed"), `the session restore failed, events: ${types.slice(0, 12).join(", ")}`);
      expect(types.includes("DeviceAuthed"), `device auth ran, events: ${types.slice(0, 12).join(", ")}`);
    });
  },
});
