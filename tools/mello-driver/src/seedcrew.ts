// `cli.ts seed-crew`: join seeded members into a crew of the local stack, for
// a person who tests the feed in a real client (qa/journeys/lib/seed.ts).

import { CARRY_TAKER_PLAYTIME_MIN, crewFeed, joinByCode, profilesFrom, reportLibrary, seedMember, seededDeviceExists, type SeededMember } from "../../../qa/journeys/lib/seed.ts";
import type { SeedCrewArgs } from "./args.ts";
import { dnsLookup, nakamaBase, nakamaTarget, requireLocalHost, type Lookup } from "./guard.ts";

const FIX = "seed-crew only writes to the local stack. Unset NAKAMA_HOST, or point it at a loopback or private address.";

/** The first profile index with no seeded account for this crew, so a rerun adds new names. */
export async function nextIndex(runId: string, nameAt: (i: number) => string, exists: (name: string) => Promise<boolean>): Promise<number> {
  let i = 0;
  while (await exists(nameAt(i))) i++;
  return i;
}

function carryHolder(feed: any): string | undefined {
  const h2h = (feed?.sections ?? []).flatMap((x: any) => x.entries ?? []).find((e: any) => e.type === "head2head");
  return h2h?.data?.titles?.find((t: any) => t.title_id === "the_carry")?.holder_user_id;
}

/** The text that says how to turn dev_fault on. It reads no file. */
export function devFaultHelp(): string {
  return [
    "dev_fault is off on this stack, so the recap is skipped. To turn it on:",
    "  1. printf 'services:\\n  nakama:\\n    environment:\\n      - MELLO_ENABLE_DEV_FAULT=1\\n' > /tmp/m3llo-dev-fault.yml",
    "  2. docker compose -f backend/docker-compose.yml -f /tmp/m3llo-dev-fault.yml \\",
    "       --env-file /Users/bob/dev/m3llo-dev/mello/backend/.env up -d --force-recreate nakama",
    "     (run it in the mello-wt-next-gen-feed worktree, so Nakama serves this branch's module)",
    "  3. run seed-crew again with --recap",
  ].join("\n");
}

export async function seedCrewCommand(args: SeedCrewArgs, resolve: Lookup = dnsLookup): Promise<number> {
  const target = nakamaTarget();
  await requireLocalHost("Nakama (NAKAMA_HOST)", target.host, FIX, resolve);
  const base = nakamaBase(target);
  try {
    const r = await fetch(`${base}/healthcheck`, { signal: AbortSignal.timeout(5_000) });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
  } catch (e) {
    throw new Error(`no local Nakama at ${base} (${e instanceof Error ? e.message : e})`);
  }

  const runId = `crew-${args.code.toLowerCase()}`;
  const names = profilesFrom(0, 200, runId).map((p) => p.name);
  const start = await nextIndex(runId, (i) => names[i], (n) => seededDeviceExists(runId, n));
  if (start + args.members > names.length) throw new Error(`the crew has ${start} seeded members, ${args.members} more are over the limit of ${names.length}`);
  const list = profilesFrom(start, args.members, runId);
  if (args.clips !== undefined) {
    list.forEach((p) => (p.clips = 0));
    for (let c = 0; c < args.clips; c++) list[c % list.length].clips++;
  }

  const members: SeededMember[] = [];
  let crewId = "";
  for (const p of list) {
    members.push(await seedMember(args.code, p.name, p, runId));
  }
  crewId = await joinByCode(members[0].token, args.code);
  // One report after the last join recomputes the crew once with everyone in.
  const last = list[list.length - 1];
  await reportLibrary(members[members.length - 1].token, last.games);

  let beltLine = "belt moved: not asked";
  if (args.belts) {
    const before = carryHolder((await crewFeed(members[0].token, crewId)).feed);
    const taker = members.find((m) => m.userId !== before);
    if (!taker) beltLine = "belt moved: no, the only new member holds the_carry";
    else {
      const own = list.find((p) => p.name === taker.name)!.games;
      const library = own.filter((g) => g.id !== "steam-730").map((g) => ({ id: g.id, name: g.name, playtimeMin: g.playtimeMin }));
      // A rerun takes the belt from the taker of the run before: the playtime grows with the member count.
      library.push({ id: "steam-730", name: "Counter-Strike 2", playtimeMin: CARRY_TAKER_PLAYTIME_MIN + (start + args.members) * 1000 });
      await reportLibrary(taker.token, library);
      const after = carryHolder((await crewFeed(members[0].token, crewId)).feed);
      beltLine = after === taker.userId && after !== before ? `belt moved: yes, the_carry went to ${taker.name}` : "belt moved: no";
    }
  }

  const sessions = list.reduce((n, p) => n + p.sessions.length, 0);
  const clips = list.reduce((n, p) => n + p.clips, 0);
  console.log(`crew ${crewId}`);
  console.log(`members added (${members.length}, ${start} before): ${list.map((p) => p.name).join(", ")}`);
  console.log(`sessions written: ${sessions}, clips written: ${clips}`);
  console.log(beltLine);

  if (args.recap) {
    const r = await fetch(`${base}/v2/rpc/dev_fault`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: `Bearer ${members[0].token}` },
      body: JSON.stringify(JSON.stringify({ action: "generate_recap", crew_id: crewId })),
      signal: AbortSignal.timeout(30_000),
    });
    if (r.ok) console.log("recap: generated");
    else if (r.status === 404 || /disabled/.test(await r.text())) console.log(`recap: skipped\n${devFaultHelp()}`);
    else console.log(`recap: skipped, dev_fault answered HTTP ${r.status}`);
  }
  return 0;
}
