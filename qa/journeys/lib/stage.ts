// Crew stages as journeys (FEED-04 to FEED-07): one real viewer in a crew,
// the rest of the crew seeded over REST (seed.ts).

import type { App, AppEvent, AppState } from "../../../tools/mello-driver/src/app.ts";
import type { JourneyContext } from "../../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./onboarding.ts";
import { readInviteCode } from "./setup.ts";
import { crewFeed, endSession, profiles, seedCrew, signInEmail, type SeededMember } from "./seed.ts";

export type Stage = { bob: App; crew: string; code: string; members: SeededMember[] };

/**
 * bob onboards with a new crew and shares the invite. `seeded` members join
 * over REST. bob then starts the app again, as a returning user opens the
 * crew: the member list and the feed load for the full crew.
 */
export async function bobWithSeededCrew(ctx: JourneyContext, seeded: number): Promise<Stage> {
  const { runId, launch, step, expect } = ctx;
  const crew = `Stage ${runId}`;
  const bob = await launch("bob");
  let code = "";
  let bobCreds = { email: "", password: "" };
  await step("setup: bob creates a crew and reads the invite link", async () => {
    bobCreds = await onboardWithNewCrew(bob, crew, `bob${runId}`, "Public");
    await bob.waitFor("the head2head card of a crew of one", (s) => s.head2head.present, 30_000);
    code = await readInviteCode(bob);
  });
  await step("setup: bob has played a few sessions, so his pairs contest more than the library titles", async () => {
    // The real app on this machine has a library and no sessions. A member
    // with no sessions contends for no session title, so every pair with bob
    // would tally on the three library titles at most (the carry, the
    // collector, one trick). Bob's sessions go in as bob, over REST.
    const token = await signInEmail(bobCreds.email, bobCreds.password);
    const crewId = (await bob.state()).active_crew_id;
    const played = [
      [{ id: "steam-730", name: "Counter-Strike 2" }, 95, 23 * 60, 1],
      [{ id: "steam-730", name: "Counter-Strike 2" }, 60, 22 * 60, 3],
      [{ id: "steam-291550", name: "Brawlhalla" }, 7, 21 * 60, 4],
      [{ id: "steam-252950", name: "Rocket League" }, 140, 20 * 60, 6],
    ] as const;
    for (let i = 0; i < played.length; i++) {
      const [game, min, endLocal, daysAgo] = played[i];
      await endSession(token, crewId, game, min, endLocal, `stage-${runId}-bob-${i}`, daysAgo);
    }
  });
  let members: SeededMember[] = [];
  await step(`setup: ${seeded} members join over REST with games, sessions and clips`, async () => {
    members = await seedCrew(code, profiles(seeded, runId), runId);
    expect(members.length === seeded, `seeded ${seeded}, got ${members.length}`);
  });
  await step("setup: bob opens the app again on the full crew", async () => {
    await bob.restart();
    await bob.waitFor("the app, logged in", (s) => s.screen === "app" && s.logged_in, 30_000);
  });
  return { bob, crew, code, members };
}

/** ms between two events of a run, or null when one is missing. */
export function between(events: AppEvent[], from: string, to: string): number | null {
  const a = events.find((e) => e.type === from);
  const b = [...events].reverse().find((e) => e.type === to);
  return a && b ? b.ts_ms - a.ts_ms : null;
}

/**
 * Wait for the feed a crew of two or more shows once the library report and
 * the contested titles have arrived: head2head, recent games tiles, titles and
 * no skeleton. `present` alone comes first and lags the content.
 */
export async function waitForFullFeed(app: { waitFor: (label: string, p: (s: AppState) => boolean, ms: number) => Promise<AppState> }): Promise<AppState> {
  return app.waitFor(
    "the full feed: head2head with titles, recent_games tiles, skeleton_trio 0",
    (s) => s.head2head.present && s.head2head.titles.length > 0 && s.recent_games.tiles.length > 0 && s.skeleton_trio === 0,
    15_000,
  );
}

/** The checks that every stage from two members on shares: no skeleton, no empty card. */
export function expectFullFeed(s: AppState, expect: (c: unknown, m: string) => void): void {
  expect(s.skeleton_trio === 0, `no skeletons from two members on, got ${s.skeleton_trio}`);
  for (const c of s.feed_cards) {
    expect(c.card_type !== "", `feed card ${c.id} has a type`);
    expect(c.title !== "" || c.actor_name !== "" || c.card_type === "head2head" || c.card_type === "invite" || c.card_type === "recent-games",
      `feed card ${c.id} (${c.card_type}) is not empty`);
  }
  expect(s.head2head.present, "the head2head card is there");
  expect(s.recent_games.tiles.length > 0, "the recent games card has tiles");
  expect(s.head2head.titles.length > 0, "contested titles exist");
}

