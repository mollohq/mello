// A fake local stack for the driver tests: the parts of Nakama and the lounge
// that the preflight and the production guards (src/guard.ts) call.

import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";

export type FakeServer = { server: Server; port: number; url: string; requests: string[]; close(): void };

async function listen(handler: (req: IncomingMessage, body: string, res: ServerResponse) => void): Promise<FakeServer> {
  const requests: string[] = [];
  const server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      requests.push(`${req.method} ${req.url}`);
      handler(req, body, res);
    });
  });
  await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
  const port = (server.address() as any).port;
  return { server, port, url: `http://127.0.0.1:${port}`, requests, close: () => server.close() };
}

export type FakeNakamaOptions = {
  /** The SFU regions that get_sfu_regions hands out: region name to health URL. */
  regions?: Record<string, string>;
  /** The invite code that create_invite_code makes. */
  inviteCode?: string;
};

/** A Nakama that answers the preflight: health, device sign-in, SFU regions, and a crew invite. */
export function fakeNakama(opts: FakeNakamaOptions = {}): Promise<FakeServer> {
  const regions = opts.regions ?? { "eu-west": "http://127.0.0.1:8080/health" };
  const rpc = (res: ServerResponse, payload: unknown) => res.end(JSON.stringify({ payload: JSON.stringify(payload) }));
  return listen((req, _body, res) => {
    res.setHeader("Content-Type", "application/json");
    const path = (req.url ?? "").split("?")[0];
    if (path === "/healthcheck") return res.end("{}");
    if (path === "/v2/account/authenticate/device") return res.end(JSON.stringify({ token: "guard-token" }));
    if (path === "/v2/account") return res.end(JSON.stringify({ user: { id: "guard-user" } }));
    if (path === "/v2/user/guard-user/group") return res.end(JSON.stringify({ user_groups: [{ group: { id: "guard-crew" } }] }));
    if (path === "/v2/rpc/get_sfu_regions") {
      return rpc(res, { regions: Object.entries(regions).map(([region, health_url]) => ({ region, health_url })) });
    }
    if (path === "/v2/rpc/create_invite_code") return rpc(res, { code: opts.inviteCode ?? "GARD-0001" });
    res.statusCode = 404;
    res.end(JSON.stringify({ error: "not found" }));
  });
}

export type FakeLoungeOptions = {
  /** The invite code that the lounge knows. Any other code gets a 404, as from another Nakama. */
  knows?: string;
  /** The Nakama address in the boot data for guest voice. */
  voiceUrl?: string;
};

/** A lounge whose invite page carries the lounge-boot JSON, as mello-site's functions/join/[code].ts. */
export function fakeLounge(opts: FakeLoungeOptions = {}): Promise<FakeServer> {
  return listen((req, _body, res) => {
    const code = decodeURIComponent((req.url ?? "").replace(/^\/join\//, ""));
    if (code !== (opts.knows ?? "GARD-0001")) {
      res.statusCode = 404;
      return res.end("<html>not found</html>");
    }
    const boot = { voice: opts.voiceUrl ? { url: opts.voiceUrl, key: "k" } : null, crew_name: "e2e guard" };
    res.setHeader("Content-Type", "text/html");
    res.end(`<html><script type="application/json" id="lounge-boot">${JSON.stringify(boot)}</script></html>`);
  });
}
