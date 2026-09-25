// AUTH-04, ONB-03, ONB-08: sign-up and sign-in with a social provider against
// the fake provider (tools/fake-oauth, backend/docker-compose.e2e.yml).
// The app, its callback server and state check, the Go auth hooks and Nakama
// all run unmodified; only the provider is fake.
//
// One journey per provider and case. `outcome` is what the fake does.

import { completeOAuth, fakeOAuthUp, type OAuthOutcome } from "../../tools/mello-driver/src/browser.ts";
import { journey, type Journey, type JourneyContext } from "../../tools/mello-driver/src/journey.ts";
import { createCrewAtStep1, profileAtStep2 } from "./lib/onboarding.ts";

type Provider = "Discord" | "Twitch" | "Steam" | "Google";

async function needFake(): Promise<void> {
  if (!(await fakeOAuthUp())) {
    throw new Error(
      "fake OAuth provider is not running. Start the e2e profile:\n" +
        "  docker compose -f backend/docker-compose.yml -f backend/docker-compose.e2e.yml up -d --build",
    );
  }
}

/** A fresh install at step 3, ready to link an identity. */
async function atStep3(ctx: JourneyContext, who: string) {
  const app = await ctx.launch(who);
  await createCrewAtStep1(app, `${who} crew ${ctx.runId}`, "Public");
  await profileAtStep2(app, `${who}${ctx.runId}`);
  return app;
}

/** Link at step 3, approve, reach the app. Then sign in on a new computer as the same person. */
function linkThenSignInElsewhere(provider: Provider): Journey {
  return journey({
    id: `auth.social.${provider.toLowerCase()}.link-then-sign-in`,
    flows: ["ONB-03", "AUTH-04", "ONB-08"],
    // #67: the only way to sign in on a fresh install today is the "Sign in"
    // link on step 1, which #67 removes. When it goes, this journey signs in
    // through step 3's link-or-switch instead.
    knownIssues: [67],
    async run(ctx) {
      await needFake();
      const { runId, step, expect } = ctx;
      const identity = `${provider.toLowerCase()}-${runId}`;
      const dana = await atStep3(ctx, "dana");

      await step(`dana links ${provider} at step 3 and reaches the app`, async () => {
        await dana.click(provider);
        await completeOAuth(dana, { identity });
        await dana.waitFor("the app after linking", (s) => s.screen === "app", 20_000);
      });

      const erin = await ctx.launch("erin");
      await step(`on a new computer, dana signs in with ${provider}`, async () => {
        await erin.waitFor("step 1", (s) => s.onboarding_step === 1, 30_000);
        await erin.settle(["DiscoverCrewsLoaded"]);
        await erin.click("Sign in");
        await erin.click(provider);
        await completeOAuth(erin, { identity });
        const s = await erin.waitFor("the app as dana", (st) => st.screen === "app" && st.logged_in, 20_000);
        expect(s.user_name === `dana${runId}`, `signed in as dana${runId}, got "${s.user_name}"`);
        await erin.waitFor("dana's crew", (st) => st.crews.includes(`dana crew ${runId}`));
      });
    },
  });
}

/** A link that fails in the browser or at the provider must leave step 3 usable and say why. */
function linkFails(provider: Provider, outcome: OAuthOutcome, expectError: boolean): Journey {
  return journey({
    id: `auth.social.${provider.toLowerCase()}.${outcome.replace("_", "-")}`,
    flows: ["ONB-03"],
    async run(ctx) {
      await needFake();
      const { step, expect } = ctx;
      const frank = await atStep3(ctx, "frank");

      await step(`frank starts ${provider}, the provider answers "${outcome}"`, async () => {
        await frank.click(provider);
        await completeOAuth(frank, { outcome });
      });

      await step("step 3 stays usable and the spinner stops", async () => {
        const s = await frank.waitFor(
          "the link attempt ends",
          (st) => !st.login_loading && (!expectError || st.link_error !== ""),
          20_000,
        );
        expect(s.onboarding_step === 3, `still on step 3, got step ${s.onboarding_step}`);
        if (expectError) expect(s.link_error !== "", "a reason shows");
        await frank.checkpoint(`after-${outcome}`);
        // Usable: skipping still works.
        await frank.click("Skip for now");
        await frank.waitFor("the app", (st) => st.screen === "app");
      });
    },
  });
}

export const discordLink = linkThenSignInElsewhere("Discord");
export const twitchLink = linkThenSignInElsewhere("Twitch");
export const steamLink = linkThenSignInElsewhere("Steam");
export const googleLink = linkThenSignInElsewhere("Google");

export const discordDeny = linkFails("Discord", "deny", true);
export const discordWrongState = linkFails("Discord", "wrong_state", true);
export const discordNoCallback = linkFails("Discord", "no_callback", true);
export const discordRejected = linkFails("Discord", "reject_token", true);
export const googleDeny = linkFails("Google", "deny", true);

export default discordLink;
