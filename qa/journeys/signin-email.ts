// AUTH-03: sign out, then sign in with email and password.
//
// After a logout, step 1 on the same computer shows the device card and
// "Use another account" (AUTH-02). Email sign-in from "I already have an
// account" is tested on a second computer: erin is a new user folder, a
// fresh install. dana onboards and links an email,
// then logs out. On erin, a wrong password gets a plain message and keeps
// the panel open; the right password opens the app as dana, with her crew.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

const HAVE_ACCOUNT = "I already have an account";
const WRONG = "Wrong email or password.";

export default journey({
  id: "auth.signin-email",
  flows: ["AUTH-03"],
  async run({ runId, launch, step, expect }) {
    const crew = `Email ${runId}`;
    const name = `dana${runId}`;
    const dana = await launch("dana");
    let creds = { email: "", password: "" };
    let danaId = "";

    await step("dana onboards, links an email and reaches the app", async () => {
      creds = await onboardWithNewCrew(dana, crew, name, "Public");
      danaId = (await dana.state()).user_id;
      expect(danaId !== "", "dana has a user id");
    });

    await step("dana logs out: step 1, not logged in", async () => {
      await dana.click("Account menu");
      await dana.activate("Log Out"); // a popup item: see App.activate
      await dana.waitFor("step 1, logged out", (s) => s.screen === "onboarding" && s.onboarding_step === 1 && !s.logged_in);
      await dana.checkpoint("logged-out");
    });

    // A second computer: a new user folder with no device account.
    const erin = await launch("erin");

    await step("erin (a fresh install) opens the email sign-in", async () => {
      await erin.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
      await erin.settle(["DiscoverCrewsLoaded"]);
      await erin.click(HAVE_ACCOUNT);
      await erin.waitFor("the sign-in panel", (s) => s.screen === "sign_in");
      await erin.click("Email + password");
    });

    await step("a wrong password: a plain message, still on the sign-in panel, no spinner", async () => {
      await erin.type("Email", creds.email);
      await erin.type("Password", `${creds.password}-wrong`);
      await erin.click("SIGN IN");
      const s = await erin.waitFor("the sign-in attempt ends with an error", (st) => !st.login_loading && st.login_error !== "", 20_000);
      expect(s.login_error === WRONG, `the message is "${WRONG}", got "${s.login_error}"`);
      expect(s.screen === "sign_in", `still on the sign-in panel, got ${s.screen}`);
      expect(!s.logged_in, "not logged in");
      expect(await erin.text(WRONG), "the message is on screen");
      await erin.checkpoint("wrong-password");
    });

    await step("the right password opens the app as dana, with dana's crew", async () => {
      await erin.type("Password", creds.password);
      await erin.click("SIGN IN");
      const s = await erin.waitFor("the app", (st) => st.screen === "app" && st.logged_in, 20_000);
      expect(s.user_id === danaId, `the same account as dana (${danaId}), got ${s.user_id}`);
      expect(s.user_name === name, `signed in as ${name}, got "${s.user_name}"`);
      await erin.waitFor(`${crew} in the sidebar`, (st) => st.crews.includes(crew));
      await erin.checkpoint("signed-in");
    });

    await step("no Error events on either app", async () => {
      for (const u of [dana, erin]) {
        const errors = (await u.events()).filter((e) => e.type === "Error");
        expect(errors.length === 0, `no Error events on ${u.name}, got: ${errors.map((e) => e.message).join(" | ")}`);
      }
    });
  },
});
