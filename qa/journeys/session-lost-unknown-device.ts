// AUTH-02: a device id on disk whose account the server does not have (for
// example a reset server, or a settings file from another install). The
// saved session is gone too. Device auth makes a new account, so the app
// goes to step 1. Step 1 must still offer "I already have an account": a
// device id on disk does not mean the user can reach the account behind it.

import { randomBytes } from "node:crypto";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

const HAVE_ACCOUNT = "I already have an account";
const SIGN_IN_LABELS = ["Sign in", "Use another account", HAVE_ACCOUNT];

export default journey({
  id: "auth.session-lost-unknown-device",
  flows: ["AUTH-02"],
  async run({ runId, launch, step, expect }) {
    const crew = `Gone ${runId}`;
    const name = `jo${runId}`;
    const jo = await launch("jo");

    await step("jo onboards and reaches the app", async () => {
      await onboardWithNewCrew(jo, crew, name, "Public");
    });

    await step("jo quits; the session is lost and the device id has no account", async () => {
      await jo.kill();
      rmSync(jo.sessionFile, { force: true });
      const settingsFile = join(jo.dir, "config", "mello.toml");
      const before = readFileSync(settingsFile, "utf8");
      const after = before.replace(/^device_id = ".*"$/m, `device_id = "${randomBytes(16).toString("hex")}"`);
      expect(after !== before, `the settings file has a device_id line: ${settingsFile}`);
      writeFileSync(settingsFile, after);
    });

    await step("jo starts the app: step 1 offers \"I already have an account\"", async () => {
      await jo.launch();
      await jo.waitFor("step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1 && !s.logged_in, 30_000);
      await jo.settle(["DiscoverCrewsLoaded", "DeviceAuthed"]);
      const deviceAuthed = (await jo.events()).some((e) => e.type === "DeviceAuthed");
      expect(deviceAuthed, "device auth ran for the unknown device id");
      const hits = (await jo.controls()).filter((c) => SIGN_IN_LABELS.includes(c.label));
      const labels = [...new Set(hits.map((c) => c.label))];
      expect(
        labels.length === 1 && labels[0] === HAVE_ACCOUNT,
        `"${HAVE_ACCOUNT}" and nothing else, got: [${labels.join("; ")}]`,
      );
      await jo.checkpoint("step1-unknown-device");
    });

    await step("\"I already have an account\" opens the sign-in panel", async () => {
      await jo.click(HAVE_ACCOUNT);
      await jo.waitFor("the sign-in panel", (s) => s.screen === "sign_in");
    });
  },
});
