// A client for the server's own protocol, spoken the way the UI speaks it (rust/crates/ui/src/runtime.rs):
// a bootstrap credential is exchanged once at /oauth/token for a bearer token, and the WebSocket at
// /ws?orchestrationProtocol=2 carries `{_tag:"Request", id, tag, payload, headers:[]}` frames answered by
// `Exit` frames (and `Chunk` frames for streams, which must be acknowledged).

export async function exchangeCredential(origin, credential) {
  const response = await fetch(`${origin}/oauth/token`, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams([
      ["grant_type", "urn:ietf:params:oauth:grant-type:token-exchange"],
      ["subject_token", credential.trim()],
      ["subject_token_type", "urn:t3:params:oauth:token-type:environment-bootstrap"],
      ["requested_token_type", "urn:ietf:params:oauth:token-type:access_token"],
      ["client_device_type", "desktop"],
      ["client_label", "Browser fixture"],
    ]),
  });
  const text = await response.text();
  if (!response.ok)
    throw new Error(`pairing failed (HTTP ${response.status}): ${text.slice(0, 300)}`);
  const body = JSON.parse(text);
  if (body.token_type !== "Bearer") throw new Error(`unexpected token type ${body.token_type}`);
  return body.access_token;
}

export class Rpc {
  constructor(origin, token) {
    this.origin = origin;
    this.token = token;
    this.next = 1;
    this.pending = new Map();
    this.streams = new Map();
  }

  static async connect(origin, token) {
    const rpc = new Rpc(origin, token);
    const url = origin.replace(/^http/, "ws") + "/ws?orchestrationProtocol=2";
    // Node's WebSocket (undici) takes request headers as an option, which a browser's does not.
    const socket = new WebSocket(url, { headers: { authorization: `Bearer ${token}` } });
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve, { once: true });
      socket.addEventListener(
        "error",
        (event) => reject(new Error(`WebSocket failed: ${event.message ?? "error"}`)),
        { once: true },
      );
    });
    rpc.socket = socket;
    socket.addEventListener("message", (event) => rpc.onMessage(String(event.data)));
    socket.addEventListener("close", () => {
      for (const { reject } of rpc.pending.values()) reject(new Error("connection closed"));
      rpc.pending.clear();
    });
    return rpc;
  }

  onMessage(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch {
      return;
    }
    if (frame._tag === "Exit") {
      const waiting = this.pending.get(frame.requestId);
      if (waiting) {
        this.pending.delete(frame.requestId);
        waiting.resolve(frame.exit);
      }
    } else if (frame._tag === "Chunk") {
      this.socket.send(JSON.stringify({ _tag: "Ack", requestId: frame.requestId }));
      this.streams.get(frame.requestId)?.(frame.values ?? []);
    }
  }

  /** The exit of a unary request: `{ok, value}` or `{ok:false, error}` with the server's error text. */
  async exit(tag, payload = {}) {
    const id = this.next++;
    const exit = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.socket.send(JSON.stringify({ _tag: "Request", id, tag, payload, headers: [] }));
    const result = await exit;
    if (result._tag === "Success") return { ok: true, value: result.value };
    return { ok: false, error: describeFailure(result), raw: result };
  }

  /** The value of a request that must succeed. */
  async call(tag, payload = {}) {
    const result = await this.exit(tag, payload);
    if (!result.ok) throw new Error(`${tag}: ${result.error}`);
    return result.value;
  }

  /** Opens a stream; `onValues` sees every chunk. Resolves to `{close}` once the request is sent. */
  subscribe(tag, payload, onValues) {
    const id = this.next++;
    this.streams.set(id, onValues);
    this.socket.send(JSON.stringify({ _tag: "Request", id, tag, payload, headers: [] }));
    return { close: () => this.streams.delete(id) };
  }

  /** A request that streams: resolves to `{events, exit}` once the stream's own exit arrives. */
  async stream(tag, payload = {}) {
    const id = this.next++;
    const events = [];
    this.streams.set(id, (values) => events.push(...values));
    const exit = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.socket.send(JSON.stringify({ _tag: "Request", id, tag, payload, headers: [] }));
    const result = await exit;
    this.streams.delete(id);
    return result._tag === "Success"
      ? { ok: true, events, value: result.value }
      : { ok: false, events, error: describeFailure(result), raw: result };
  }

  close() {
    try {
      this.socket.close();
    } catch {}
  }
}

export function describeFailure(exit) {
  const first = exit?.cause?.[0];
  const error = first?.error ?? first ?? exit;
  const parts = [
    error?._tag,
    error?.message,
    error?.detail,
    error?.reason,
    error?.cause?.message,
  ].filter((part) => typeof part === "string" && part);
  return parts.length ? [...new Set(parts)].join(": ") : JSON.stringify(exit).slice(0, 400);
}
