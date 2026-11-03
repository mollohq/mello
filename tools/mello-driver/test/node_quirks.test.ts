import assert from "node:assert/strict";
import { test } from "node:test";
import { guardSetTypeOfService } from "../src/node_quirks.ts";

function failing(code: string) {
  const e = new Error(`setTypeOfService ${code}`) as NodeJS.ErrnoException;
  e.code = code;
  return e;
}

test("an EINVAL from setTypeOfService is swallowed, any other error is not", () => {
  let next: NodeJS.ErrnoException | null = failing("EINVAL");
  const proto = {
    setTypeOfService() {
      if (next) throw next;
      return "ok";
    },
  };
  guardSetTypeOfService(proto as never);
  assert.doesNotThrow(() =>
    (proto as { setTypeOfService: () => unknown }).setTypeOfService(),
  );
  next = failing("ENOTSUP");
  assert.throws(
    () => (proto as { setTypeOfService: () => unknown }).setTypeOfService(),
    /ENOTSUP/,
  );
  next = null;
  assert.equal(
    (proto as { setTypeOfService: () => unknown }).setTypeOfService(),
    "ok",
  );
});

test("the guard wraps once and leaves a prototype without the method alone", () => {
  const proto = { setTypeOfService: () => "ok" } as {
    setTypeOfService: () => unknown;
    melloTypeOfServiceGuard?: boolean;
  };
  guardSetTypeOfService(proto as never);
  const wrapped = proto.setTypeOfService;
  guardSetTypeOfService(proto as never);
  assert.equal(proto.setTypeOfService, wrapped, "wrapped once");
  assert.equal(proto.melloTypeOfServiceGuard, true);
  const bare = {} as { setTypeOfService?: unknown };
  guardSetTypeOfService(bare as never);
  assert.equal(
    bare.setTypeOfService,
    undefined,
    "Node 22 has no such method: nothing to wrap",
  );
});
