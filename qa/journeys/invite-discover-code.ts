// INV-07: join a crew by typing its invite code in Discover.
//
// alice makes a public crew and reads its invite code. bob uses mello with
// his own crew. He opens Discover and first types a text that is no invite:
// the app reads no code in it and sends nothing (CREW-INVITES.md §8.4). Then he types alice's code and presses "Join Crew":
// bob has both crews, and alice sees bob in hers.

import { setTimeout as sleep } from "node:timers/promises";

import { DriverError, type App } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./lib/onboarding.ts";

const JOIN_EVENTS = ["InviteJoined", "InviteJoinFailed"];

/**
 * Wait until the app log has a line that matches. The log is a local file,
 * so a poll costs nothing.
 */
async function waitForLog(app: App, line: RegExp, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (line.test(app.logTail(2_000))) return;
    await sleep(100);
  }
  throw new DriverError(`${app.name}: no log line ${line} after ${timeoutMs} ms`);
}

export default journey({
  id: "invite.discover-code",
  flows: ["INV-07"],
  async run({ runId, launch, step, expect }) {
    const aliceCrew = `Owls ${runId}`;
    const bobCrew = `Bob Home ${runId}`;
    const aliceName = `alice${runId}`;
    const bobName = `bob${runId}`;

    const alice = await launch("alice");
    const bob = await launch("bob");

    await step("alice and bob onboard, each with a crew; alice's crew is public", async () => {
      await Promise.all([onboardWithNewCrew(alice, aliceCrew, aliceName, "Public"), onboardWithNewCrew(bob, bobCrew, bobName)]);
    });

    let code = "";
    await step("alice reads the invite code", async () => {
      await alice.click("Share invite link");
      const m = (await alice.find("Invite link")).value.match(/\/join\/([A-Z0-9-]+)$/);
      expect(m, "the invite modal shows a join link");
      code = m![1];
      await alice.dismiss("invite_share");
    });

    await step("bob opens Discover", async () => {
      await bob.click("Discover");
      await bob.waitFor("Discover is open", (s) => s.open_modals.includes("discover"));
      await bob.find("Invite code");
      await bob.checkpoint("discover");
    });

    await step("bob types a text that is no invite: the app reads no code and sends nothing", async () => {
      await bob.type("Invite code", "hello world");
      await bob.click("Join Crew");
      // The click callback reads the text, logs that it found no code, sets
      // the message, and sends no command (client/src/callbacks/crew.rs).
      //
      // The journey does not read the message on screen. The state port has
      // no field for it, and a read of the screen text on Discover (about 350
      // requests at once) crashed the driver on the Node bug of
      // plans/E2E-QA.md §16.7.4 in 4 of 20 runs. The harness test
      // the_discover_code_field_refuses_what_is_no_invite checks the message.
      await waitForLog(bob, /\[discover\] join-by-invite input="hello world" code=None/);
      const joins = (await bob.events()).filter((e) => JOIN_EVENTS.includes(e.type));
      expect(joins.length === 0, `no join answer, got: ${joins.map((e) => e.type).join(", ")}`);
      const s = await bob.state();
      expect(s.crews.length === 1 && s.crews[0] === bobCrew, `bob still has only ${bobCrew}, got: ${s.crews.join(", ")}`);
      await bob.checkpoint("not-valid");
    });

    await step("bob types alice's code and presses Join Crew: both crews", async () => {
      await bob.type("Invite code", code);
      await bob.click("Join Crew");
      const s = await bob.waitFor(`${aliceCrew} and ${bobCrew} in bob's crews`, (st) => st.crews.includes(aliceCrew) && st.crews.includes(bobCrew), 20_000);
      expect(s.crews.length === 2, `two crews, got: ${s.crews.join(", ")}`);
      const joins = (await bob.events()).filter((e) => JOIN_EVENTS.includes(e.type)).map((e) => e.type);
      expect(joins.length === 1 && joins[0] === "InviteJoined", `one InviteJoined and no InviteJoinFailed, got: ${joins.join(", ")}`);
      const errors = (await bob.events()).filter((e) => e.type === "Error");
      expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
      await bob.checkpoint("joined");
    });

    await step("alice sees bob in her crew", async () => {
      await alice.waitFor(`${bobName} in alice's members`, (s) => s.members.includes(bobName), 20_000);
      await alice.checkpoint("alice-sees-bob");
    });
  },
});
