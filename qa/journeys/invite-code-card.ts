// INV-09: the web lounge could not hand the invite to the app, so bob has no
// deep link. He installs the app, and step 1 has a "Got an invite?" card.
// Three journeys:
//  - typedLink: bob types alice's invite link into the card. The welcome
//    screen names alice and her crew. "Join" opens step 2, and bob is a
//    member of the crew when onboarding ends. Both sides are checked.
//  - invalidCode: a text that is no invite, and a code that does not exist.
//    The card shows the message and marks the field. Editing clears it.
//    Step 1 is not a dead end.
//  - afterLogout: hank has a device account and logged out. He types the
//    link into the card, and finalize joins the crew into his account.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { inviteWelcome, joinFromWelcome, linkEmailAtStep3, onboardWithNewCrew, profileAtStep2 } from "./lib/onboarding.ts";

const NOT_VALID = "This invite code is not valid.";

export const typedLink = journey({
  id: "invite.code-card",
  flows: ["INV-09", "INV-03", "ONB-01"],
  async run({ runId, launch, step, expect }) {
    const crew = `Night Owls ${runId}`;
    const aliceName = `alice${runId}`;
    const bobName = `bob${runId}`;

    const alice = await launch("alice");
    let link = "";
    await step("alice: creates a crew and reads the invite link on screen", async () => {
      await onboardWithNewCrew(alice, crew, aliceName);
      await alice.click("Share invite link");
      link = (await alice.find("Invite link")).value;
      expect(/^https?:\/\/[^/]+\/join\/[A-Z0-9-]+$/.test(link), `the invite link field shows a join URL, got "${link}"`);
      await alice.checkpoint("invite-link");
    });

    // No deep link: the app opens on step 1, as after a plain download.
    const bob = await launch("bob");
    await step("bob: step 1 has the invite card, and it is empty", async () => {
      await bob.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
      await bob.settle(["DiscoverCrewsLoaded"]);
      expect(await bob.text("GOT AN INVITE?"), `step 1 shows "GOT AN INVITE?"`);
      expect(await bob.text("Paste the link or the code your friend sent you."), "the card says what to paste");
      const s = await bob.state();
      expect(s.onboarding_invite_code_error === "", "no error before a try");
      expect(!s.onboarding_invite_code_checking, "no resolve before a try");
      await bob.checkpoint("step1-card");
    });

    await step("bob: types alice's link into the card: the welcome screen names alice and the crew", async () => {
      // alice's link uses the web host. The card takes it as a user pastes it.
      const pasted = link.replace(/^https?:\/\//, "");
      await bob.type("GOT AN INVITE?", pasted);
      await bob.click("Open invite");
      await inviteWelcome(bob, crew, aliceName);
      const s = await bob.state();
      expect(!s.join_crew_modal_open, "no join modal: onboarding joins the crew");
      expect(s.onboarding_invite_code_error === "", "no error in the card");
      expect(!s.onboarding_invite_code_checking, "the resolve is done");
      await bob.checkpoint("welcome");
    });

    await step("bob: Join opens step 2, step 1 of 2, which names alice's crew", async () => {
      await joinFromWelcome(bob, crew);
      const s = await bob.state();
      expect(s.onboarding_invite_path, "the invite path counts two steps");
      await bob.checkpoint("step2-invited-crew");
    });

    await step("bob: Back on step 2 opens the welcome screen again, and Join opens step 2 again", async () => {
      await bob.click("Back");
      await inviteWelcome(bob, crew, aliceName);
      await joinFromWelcome(bob, crew);
      await bob.checkpoint("step2-after-back");
    });

    await step("bob: finishes onboarding (profile, then email at step 3)", async () => {
      await profileAtStep2(bob, bobName, 2);
      expect(await bob.text("STEP 02 / 02"), `step 3 shows "STEP 02 / 02"`);
      await linkEmailAtStep3(bob, bobName);
    });

    await step("bob: alice's crew is his only crew, and it is active", async () => {
      const s = await bob.waitFor(`${crew} in bob's crews`, (st) => st.crews.includes(crew));
      expect(s.crews.length === 1, `only the invited crew, got: ${s.crews.join(", ")}`);
      expect(!s.join_crew_modal_open, "no join modal in the app");
      const crewId = (await alice.state()).active_crew_id;
      await bob.waitFor(`${crew} is bob's active crew`, (st) => st.active_crew_id === crewId);
      const errors = (await bob.events()).filter((e) => e.message);
      expect(errors.length === 0, `no Error events on bob, got: ${errors.map((e) => e.message).join(" | ")}`);
      await bob.checkpoint("bob-in-app");
    });

    await step("alice: sees bob in her crew", async () => {
      await alice.waitFor(`${bobName} in alice's member list`, (s) => s.members.includes(bobName));
      await alice.checkpoint("alice-sees-bob");
    });
  },
});

export const invalidCode = journey({
  id: "invite.code-card-invalid",
  flows: ["INV-09", "INV-05", "ONB-01"],
  async run({ runId, launch, step, expect }) {
    const crew = `Fresh Start ${runId}`;
    const name = `ivy${runId}`;
    const ivy = await launch("ivy");

    await step("ivy: a text that is no invite gets the message in the card, with no resolve", async () => {
      await ivy.waitFor("onboarding step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1, 30_000);
      await ivy.settle(["DiscoverCrewsLoaded"]);
      await ivy.type("GOT AN INVITE?", "hello world");
      await ivy.click("Open invite");
      const s = await ivy.waitFor("the card error", (st) => st.onboarding_invite_code_error !== "");
      expect(s.onboarding_invite_code_error === NOT_VALID, `the message, got "${s.onboarding_invite_code_error}"`);
      expect(!s.onboarding_invite_code_checking, "no resolve ran: the button is not waiting");
      expect(await ivy.text(NOT_VALID), "the message is on screen, in the card");
      await ivy.checkpoint("card-malformed");
    });

    await step("ivy: editing the field clears the message", async () => {
      await ivy.type("GOT AN INVITE?", "NO0I-10O0");
      const s = await ivy.waitFor("the message gone", (st) => st.onboarding_invite_code_error === "");
      expect(!(await ivy.text(NOT_VALID)), "the message is off screen");
      expect(s.onboarding_step === 1, "still step 1");
    });

    await step("ivy: a code that does not exist gets the same message, in the card", async () => {
      // The code alphabet has no 0, 1, I or O, so this code can never exist.
      await ivy.click("Open invite");
      const s = await ivy.waitFor(
        "the card error after the resolve",
        (st) => st.onboarding_invite_code_error !== "" && !st.onboarding_invite_code_checking,
        20_000,
      );
      expect(s.onboarding_invite_code_error === NOT_VALID, `the message, got "${s.onboarding_invite_code_error}"`);
      expect(s.onboarding_invite_error === "", "nothing above the crews: the message stays in the card");
      expect(!s.join_crew_modal_open, "no join modal");
      expect(s.onboarding_step === 1, "still step 1");
      expect(await ivy.text(NOT_VALID), "the message is on screen");
      await ivy.checkpoint("card-unknown-code");
    });

    await step("ivy: step 1 is the way forward: create a crew and reach the app", async () => {
      await onboardWithNewCrew(ivy, crew, name);
      const s = await ivy.state();
      expect(s.crews.length === 1 && s.crews[0] === crew, `only the new crew, got: ${s.crews.join(", ")}`);
      expect(!s.join_crew_modal_open, "no join modal in the app");
      await ivy.checkpoint("in-app");
    });
  },
});

// After a logout the machine has a device account and the user has no
// session. The card takes the same path as on a fresh install. Finalize
// device-auths into the existing account and joins the crew by its code.
export const afterLogout = journey({
  id: "invite.code-card-after-logout",
  flows: ["INV-09", "AUTH-02"],
  async run({ runId, launch, step, expect }) {
    const aliceCrew = `Night Owls ${runId}`;
    const hankCrew = `Return ${runId}`;
    const aliceName = `alice${runId}`;
    const hankName = `hank${runId}`;

    const alice = await launch("alice");
    let link = "";
    await step("alice: creates a crew and reads the invite link", async () => {
      await onboardWithNewCrew(alice, aliceCrew, aliceName);
      await alice.click("Share invite link");
      link = (await alice.find("Invite link")).value;
      expect(/\/join\/[A-Z0-9-]+$/.test(link), `the invite link field shows a join URL, got "${link}"`);
    });

    const hank = await launch("hank");
    let hankUserId = "";
    await step("hank: onboards with his own crew, then logs out", async () => {
      await onboardWithNewCrew(hank, hankCrew, hankName, "Public");
      hankUserId = (await hank.state()).user_id;
      await hank.click("Account menu");
      await hank.activate("Log Out"); // a popup item: see App.activate
      await hank.waitFor("step 1", (s) => s.screen === "onboarding" && s.onboarding_step === 1 && !s.logged_in);
      await hank.settle(["DiscoverCrewsLoaded", "DeviceAuthed"]);
      expect(await hank.text("GOT AN INVITE?"), "the card shows after a logout");
      await hank.checkpoint("step1-after-logout");
    });

    await step("hank: types alice's link into the card: the welcome screen names alice and the crew", async () => {
      await hank.type("GOT AN INVITE?", link);
      await hank.click("Open invite");
      await inviteWelcome(hank, aliceCrew, aliceName);
      expect(!(await hank.state()).join_crew_modal_open, "no join modal: hank has no session");
      await hank.checkpoint("welcome");
    });

    await step("hank: Join, profile, and step 3 end in the app with alice's crew", async () => {
      await joinFromWelcome(hank, aliceCrew);
      await profileAtStep2(hank, hankName, 3);
      await linkEmailAtStep3(hank, hankName);
      const s = await hank.waitFor(`${aliceCrew} in hank's crews`, (st) => st.crews.includes(aliceCrew), 20_000);
      expect(s.user_id === hankUserId, `the same account as before the logout, got ${s.user_id} instead of ${hankUserId}`);
      await hank.checkpoint("hank-in-app");
    });

    await step("alice: sees hank in her crew", async () => {
      await alice.waitFor(`${hankName} in alice's member list`, (s) => s.members.includes(hankName));
    });
  },
});

export default typedLink;
