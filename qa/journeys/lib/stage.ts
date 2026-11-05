// Crew stages as journeys (FEED-04 to FEED-07): one real viewer in a crew,
// the rest of the crew seeded over REST (seed.ts).

import type { App, AppEvent, AppState } from "../../../tools/mello-driver/src/app.ts";
import type { JourneyContext } from "../../../tools/mello-driver/src/journey.ts";
import { onboardWithNewCrew } from "./onboarding.ts";
import { readInviteCode } from "./setup.ts";
import {
  CARRY_SECOND_TAKER_PLAYTIME_MIN,
  CARRY_TAKER_PLAYTIME_MIN,
  crewFeed,
  endSession,
  profiles,
  reportLibrary,
  seedCrew,
  signInEmail,
  type SeededMember,
} from "./seed.ts";

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
    await bob.waitFor("the solo board of a crew of one", (s) => s.feed_zones.board === "solo", 30_000);
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

/** The belt of the_carry on bob's board. */
export function carryBelt(s: AppState): AppState["feed_zones"]["belts"][number] | undefined {
  return s.feed_zones.belts.find((t) => t.title === "THE CARRY");
}

/** The holder of the_carry on bob's board, by name. */
function carryHolder(s: AppState): string | undefined {
  return carryBelt(s)?.holder_name;
}

/**
 * Seeded members re-report their libraries until the last move of the_carry is
 * between two members other than bob: the belt of the board is sealed and
 * unread, and the Stage has no belt of bob's. Returns the new holder.
 *
 * bob's app reports the library of the machine it runs on. With more Counter-
 * Strike 2 hours there than any seeded member has, bob holds the_carry, and
 * one move would take it from him. Then a first member takes it from bob and a
 * second member takes it from the first.
 */
export async function moveTheCarryBetweenOthers(ctx: JourneyContext, bob: App, members: SeededMember[]): Promise<SeededMember> {
  const { runId, expect } = ctx;
  const take = async (taker: SeededMember, playtimeMin: number) => {
    // The taker keeps the library the seed gave them and plays the shared top
    // game far more than anyone.
    const own = profiles(members.length, runId).find((p) => p.name === taker.name)?.games ?? [];
    const library = own.filter((g) => g.id !== "steam-730").map((g) => ({ id: g.id, name: g.name, playtimeMin: g.playtimeMin }));
    library.push({ id: "steam-730", name: "Counter-Strike 2", playtimeMin });
    await reportLibrary(taker.token, library);
    await bob.waitFor(`the_carry held by ${taker.name}`, (st) => carryHolder(st) === taker.name, 60_000);
  };
  const holder = carryHolder(await bob.state());
  expect(holder !== undefined, "the_carry has a holder before the move");
  if (members.some((m) => m.name === holder)) {
    const taker = members.find((m) => m.name !== holder);
    expect(taker !== undefined, `a seeded member other than the holder ${holder} exists`);
    await take(taker!, CARRY_TAKER_PLAYTIME_MIN);
    return taker!;
  }
  // bob holds it, from the library of this machine.
  expect(members.length >= 2, `two seeded members to move the_carry between, got ${members.length}`);
  const [first, second] = members;
  await take(first, CARRY_TAKER_PLAYTIME_MIN);
  await take(second, CARRY_SECOND_TAKER_PLAYTIME_MIN);
  return second;
}

/** ms between two events of a run, or null when one is missing. */
export function between(events: AppEvent[], from: string, to: string): number | null {
  const a = events.find((e) => e.type === from);
  const b = [...events].reverse().find((e) => e.type === to);
  return a && b ? b.ts_ms - a.ts_ms : null;
}

/** Every card of the zones: the Stage, the clips and the sessions. */
export function zoneCards(s: AppState): AppState["feed_zones"]["clips"] {
  const z = s.feed_zones;
  return [...(z.stage_card ? [z.stage_card] : []), ...z.clips, ...z.sessions];
}

/**
 * Wait for the feed a crew of two or more shows once the library report and
 * the contested titles have arrived: the board with its belts. The board
 * comes first and the belts lag it.
 */
