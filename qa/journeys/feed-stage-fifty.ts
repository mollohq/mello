// FEED-07: fifty members, the size of a real crew in production. bob is real,
// 49 are seeded over REST. The feed must load in under three seconds.

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bigCrewStage } from "./lib/stage.ts";

export default journey({
  id: "feed.stage-fifty",
  flows: ["FEED-07"],
  async run(ctx) {
    await bigCrewStage(ctx, 49, "fifty");
  },
});
