// INV-05: a fresh install opened from an invite link with a code that does
// not exist. Step 1 shows with a clear message and no join modal. Step 1 is
// not a dead end: the user creates a crew and reaches the app.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

export default journey({
  id: "invite.invalid-code-cold",
  flows: ["INV-05", "ONB-01"],
  async run({ runId, launch, step, expect }) {
    const crew = `Fresh Start ${runId}`;
    const name = `ivy${runId}`;
    // The code alphabet has no 0, 1, I or O, so this code can never exist.
    const ivy = await launch("ivy", "mello://join/NO0I-10O0");

    await step("step 1 says the invite link is not valid", async () => {
      const s = await ivy.waitFor(
        "step 1 with the invite message",
        (st) => st.screen === "onboarding" && st.onboarding_step === 1 && st.onboarding_invite_error !== "",
        30_000,
      );
      expect(
        s.onboarding_invite_error === "This invite link is no longer valid.",
        `a clear message, got "${s.onboarding_invite_error}"`,
      );
      expect(!s.join_crew_modal_open, "no join modal");
      expect(await ivy.text("This invite link is no longer valid."), "the message is on screen");
      await ivy.checkpoint("step1-invalid-invite");
    });

    await step("step 1 is the way forward: create a crew and reach the app", async () => {
      await onboardWithNewCrew(ivy, crew, name);
      const s = await ivy.state();
      expect(s.crews.length === 1 && s.crews[0] === crew, `only the new crew, got: ${s.crews.join(", ")}`);
      expect(!s.join_crew_modal_open, "no join modal in the app");
      await ivy.checkpoint("in-app");
    });
  },
});