export async function waitForFullFeed(
  app: { waitFor: (label: string, p: (s: AppState) => boolean, ms: number) => Promise<AppState> },
): Promise<AppState> {
  return app.waitFor(
    "the full feed: the board with belts",
    (s) => s.feed_zones.board !== "" && s.feed_zones.board !== "solo" && s.feed_zones.belts.length > 0,
    15_000,
  );
}

/** The checks that every stage from two members on shares: the board and its belts, no empty card. */
export function expectFullFeed(s: AppState, expect: (c: unknown, m: string) => void): void {
  const z = s.feed_zones;
  expect(z.board !== "" && z.board !== "solo", `the board of a crew shows, got "${z.board}"`);
  expect(z.belts.length > 0, "contested titles exist");
  expect(z.belts.every((b) => b.holder_name !== ""), "every belt names its holder");
  for (const c of zoneCards(s)) {
    expect(c.card_type !== "", `card ${c.id} has a type`);
    expect(c.title !== "" || c.actor_name !== "" || c.game_name !== "" || c.card_type === "head2head",
      `card ${c.id} (${c.card_type}) is not empty`);
  }
}

export { crewFeed };

/**
 * A crew of `seeded + 1` (ten or more): the People zone has no open seats and
 * no invite, the board holds the belts (seven at most), the clips are one
 * carousel, and the feed loads in under three seconds. Spec
 * features/CREW-FEED-ZONES.md.
 */
export async function bigCrewStage(ctx: JourneyContext, seeded: number, label: string): Promise<void> {
  const { step, expect } = ctx;
  const total = seeded + 1;
  const { bob, members } = await bobWithSeededCrew(ctx, seeded);

  await step(`the board holds the belts of ${total} members, seven at most`, async () => {
    const s = await waitForFullFeed(bob);
    const z = s.feed_zones;
    expect(z.belts.length >= 1 && z.belts.length <= 7, `one to seven belts on the board, got ${z.belts.length}`);
    expect(z.belt_count >= z.belts.length, `belt_count counts every belt, got ${z.belt_count}`);
    expectFullFeed(s, expect);
  });

  await step("no open seat and no invite from ten members", async () => {
    const s = await bob.state();
    expect(s.feed_zones.people.open_seats === 0, `no open seat, got ${s.feed_zones.people.open_seats}`);
    expect(s.feed_zones.people.members <= 5, `the first five at most, got ${s.feed_zones.people.members}`);
    const labels = (await bob.controls()).map((c) => c.label);
    expect(!labels.includes("Invite your crew"), "no red invite in the feed");
  });

  await step("the clips of the crew are one carousel", async () => {
    const s = await bob.waitFor("the clips", (st) => st.feed_zones.clips.length > 0 || st.feed_zones.stage === "card", 30_000);
    for (const c of s.feed_zones.clips) expect(c.card_type === "clip", `the carousel holds clips, got ${c.card_type}`);
  });

  await step("the feed loads in under three seconds", async () => {
    const events = await bob.events();
    // From the crew list (the app knows the crew) to the last of the feed and
    // the member list, which together draw the board and the People zone.
    const t0 = events.find((e) => e.type === "CrewsLoaded")?.ts_ms;
    const t1 = Math.max(...events.filter((e) => e.type === "FeedLoaded" || e.type === "CrewStateLoaded").map((e) => e.ts_ms));
    const ms = t0 === undefined || !Number.isFinite(t1) ? null : t1 - t0;
    const viewer = members[0];
    const crewId = (await bob.state()).active_crew_id;
    const rpc = await crewFeed(viewer.token, crewId);
    const pairs = rpc.feed.zones?.head2head?.pairs?.length ?? 0;
    console.log(
      `${label}: crew_feed RPC ${rpc.ms} ms, ${pairs} pairs in the payload, ` +
        `app CrewsLoaded to the last FeedLoaded or CrewStateLoaded ${ms ?? "n/a"} ms`,
    );
    expect(rpc.ms < 3000, `crew_feed takes ${rpc.ms} ms, over 3000`);
    expect(ms === null || ms < 3000, `the app takes ${ms} ms from the crew to the feed, over 3000`);
    await bob.checkpoint(`${label}-feed`);
  });
}
