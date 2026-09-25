// AUTH-01: a returning user quits and starts the app again. The saved session
// restores, and the app opens on the same crew with no onboarding.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

export default journey({
  id: "auth.session-restore",
  flows: ["AUTH-01"],
  async run({ runId, launch, step, expect }) {
    const crew = `Restore ${runId}`;
    const name = `carol${runId}`;
    const carol = await launch("carol");

    await step("carol onboards and reaches the app", async () => {
      await onboardWithNewCrew(carol, crew, name, "Public");
    });

    await step("carol quits and starts the app again", async () => {
      await carol.restart();
    });

    await step("the session restores into the app, same user, same crew", async () => {
      const s = await carol.waitFor("the app after restore", (st) => st.screen === "app" && st.logged_in, 20_000);
      expect(s.user_name === name, `user is ${name}, got "${s.user_name}"`);
      await carol.waitFor(`${crew} in the sidebar`, (st) => st.crews.includes(crew));
      await carol.checkpoint("restored");
    });

    await step("the restore never showed onboarding", async () => {
      const shown = (await carol.events()).map((e) => e.type);
      expect(!shown.includes("LoginFailed"), `no LoginFailed during restore, events: ${shown.slice(0, 12).join(", ")}`);
    });
  },
});
