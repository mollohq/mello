// AUTH-05 (#67): on a fresh install, a user picks "I already have an account"
// and signs in with an email that has no account. The sign-in panel says so in
// plain words and offers a way to start as a new player. That way returns to
// step 1 with no error left over, and onboarding goes on from there.

import type { App } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { createCrewAtStep1 } from "./lib/onboarding.ts";

const HAVE_ACCOUNT = "I already have an account";
const START_NEW = "Start as a new player";

/** Open the sign-in panel from step 1 and sign in with an email that has no account. */
async function signInWithUnknownEmail(app: App, email: string): Promise<void> {
  await app.click(HAVE_ACCOUNT);
  await app.waitFor("the sign-in panel", (s) => s.screen === "sign_in");
  await app.click("Email + password");
  await app.type("Email", email);
  await app.type("Password", "not-a-real-password");
  await app.click("SIGN IN");
  await app.waitFor("the sign-in attempt ends with an error", (s) => !s.login_loading && s.login_error !== "", 20_000);
}

export default journey({
  id: "auth.signin-fresh-install-unknown-account",
  flows: ["AUTH-05"],
  async run({ runId, launch, step, expect }) {
    const gina = await launch("gina");
    const email = `nobody-${runId}@example.test`;

    await step("fresh install: step 1 offers exactly one way to sign in", async () => {
      await gina.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
      await gina.settle(["DiscoverCrewsLoaded"]);
      await gina.find(HAVE_ACCOUNT);
      const labels = [...new Set((await gina.controls()).map((c) => c.label))];
      expect(!labels.includes("Sign in"), `no returning-user "Sign in" control on a fresh install, got: ${labels.join(", ")}`);
      await gina.checkpoint("step1-fresh");
    });

    await step("sign in with an email that has no account: a plain message and a way forward", async () => {
      await signInWithUnknownEmail(gina, email);
      const s = await gina.state();
      expect(s.screen === "sign_in", `still on the sign-in panel, got ${s.screen}`);
      expect(!/Authentication failed|User account not found/i.test(s.login_error), `no raw server text, got "${s.login_error}"`);
      expect(await gina.text("No account found"), "the panel says that no account was found");
      expect(!(await gina.text("Authentication failed")), "the raw server text is not on screen");
      await gina.find(START_NEW);
      await gina.checkpoint("no-account");
    });

    await step("Back leaves the panel and clears the error", async () => {
      await gina.click("Back");
      const s = await gina.waitFor("step 1", (st) => st.screen === "onboarding" && st.onboarding_step === 1);
      expect(s.login_error === "", `no leftover sign-in error on step 1, got "${s.login_error}"`);
    });

    await step("opening the panel again shows no old error", async () => {
      await gina.click(HAVE_ACCOUNT);
      const s = await gina.waitFor("the sign-in panel", (st) => st.screen === "sign_in");
      expect(s.login_error === "", `the panel opens clean, got "${s.login_error}"`);
      expect(!(await gina.text("No account found")), "no old message on the panel");
      await gina.click("Back");
      await gina.waitFor("step 1", (st) => st.screen === "onboarding" && st.onboarding_step === 1);
    });

    await step(`"${START_NEW}" returns to step 1 with no error`, async () => {
      await signInWithUnknownEmail(gina, email);
      await gina.click(START_NEW);
      const s = await gina.waitFor("step 1", (st) => st.screen === "onboarding" && st.onboarding_step === 1);
      expect(s.login_error === "", `no leftover sign-in error, got "${s.login_error}"`);
      expect(!s.show_sign_in, "the sign-in panel is closed");
      await gina.checkpoint("back-on-step1");
    });

    await step("no loop: the new player goes on to step 2", async () => {
      await createCrewAtStep1(gina, `Gina Crew ${runId}`);
    });
  },
});
