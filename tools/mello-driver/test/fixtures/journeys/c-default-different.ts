// Fixture: the default export is a different object from the named export.
const run = async () => {};

export const named = { id: "fixture.default-different.named", flows: ["F-05"], run };
export default { id: "fixture.default-different.default", flows: ["F-06"], knownIssues: [1], run };
