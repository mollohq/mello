// FEED-12: a game clip (CLIPS.md 15.13). bob is in voice and plays
// a game. He presses F8, and the crew feed shows the clip card with its
// poster. The video goes up, and alice plays the clip, pauses it and seeks.
//
// A journey cannot start a real game. bob's game comes through the e2e-only
// fake sensor (POST /fake-game), so the replay rules run their normal code. A
// fake game has no window, so the replay records the reference clip of
// mello_core::e2e_game in its place. F8 comes through POST /game-clip-key,
// which the poll loop takes as it takes the real key.
//
// The upload goes to the local MinIO of the stack, through the branch's
// game clip RPCs. alice plays the uploaded MP4, not bob's local file.

import type { AppState } from "../../tools/mello-driver/src/app.ts";
import { journey } from "../../tools/mello-driver/src/journey.ts";
import { twoUsersInOneCrew } from "./lib/setup.ts";
import { joinVoice } from "./lib/voice.ts";

const CHANNEL = "General";
const GAME_ID = "rocket-league";
// The clip holds what the ring recorded. The journey presses F8 this long
// after the replay starts, so the clip has about this much footage.
const RECORD_MS = 8_000;
// A clip is at most 30 s (CLIPS.md 15.2).
const MAX_CLIP_MS = 31_000;
const FAILURES = ["GameClipFailed", "GameClipUploadFailed"];

type Card = AppState["feed_cards"][number];

function gameClip(s: AppState): Card | undefined {
  return s.feed_cards.find((c) => c.card_type === "clip" && c.clip_kind === "game");
}

export default journey({
  id: "feed.game-clip",
  flows: ["FEED-12"],
  voice: true,
  async run(ctx) {
    const { step, expect } = ctx;
    const { alice, bob, bobName } = await twoUsersInOneCrew(ctx);
    let recordingSince = 0;
    let durationMs = 0;

    await step(`bob joins ${CHANNEL} and plays: the replay records`, async () => {
      await joinVoice(bob, CHANNEL);
      await bob.fakeGame("start", GAME_ID);
      const rec = await bob.waitForEvent(
        "the replay records the fake game",
        (e) => e.type === "GameClipStateChanged" && e.game_clip_state === "recording",
        20_000,
      );
      recordingSince = rec.ts_ms;
    });

    await step("bob presses F8: the clip saves and its card shows with the local poster", async () => {
      // The clip length is the recorded time: wait on it, then press.
      await bob.waitFor(`${RECORD_MS} ms of footage in the ring`, () => Date.now() - recordingSince >= RECORD_MS, RECORD_MS + 5_000);
      await bob.gameClipKey();
      const saved = await bob.waitForEvent("the game clip saves", (e) => e.type === "GameClipCaptured" || FAILURES.includes(e.type), 20_000);
      expect(saved.type === "GameClipCaptured", `the clip saves, got ${JSON.stringify(saved)}`);
      const s = await bob.waitFor("bob's feed shows the game clip card", (st) => gameClip(st) !== undefined, 20_000);
      const card = gameClip(s)!;
      expect(card.clip_poster_path !== "", `bob's card shows the local poster, got ${JSON.stringify(card)}`);
      expect(card.clip_can_play, `bob has the clip file, so his card can play, got ${JSON.stringify(card)}`);
      await bob.checkpoint("game-clip-card-clipper");
    });

    await step("the video goes up: alice sees the card, ready to play", async () => {
      const s = await alice.waitFor("alice's feed shows the uploaded game clip", (st) => {
        const c = gameClip(st);
        return c !== undefined && !c.clip_uploading && c.clip_can_play;
      }, 90_000);
      const card = gameClip(s)!;
      expect(card.actor_name === bobName, `the clipper is bob, got ${card.actor_name}`);
      expect(card.clip_poster_path === "", `alice has no local poster, got ${card.clip_poster_path}`);
      await alice.checkpoint("game-clip-card-crew");
    });

    await step("alice plays the clip: the player opens and the clock moves", async () => {
      await alice.click(`Play the game clip of ${bobName}`);
      const s = await alice.waitFor(
        "the player plays past 0.5 s",
        (st) => st.game_clip_player.open && !st.game_clip_player.buffering && st.game_clip_player.position_ms >= 500,
        10_000,
      );
      const p = s.game_clip_player;
      expect(p.error === "", `the clip plays, got the error "${p.error}"`);
      durationMs = p.duration_ms;
      expect(durationMs >= RECORD_MS - 2_000 && durationMs <= MAX_CLIP_MS, `the clip is about ${RECORD_MS} ms, got ${durationMs} ms`);
      await alice.checkpoint("game-clip-playing");
    });

    await step("alice pauses, then plays on", async () => {
      await alice.click("Pause");
      const paused = await alice.waitFor("the player pauses", (st) => st.game_clip_player.paused, 3_000);
      const at = paused.game_clip_player.position_ms;
      await alice.click("Play");
      await alice.waitFor(
        "the clock moves on after play",
        (st) => !st.game_clip_player.paused && st.game_clip_player.position_ms >= at + 300,
        3_000,
      );
    });

    await step("alice seeks to the middle: the clock jumps there", async () => {
      // A click on the position bar seeks to that fraction; the driver
      // clicks its centre.
      await alice.click("Clip position");
      const mid = durationMs / 2;
      const s = await alice.waitFor(
        "the clock is at the middle",
        (st) => !st.game_clip_player.buffering && Math.abs(st.game_clip_player.position_ms - mid) <= 1_500,
        3_000,
      );
      expect(s.game_clip_player.open, "the player stays open after the seek");
    });

    await step("alice closes the player", async () => {
      await alice.click("Close the clip");
      await alice.waitFor("the player is closed", (st) => !st.game_clip_player.open, 3_000);
    });

    await step("bob's game ends: the replay stops", async () => {
      await bob.fakeGame("end", GAME_ID);
      await bob.waitForEvent("the replay is off", (e) => e.type === "GameClipStateChanged" && e.game_clip_state === "off", 20_000);
    });

    await step("no hidden errors and no clip failures on either side", async () => {
      for (const u of [alice, bob]) {
        const events = await u.events();
        const errors = events.filter((e) => e.message).map((e) => e.message);
        expect(errors.length === 0, `${u.name} has no Error events, got: ${errors.join(" | ")}`);
        const failed = events.filter((e) => FAILURES.includes(e.type) || (e.type === "GameClipPlaybackClosed" && e.reason));
        expect(failed.length === 0, `${u.name} has no game clip failures, got: ${JSON.stringify(failed)}`);
      }
    });
  },
});
