// Client for the Slint MCP server embedded in an e2e build (`mcp` feature).
//
// Speaks JSON-RPC over HTTP to 127.0.0.1:<SLINT_MCP_PORT>/mcp. Reads run in
// parallel on one keep-alive connection pool, so listing every control on a
// screen costs one round of requests, not one process per element.

export type Handle = { index: string; generation: string };

/** The subset of Slint's ElementPropertiesResponse that the driver uses. */
export type ElementProps = {
  accessibleLabel?: string;
  accessibleValue?: string;
  accessibleRole?: string;
  accessiblePlaceholderText?: string;
  accessibleChecked?: boolean;
  accessibleEnabled?: boolean;
  size?: { width: number; height: number };
  absolutePosition?: { x: number; y: number };
};

/** A control on screen, found through its accessibility properties. */
export type Control = {
  handle: Handle;
  role: string;
  label: string;
  value: string;
  checked: boolean;
  x: number;
  y: number;
  width: number;
  height: number;
};

/** Roles that the a11y lint requires a label on (client/src/a11y_lint.rs). */
export const CONTROL_ROLES = ["Button", "Switch", "Tab", "Slider", "Combobox", "TextInput"];

export class SlintMcp {
  readonly url: string;
  private id = 0;
  /** Journey requests and actions that are running now. */
  private holds = 0;
  /** The live screenshot that is running now, if any. */
  private exclusive: Promise<unknown> | null = null;

  constructor(port: number) {
    this.url = `http://127.0.0.1:${port}/mcp`;
  }

  /**
   * Run `fn` as part of the journey. Journey work shares the connection with
   * other journey work, but never runs at the same time as an exclusive job
   * (a live screenshot): it waits for that job first. Nesting is safe, because
   * an exclusive job cannot start while a hold is open.
   */
  async hold<T>(fn: () => Promise<T>): Promise<T> {
    while (this.exclusive) await this.exclusive.catch(() => undefined);
    this.holds++;
    try {
      return await fn();
    } finally {
      this.holds--;
    }
  }

  /**
   * Run `fn` alone on this connection, or not at all: it is skipped (and this
   * returns false) when a journey request or action is in flight, or another
   * exclusive job runs. `fn` must call `rawCall`, not `call`.
   */
  async tryExclusive(fn: () => Promise<void>): Promise<boolean> {
    if (this.holds > 0 || this.exclusive) return false;
    const job = fn();
    this.exclusive = job;
    try {
      await job;
    } finally {
      this.exclusive = null;
    }
    return true;
  }

  /** Call one MCP tool. Throws with the tool name when the call fails. */
  call<T = any>(tool: string, args: Record<string, unknown> = {}): Promise<T> {
    return this.hold(() => this.rawCall<T>(tool, args));
  }

