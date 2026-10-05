// The headless browser for the parts of a journey that happen outside the
// app: a provider's consent page (plans/E2E-QA.md §8) and the web
// lounge. Playwright loads on first use, so journeys without a browser step
// do not need it installed.

import { existsSync, readFileSync, rmSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

import { DriverError, FAKE_OAUTH, type App } from "./app.ts";

/** What the fake provider does at the next consent (tools/fake-oauth). */
export type OAuthOutcome = "approve" | "deny" | "wrong_state" | "no_callback" | "reject_token" | "down";

let browserPromise: Promise<any> | null = null;

async function browser(): Promise<any> {
  browserPromise ??= import("playwright").then(({ chromium }) => chromium.launch({ headless: true }));
  return browserPromise;
}

/** Close the shared browser. The runner calls this when a run ends. */
export async function closeBrowser(): Promise<void> {
  if (!browserPromise) return;
  const b = await browserPromise.catch(() => null);
  browserPromise = null;
  await b?.close();
}

/** True when the e2e Docker profile's fake provider answers. */
export async function fakeOAuthUp(): Promise<boolean> {
  try {
    return (await fetch(`${FAKE_OAUTH}/healthz`)).ok;
  } catch {
    return false;
  }
}

// The app's OAuth callback server has one fixed port (29405), so two users
// cannot be in a browser flow at the same time. Flows queue here.
let oauthQueue: Promise<unknown> = Promise.resolve();

/**
 * Complete a sign-in in the browser after the journey clicked a provider
 * button in the app. The fake is told the outcome first; the consent page is
 * then approved or denied like a user does.
 *
 * The app writes the auth URL to its browser file instead of opening the
 * system browser (`e2e-oauth` feature, MELLO_E2E_BROWSER_FILE).
 */
export function completeOAuth(
  app: App,
  opts: { outcome?: OAuthOutcome; identity?: string; timeoutMs?: number } = {},
): Promise<void> {
  const run = async () => {
    const outcome = opts.outcome ?? "approve";
    const res = await fetch(`${FAKE_OAUTH}/_control/next`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ outcome, identity: opts.identity ?? "" }),
    });
    if (res.status !== 204) throw new DriverError(`fake-oauth rejected the script: ${res.status} ${await res.text()}`);

    const url = await waitForBrowserUrl(app, opts.timeoutMs ?? 15_000);
    const page = await (await browser()).newPage();
    try {
      await page.goto(url);
      await page.getByRole("button", { name: outcome === "deny" ? "Deny" : "Approve" }).click();
      // The implicit flows finish in the app's own callback page, whose script
      // posts the fragment token back to the app. Let it run.
      await page.waitForLoadState("networkidle").catch(() => undefined);
    } finally {
      await page.close();
    }
  };
  const next = oauthQueue.then(run, run);
  oauthQueue = next.catch(() => undefined);
  return next;
}

async function waitForBrowserUrl(app: App, timeoutMs: number): Promise<string> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(app.browserFile)) {
      const url = readFileSync(app.browserFile, "utf8").trim();
      if (url) {
        rmSync(app.browserFile);
        return url;
      }
    }
    await sleep(100);
  }
  throw new DriverError(`${app.name}: the app did not start a browser sign-in within ${timeoutMs} ms`);
}

// ── The web lounge (mello-site/lounge) ─────────────────────────────

/** The lounge of the local stack: `npm run dev` in mello-site. */
export const LOUNGE = process.env.MELLO_E2E_LOUNGE_URL ?? "http://localhost:8788";

/** True when the local lounge answers. */
export async function loungeUp(): Promise<boolean> {
  try {
    return (await fetch(`${LOUNGE}/join/NONE-0000`)).status < 500;
  } catch {
    return false;
  }
}

/** What the lounge did when the guest pressed its install button. */
export type LoungeDownload = {
  /** The installer URL the browser asked for. It gets an empty file. */
  installer: string;
  /** The text the lounge put on the clipboard, or "" when it put none. */
  copied: string;
  /** The gate shows "Your invite is copied. …". */
  copiedLineShown: boolean;
  /** The href of the gate's "Open in m3llo" button. */
  openLink: string;
};

/**
 * A guest opens the invite in the lounge and presses "Install m3llo".
 *
 * The browser grants the clipboard, as Chrome does for a click. The installer
 * is an empty file: a journey runs the app it already has. The browser's
 * clipboard is its own, so the journey hands `copied` to the machine's
 * clipboard (`ctx.clipboard`) itself.
 */
export async function downloadFromLounge(code: string): Promise<LoungeDownload> {
  const context = await (await browser()).newContext();
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: LOUNGE });
  const page = await context.newPage();
  let installer = "";
  // Answer with an empty attachment, as the real installer is one: the
  // browser starts a download and the lounge page stays. An aborted request
  // would replace the page with an error page.
  await page.route("https://github.com/**", (route: any) => {
    installer = route.request().url();
    return route.fulfill({
      status: 200,
      headers: { "Content-Type": "application/octet-stream", "Content-Disposition": 'attachment; filename="m3llo-Setup"' },
      body: "",
    });
  });
  try {
    await page.goto(`${LOUNGE}/join/${encodeURIComponent(code)}`);
    // The join panel dims the page until the guest answers it.
    const notNow = page.getByRole("button", { name: "Not now" });
    if (await notNow.isVisible().catch(() => false)) await notNow.click();
    await page.getByRole("button", { name: "Install m3llo" }).click();

    const gate = page.locator(".gate[open]");
    const open = gate.getByRole("link", { name: "Open in m3llo" });
    await open.waitFor({ state: "visible", timeout: 10_000 });
    const copiedLine = gate.getByText("Your invite is copied. m3llo picks it up when it opens.");
    // The line shows once the clipboard write answers.
    const copiedLineShown = await copiedLine
      .waitFor({ state: "visible", timeout: 5_000 })
      .then(() => true, () => false);
    const copied = await page.evaluate(() => navigator.clipboard.readText()).catch(() => "");
    if (!installer) throw new DriverError("the lounge did not ask for an installer");
    return { installer, copied, copiedLineShown, openLink: (await open.getAttribute("href")) ?? "" };
  } finally {
    await context.close();
  }
}
