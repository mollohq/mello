// Node 24 ships an undici that calls `socket.setTypeOfService()` on every
// HTTP/1.1 request it writes (the method came in Node 24.15). On macOS the
// kernel refuses that option on a socket whose peer has just reset the
// connection: EINVAL, thrown from a socket callback where no try/catch around
// `fetch()` reaches it, and the driver dies in the middle of a journey ("the
// driver wrote no result.json"). The app a journey restarts resets such
// sockets. Fixed upstream in undici 8.8.0 (nodejs/undici#5547), which no Node
// 24 ships. Until then the driver swallows that one error, as the upstream fix
// does, and no other.

import net from "node:net";

type SocketProto = {
  setTypeOfService?: (this: net.Socket, ...args: unknown[]) => unknown;
  melloTypeOfServiceGuard?: boolean;
};

/** Wrap `net.Socket.prototype.setTypeOfService` so an EINVAL is ignored. Runs once. */
export function guardSetTypeOfService(
  proto: SocketProto = net.Socket.prototype as unknown as SocketProto,
): void {
  const orig = proto.setTypeOfService;
  if (!orig || proto.melloTypeOfServiceGuard) return;
  proto.setTypeOfService = function (this: net.Socket, ...args: unknown[]) {
    try {
      return orig.apply(this, args);
    } catch (e) {
      if ((e as NodeJS.ErrnoException | undefined)?.code === "EINVAL")
        return this;
      throw e;
    }
  };
  proto.melloTypeOfServiceGuard = true;
}