  /** `call` without the journey hold. Only an exclusive job uses it. */
  async rawCall<T = any>(tool: string, args: Record<string, unknown> = {}): Promise<T> {
    const res = await fetch(this.url, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: ++this.id,
        method: "tools/call",
        params: { name: tool, arguments: args },
      }),
    });
    let body = await res.text();
    // The server may answer as an SSE stream: keep the `data:` payloads.
    if (body.startsWith("event:") || body.startsWith("data:") || body.includes("\ndata:")) {
      body = body
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5))
        .join("");
    }
    const msg = JSON.parse(body);
    if (msg.error) throw new Error(`slint mcp ${tool}: ${JSON.stringify(msg.error)}`);
    const result = msg.result ?? {};
    if (result.isError) {
      const text = (result.content ?? []).map((c: any) => c.text ?? "").join(" ");
      throw new Error(`slint mcp ${tool}: ${text}`);
    }
    const content = result.content ?? [];
    const image = content.find((c: any) => c.type === "image");
    if (image) return { png: Buffer.from(image.data, "base64") } as T;
    const text = content.find((c: any) => c.type === "text");
    return (text ? JSON.parse(text.text) : result) as T;
  }

  /** True when the server answers. */
  async ready(): Promise<boolean> {
    try {
      await this.call("list_windows");
      return true;
    } catch {
      return false;
    }
  }

  async window(): Promise<Handle> {
    const r = await this.call<{ windowHandles: Handle[] }>("list_windows");
    const w = r.windowHandles?.[0];
    if (!w) throw new Error("slint mcp: no window");
    return w;
  }

  async root(): Promise<Handle> {
    const r = await this.call<{ rootElementHandle: Handle }>("get_window_properties", {
      windowHandle: await this.window(),
    });
    return r.rootElementHandle;
  }

  async query(root: Handle, instruction: Record<string, unknown>): Promise<Handle[]> {
    const r = await this.call<{ elementHandles?: Handle[] }>("query_element_descendants", {
      elementHandle: root,
      queryStack: [{ matchDescendants: true }, instruction],
      findAll: true,
    });
    return r.elementHandles ?? [];
  }

  /** Properties of many elements at once. A stale handle yields null. */
  async props(handles: Handle[]): Promise<(ElementProps | null)[]> {
    return Promise.all(
      handles.map((h) =>
        this.call<ElementProps>("get_element_properties", { elementHandle: h }).catch(() => null),
      ),
    );
  }

  /** Every labelled control on screen, in tree order within each role. */
  controls(): Promise<Control[]> {
    return this.hold(() => this.controlsNow());
  }

  private async controlsNow(): Promise<Control[]> {
    const root = await this.root();
    const perRole = await Promise.all(
      CONTROL_ROLES.map((role) => this.query(root, { matchElementAccessibleRole: role })),
    );
    const handles = perRole.flat();
    const roles = perRole.flatMap((hs, i) => hs.map(() => CONTROL_ROLES[i]));
    const props = await this.props(handles);
    const out: Control[] = [];
    props.forEach((p, i) => {
      if (!p) return;
      out.push({
        handle: handles[i],
        role: roles[i],
        label: p.accessibleLabel ?? "",
        value: p.accessibleValue ?? "",
        checked: p.accessibleChecked ?? false,
        x: p.absolutePosition?.x ?? 0,
        y: p.absolutePosition?.y ?? 0,
        width: p.size?.width ?? 0,
        height: p.size?.height ?? 0,
      });
    });
    return out;
  }

  /** Visible text: Text labels and text-input values. */
  texts(): Promise<string[]> {
    return this.hold(() => this.textsNow());
  }

  private async textsNow(): Promise<string[]> {
    const root = await this.root();
    const [texts, inputs] = await Promise.all([
      this.query(root, { matchElementTypeName: "Text" }),
      this.query(root, { matchElementAccessibleRole: "TextInput" }),
    ]);
    const props = await this.props([...texts, ...inputs]);
    return props
      .map((p, i) => (i < texts.length ? p?.accessibleLabel : p?.accessibleValue) ?? "")
      .filter((t) => t !== "");
  }

  async click(handle: Handle): Promise<void> {
    await this.call("click_element", { elementHandle: handle });
  }

  /** The control's default accessibility action, as a screen reader sends it. */
  async activate(handle: Handle): Promise<void> {
    await this.call("invoke_accessibility_action", { elementHandle: handle, action: "Default_" });
  }

  async setValue(handle: Handle, value: string): Promise<void> {
    await this.call("set_element_value", { elementHandle: handle, value });
  }

  async key(text: string): Promise<void> {
    await this.hold(async () => {
      await this.call("dispatch_key_event", { windowHandle: await this.window(), text });
    });
  }

  screenshot(): Promise<Buffer> {
    return this.hold(async () => {
      const r = await this.call<{ png: Buffer }>("take_screenshot", { windowHandle: await this.window() });
      return r.png;
    });
  }

  /** The same screenshot, for an exclusive job. */
  async rawScreenshot(): Promise<Buffer> {
    const w = (await this.rawCall<{ windowHandles: Handle[] }>("list_windows")).windowHandles?.[0];
    if (!w) throw new Error("slint mcp: no window");
    return (await this.rawCall<{ png: Buffer }>("take_screenshot", { windowHandle: w })).png;
  }
}