export { crewFeed };

/**
 * A crew of `seeded + 1`: head2head is a cell with no seats that draws the
 * viewer's three closest pairs and a "+N more" line, the invite card shows
 * eight octagons and a "+N", recent games keeps six tiles, and the feed loads
 * in under three seconds. Plan next-gen-feed.md section 3 and spec HEAD2HEAD
 * section 5, rules for large crews.
 */
export async function bigCrewStage(ctx: JourneyContext, seeded: number, label: string): Promise<void> {
  const { step, expect } = ctx;
  const total = seeded + 1;
  const { bob, members } = await bobWithSeededCrew(ctx, seeded);

  await step(`head2head is a cell with no seats over ${total} members and ${total - 1} pairs`, async () => {
    const s = await bob.waitFor(
      `head2head for ${total} members`,
      (st) => st.head2head.member_count === total && st.head2head.pairs_count === total - 1,
      30_000,
    );
    const h = s.head2head;
    expect(h.placement === "cell" || h.placement === "versus", `placement is cell or versus, got ${h.placement}`);
    expect(h.open_seats === 0, `open_seats is 0, got ${h.open_seats}`);
    if (h.placement === "cell") {
      expect(h.pair_rows === 4, `the cell draws the four closest pairs, got ${h.pair_rows}`);
      expect(h.more_pairs === total - 1 - 4, `the +N more line says ${total - 5}, got ${h.more_pairs}`);
      expect((await bob.find(`+${total - 5} more`)).role === "Button", "the +N more line is a control");
    }
  });

  await step("the invite card shows eight octagons and a +N", async () => {
    const s = await bob.waitFor("the invite card with eight octagons", (st) => st.invite_card.present && st.invite_card.octagons === 8, 15_000);
    expect(s.invite_card.present, "the invite card shows");
    expect(s.invite_card.octagons === 8, `eight octagons, got ${s.invite_card.octagons}`);
    expect(s.invite_card.more_count === total - 8, `+${total - 8}, got ${s.invite_card.more_count}`);
    await bob.waitFor("the octagons wear their avatars", (st) => st.invite_card.with_avatar === 8, 20_000);
  });

  await step("recent games stays at six tiles at most, and no card is empty", async () => {
    const s = await waitForFullFeed(bob);
    expect(s.recent_games.tiles.length >= 1 && s.recent_games.tiles.length <= 6, `at most six tiles, got ${s.recent_games.tiles.length}`);
    expectFullFeed(s, expect);
  });

  await step("recent games is in the first two rows and no clip is in a wide slot", async () => {
    const s = await bob.state();
    const types = s.feed_cards.map((c) => c.card_type);
    const at = types.indexOf("recent-games");
    // Plain grid: head2head, invite and one card, then one card and the wide
    // slot (client/src/feed_grid.rs). Cards 0 to 4 are the first two rows.
    expect(at >= 0 && at <= 4, `recent games is in the first two rows, at card ${at} of ${types.join(",")}`);
    const wide = 4;
    expect(types[wide] !== "clip", `no clip in the wide slot, card ${wide} is ${types[wide]}`);
  });

  await step("the feed loads in under three seconds", async () => {
    const events = await bob.events();
    // From the crew list (the app knows the crew) to the last of the feed and
    // the member list, which together draw the full head2head and invite cards.
    const t0 = events.find((e) => e.type === "CrewsLoaded")?.ts_ms;
    const t1 = Math.max(...events.filter((e) => e.type === "FeedLoaded" || e.type === "CrewStateLoaded").map((e) => e.ts_ms));
    const ms = t0 === undefined || !Number.isFinite(t1) ? null : t1 - t0;
    const viewer = members[0];
    const crewId = (await bob.state()).active_crew_id;
    const rpc = await crewFeed(viewer.token, crewId);
    const h2h = rpc.feed.sections.flatMap((x: any) => x.entries).find((e: any) => e.type === "head2head");
    console.log(
      `${label}: crew_feed RPC ${rpc.ms} ms, ${h2h?.data?.pairs?.length ?? 0} pairs in the payload, ` +
        `app CrewsLoaded to the last FeedLoaded or CrewStateLoaded ${ms ?? "n/a"} ms`,
    );
    expect(rpc.ms < 3000, `crew_feed takes ${rpc.ms} ms, over 3000`);
    expect(ms === null || ms < 3000, `the app takes ${ms} ms from the crew to the feed, over 3000`);
    await bob.checkpoint(`${label}-feed`);
  });
}
