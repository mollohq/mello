// Frame cost of the sealed belt reveal in the real client (spec
// features/D2D-RENDERER.md section 11, P4). Not part of the e2e suite. The
// setup is the one of qa/journeys/feed-sealed-reveal.ts; then bob opens the
// belt and holds it at rest, embers running, for HOLD seconds. Run it with
// FX_D2D_STATS=1 and read bob's app.log: one stats line per second.
//
//   MELLO_BIN=target/release/mello.exe MELLO_RENDERER=d2d FX_D2D_STATS=1 //     node tools/mello-driver/src/cli.ts run scripts/perf/reveal-frame-cost.ts

import { setTimeout as sleep } from "node:timers/promises";

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { CARRY_TAKER_PLAYTIME_MIN, generateRecap, profiles, reportLibrary } from "../../qa/journeys/lib/seed.ts";
import { bobWithSeededCrew } from "../../qa/journeys/lib/stage.ts";

export default journey({
  id: "perf.reveal-frame-cost",
  flows: [],
  async run(ctx) {
    const { step, expect, runId } = ctx;
    const { bob, members } = await bobWithSeededCrew(ctx, 4);

    await step("nothing is sealed before a belt moves", async () => {
      await bob.restart();
      const s = await bob.waitFor("the wide card after a reopen", (st) => st.head2head.present && st.head2head.pairs_count === 4 && st.head2head.titles.length > 0, 30_000);
      expect(s.sealed.belts === 0, `sealed.belts is 0 before the move, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `open_center is none, got ${s.sealed.open_center}`);
    });

    await step("a member re-reports a library that takes the_carry: one belt is sealed", async () => {
      const s0 = await bob.state();
      const holder = s0.head2head.titles.find((t) => t.title_id === "the_carry")?.holder_name;
      const taker = members.find((m) => m.name !== holder);
      expect(taker !== undefined, `a seeded member other than the holder ${holder} exists`);
      const own = profiles(members.length, runId).find((p) => p.name === taker!.name)?.games ?? [];
      const library = own.filter((g) => g.id !== "steam-730").map((g) => ({ id: g.id, name: g.name, playtimeMin: g.playtimeMin }));
      library.push({ id: "steam-730", name: "Counter-Strike 2", playtimeMin: CARRY_TAKER_PLAYTIME_MIN });
      await reportLibrary(taker!.token, library);
      const s = await bob.waitFor(
        "the_carry held by the taker and one sealed belt",
        (st) => st.head2head.titles.some((t) => t.title_id === "the_carry" && t.holder_name === taker!.name) && st.sealed.belts >= 1,
        60_000,
      );
      expect(s.sealed.belts === 1, `sealed.belts is 1, got ${s.sealed.belts}`);
      expect(s.sealed.open_center === "none", `nothing in the centre, got ${s.sealed.open_center}`);
      await bob.checkpoint("sealed-in-the-feed");
    });

    await step("a click on the sealed chip opens the belt in the centre", async () => {
      // The chip may stand below the fold of the feed.
      await bob.scrollToControl("Sealed belt, open it");
      await bob.click("Sealed belt, open it");
      const s = await bob.waitFor("the belt in the centre", (st) => st.sealed.open_center === "belt", 5_000);
      expect(s.sealed.belts === 0, `the belt in the centre is no longer sealed in the feed, got ${s.sealed.belts}`);
      await bob.checkpoint("reveal-start");
      // The face shows from 1.03 s and the card rests from 1.25 s.
      await bob.waitFor("the card at rest, the face shown", (st) => st.sealed.resting, 5_000);
      await bob.checkpoint("reveal-face");
    });

    await step("the belt at rest with its embers, held", async () => {
      await sleep(Number(process.env.MELLO_PERF_HOLD_MS ?? 10_000));
      await bob.checkpoint("reveal-rest");
    });
  },
});
