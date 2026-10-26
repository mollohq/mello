// FEED-06: twenty members. bob is real, nineteen are seeded over REST. The
// mockups stop at five, so this applies the plan and spec rules for a large
// crew (lib/stage.ts, bigCrewStage).

import { journey } from "../../tools/mello-driver/src/journey.ts";
import { bigCrewStage } from "./lib/stage.ts";

export default journey({
  id: "feed.stage-twenty",
  flows: ["FEED-06"],
  async run(ctx) {
    await bigCrewStage(ctx, 19, "twenty");
  },
});
