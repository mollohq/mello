// Fixture: two named journeys, one with known issues, and exports that are not journeys.
const run = async () => {};

export const first = { id: "fixture.named.first", flows: ["F-01"], run };
export const second = { id: "fixture.named.second", flows: ["F-02", "F-03"], knownIssues: [7, 9], run };
export const NOT_A_JOURNEY = { id: "fixture.helper" };
export const label = "also not a journey";
