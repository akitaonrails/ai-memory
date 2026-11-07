/**
 * HTTP and JSON-RPC transport: one `fetch` per call, a deadline per request,
 * plain-JSON and SSE response bodies, no redirects and no retries.
 *
 * @module
 */

import {
  AbortError,
  AiMemoryError,
  AuthError,
  ConfigError,
  ForbiddenError,
  HttpError,
  InvalidParamsError,
  NotFoundError,
  PreconditionFailedError,
  ProtocolError,
  RateLimitedError,
  RpcError,
  TimeoutError,
  TransportError,
  preview,
} from "./errors.js";

/** The subset of the WHATWG `fetch` signature the SDK calls. */
export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

/** Options shared by {@link AiMemoryClient} and `AdminClient`. */
export interface ConnectionOptions {
  /**
   * Server URL: `scheme://host[:port]`, optionally followed by the base path
   * the server runs under (`--base-path`), for example
   * `https://memory.example.com/wiki`. No credentials, query or fragment.
   */
  serverUrl: string;
  /**
   * Bearer token: the server's root token or a user `aim_` key. Optional for
   * an unauthenticated loopback server. The SDK reads no environment
   * variables; pass the token explicitly.
   */
  token?: string | undefined;
  /** Per-request deadline in milliseconds. Default 30000. */
  timeoutMs?: number;
  /**
   * Allow a token over plain `http://` to a host that is not loopback. Off by
   * default: put TLS in front of a remote server instead.
   */
  allowInsecureHttp?: boolean;
  /** Replacement `fetch`, for runtimes without a global one or for tests. */
  fetch?: FetchLike;
  /** Largest response body accepted, in bytes. Default 32 MiB. */
  maxResponseBytes?: number;
}

/** Per-call options accepted by every method. */
export interface CallOptions {
  /** Cancels the call. Combined with the deadline; the error says which fired. */
  signal?: AbortSignal;
  /** Overrides the client's deadline for this call, in milliseconds. */
  timeoutMs?: number;
}

/** The default per-request deadline, the same as the wikisync companion. */
export const DEFAULT_TIMEOUT_MS = 30_000;
const DEFAULT_MAX_RESPONSE_BYTES = 32 * 1024 * 1024;
/** `AbortSignal.timeout` accepts at most a signed 32-bit millisecond count. */
const MAX_TIMEOUT_MS = 2_147_483_647;
const JSON_RPC_INVALID_REQUEST = -32600;
const JSON_RPC_INVALID_PARAMS = -32602;
const SESSION_HEADER = "mcp-session-id";

/** Which surface a request went to; selects the hint on an HTTP error. */
type Route = "mcp" | "api" | "admin" | "identity";

/** A response with its body already read (bounded). */
interface RawResponse {
  status: number;
  headers: Headers;
  text: string;
}

interface RequestInitLite {
  query?: Record<string, string | undefined>;
  json?: unknown;
  headers?: Record<string, string>;
  route: Route;
  mutating: boolean;
}

function isAborted(signal: AbortSignal | undefined): boolean {
  return signal?.aborted === true;
}

function isLoopbackHost(hostname: string): boolean {
  return (
    hostname === "localhost" ||
    hostname === "[::1]" ||
    /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(hostname)
  );
}

function validTimeout(value: unknown, field: string): number {
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 1 ||
    value > MAX_TIMEOUT_MS
  ) {
    throw new ConfigError(`${field} must be an integer between 1 and ${MAX_TIMEOUT_MS}`);
  }
  return value;
}

/** Parse a server URL into the base every route is appended to. */
function parseServerUrl(raw: unknown): URL {
  if (typeof raw !== "string" || raw.trim() === "") {
    throw new ConfigError("serverUrl must be a non-empty string");
  }
  // Checked on the raw text: `new URL` drops an empty `?` or `#`.
  if (raw.includes("?") || raw.includes("#")) {
    throw new ConfigError("serverUrl must not carry a query or fragment");
  }
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    // The raw value is not echoed: it may hold credentials.
    throw new ConfigError("serverUrl is not a valid absolute URL");
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new ConfigError("serverUrl must use http or https");
  }
  if (url.username !== "" || url.password !== "") {
    throw new ConfigError("serverUrl must not embed credentials; pass the token option instead");
  }
  return url;
}

/**
 * The JSON-RPC message in a response body: the body itself when it is JSON,
 * otherwise the last SSE event whose `data:` lines parse as JSON.
 * @internal
 */
export function parseJsonRpcMessage(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    // Not plain JSON; read it as an SSE stream below.
  }
  let last: unknown;
  let data: string[] = [];
  const flush = (): void => {
    if (data.length > 0) {
      try {
        last = JSON.parse(data.join("\n"));
      } catch {
        // A non-JSON frame (a keep-alive or an empty priming event).
      }
      data = [];
    }
  };
  for (const line of text.split(/\r\n|\r|\n/)) {
    if (line === "") {
      flush();
    } else if (line.startsWith("data:")) {
      data.push(line.slice(5).replace(/^ /, ""));
    }
  }
  flush();
  return last;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * The HTTP and JSON-RPC client every namespace shares. Holds the bearer token
 * in a private field: it is sent only in the `Authorization` header and is
 * scrubbed from any server text that ends up in an error.
 * @internal
 */
export class Transport {
  readonly #token: string | undefined;
  readonly #fetch: FetchLike;
  readonly #base: string;
  readonly #timeoutMs: number;
  readonly #maxResponseBytes: number;
  #nextId = 0;
  #sessionId: string | undefined;
  // Bumped by every `resetSession`, so a handshake that started earlier can
  // tell it has been superseded by a newer one (e.g. a concurrent refresh)
  // and must not adopt its now-stale session id (#2047).
  #sessionEpoch = 0;

  /** The validated server URL, without a trailing slash. Never holds a secret. */
  readonly serverUrl: string;

  constructor(options: ConnectionOptions) {
    if (options === null || typeof options !== "object") {
      throw new ConfigError("options must be an object");
    }
    const url = parseServerUrl(options.serverUrl);
    const token = options.token;
    if (token !== undefined) {
      if (typeof token !== "string" || !/^[\x21-\x7e]+$/.test(token)) {
        throw new ConfigError(
          "token must be a non-empty string of printable ASCII without spaces",
        );
      }
      if (
        url.protocol === "http:" &&
        !isLoopbackHost(url.hostname) &&
        options.allowInsecureHttp !== true
      ) {
        throw new ConfigError(
          `refusing to send a token over plain http to ${url.hostname}; use https ` +
            "(TLS through a reverse proxy) or set allowInsecureHttp: true",
        );
      }
    }
    if (options.fetch !== undefined && typeof options.fetch !== "function") {
      throw new ConfigError("fetch must be a function");
    }
    const fetchImpl = options.fetch ?? (globalThis.fetch as FetchLike | undefined);
    if (fetchImpl === undefined) {
      throw new ConfigError("no global fetch in this runtime; pass the fetch option");
    }
    this.#fetch = fetchImpl;
    this.#token = token;
    this.#timeoutMs =
      options.timeoutMs === undefined
        ? DEFAULT_TIMEOUT_MS
        : validTimeout(options.timeoutMs, "timeoutMs");
    this.#maxResponseBytes =
      options.maxResponseBytes === undefined
        ? DEFAULT_MAX_RESPONSE_BYTES
        : validTimeout(options.maxResponseBytes, "maxResponseBytes");
    this.#base = url.origin + url.pathname.replace(/\/+$/, "");
    this.serverUrl = this.#base;
  }

  /** True when a bearer token is configured. */
  get hasToken(): boolean {
    return this.#token !== undefined;
  }

  /** Replace every occurrence of the token in server-supplied text. */
  #redact(text: string): string {
    const token = this.#token;
    if (token === undefined) return text;
    let out = text.split(token).join("[redacted]");
    const encoded = encodeURIComponent(token);
    if (encoded !== token) out = out.split(encoded).join("[redacted]");
    return out;
  }

  /** Bound and scrub server text for an error. */
  safeText(text: string): string {
    return preview(this.#redact(text));
  }

  /** A bounded, scrubbed deep copy of server JSON for an error's `data`. */
  safeData(value: unknown, depth = 0): unknown {
    if (typeof value === "string") return this.safeText(value);
    if (value === null || typeof value !== "object") return value;
    if (depth >= 4) return "[truncated]";
    if (Array.isArray(value)) {
      return value.slice(0, 32).map((item) => this.safeData(item, depth + 1));
    }
    const out: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value).slice(0, 32)) {
      out[this.safeText(key)] = this.safeData(item, depth + 1);
    }
    return out;
  }

  /** Send one HTTP request and read its body, bounded by size and deadline. */
  async request(
    method: "GET" | "POST",
    path: string,
    init: RequestInitLite,
    options: CallOptions = {},
  ): Promise<RawResponse> {
    const callerSignal = options.signal;
    if (callerSignal?.aborted === true) {
      // Nothing was sent, so the outcome is certain.
      throw new AbortError("aborted by the caller before the request was sent", false);
    }
    const timeoutMs =
      options.timeoutMs === undefined
        ? this.#timeoutMs
        : validTimeout(options.timeoutMs, "timeoutMs");
    const deadline = AbortSignal.timeout(timeoutMs);
    const signal =
      callerSignal === undefined ? deadline : AbortSignal.any([callerSignal, deadline]);

    let url = this.#base + path;
    if (init.query !== undefined) {
      const params = new URLSearchParams();
      for (const [key, value] of Object.entries(init.query)) {
        if (value !== undefined) params.append(key, value);
      }
      const query = params.toString();
      if (query !== "") url += `?${query}`;
    }
    const headers: Record<string, string> = {
      Accept: init.route === "mcp" ? "application/json, text/event-stream" : "application/json",
      ...init.headers,
    };
    if (init.json !== undefined) headers["Content-Type"] = "application/json";
    if (this.#token !== undefined) headers["Authorization"] = `Bearer ${this.#token}`;
    if (init.route === "mcp" && this.#sessionId !== undefined) {
      headers["Mcp-Session-Id"] = this.#sessionId;
    }
    const requestInit: RequestInit = {
      method,
      headers,
      // A redirect could carry the bearer token to another origin.
      redirect: "error",
      signal,
    };
    if (init.json !== undefined) requestInit.body = JSON.stringify(init.json);

    try {
      const response = await this.#fetch(url, requestInit);
      if (
        response.type === "opaqueredirect" ||
        (response.status >= 300 && response.status < 400 && response.status !== 304)
      ) {
        await response.body?.cancel().catch(() => undefined);
        throw new TransportError(`redirect refused for ${path} (HTTP ${response.status})`);
      }
      const text = await this.#readBody(response);
      return { status: response.status, headers: response.headers, text };
    } catch (error) {
      if (error instanceof AiMemoryError) throw error;
      // The request had been handed to fetch, so a mutation may have landed.
      // (Re-read through a function: the early return narrowed `aborted`.)
      if (isAborted(callerSignal)) {
        throw new AbortError(`${path} aborted by the caller`, init.mutating);
      }
      if (deadline.aborted) {
        throw new TimeoutError(`${path} got no response within ${timeoutMs} ms`, init.mutating);
      }
      throw this.#transportError(path, error);
    }
  }

  async #readBody(response: Response): Promise<string> {
    const body = response.body;
    if (body === null) return "";
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let total = 0;
    let text = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > this.#maxResponseBytes) {
        await reader.cancel().catch(() => undefined);
        throw new ProtocolError(`response body exceeds ${this.#maxResponseBytes} bytes`);
      }
      text += decoder.decode(value, { stream: true });
    }
    return text + decoder.decode();
  }

  #transportError(path: string, error: unknown): TransportError {
    const outer = error instanceof Error ? error : undefined;
    const inner = outer?.cause instanceof Error ? outer.cause : outer;
    const reason = this.safeText(inner?.message ?? String(error));
    const redirect = /redirect/i.test(reason) || /redirect/i.test(outer?.message ?? "");
    const cause = new Error(reason);
    const code = (inner as { code?: unknown } | undefined)?.code;
    if (typeof code === "string") {
      Object.defineProperty(cause, "code", { value: this.safeText(code), enumerable: true });
    }
    return new TransportError(
      redirect ? `redirect refused for ${path}` : `request to ${path} failed: ${reason}`,
      cause,
    );
  }

  /** Map a non-2xx response to the matching {@link HttpError} subclass. */
  httpError(path: string, response: RawResponse, route: Route): HttpError {
    let detail = response.text;
    try {
      const parsed: unknown = JSON.parse(response.text);
      if (isRecord(parsed) && typeof parsed["error"] === "string") detail = parsed["error"];
    } catch {
      // Plain-text body; keep it as is.
    }
    const shown = this.safeText(detail);
    const message = `HTTP ${response.status} from ${path}${shown === "" ? "" : `: ${shown}`}`;
    switch (response.status) {
      case 401:
        return new AuthError(
          message,
          shown,
          "the server did not accept the bearer token; pass the root token or a live aim_ key " +
            "(a bearer never authenticates /auth/*)",
        );
      case 403:
        return new ForbiddenError(
          message,
          shown,
          route === "admin"
            ? "/admin/* is root-only once the server has DB users, and an aim_ key is always user-level"
            : "the caller lacks access: a restricted project without a grant, or a Host header " +
                "outside AI_MEMORY_ALLOWED_HOSTS",
        );
      case 404:
        return new NotFoundError(
          message,
          shown,
          route === "api"
            ? "unknown workspace, project, page or session, or the server runs without " +
                "--enable-api (capabilities() reports apiMounted)"
            : undefined,
        );
      case 429:
        return new RateLimitedError(message, shown, "the server is saturated; retry after a pause");
      default:
        return new HttpError(response.status, message, shown);
    }
  }

  /** GET or POST a JSON route and return its decoded body. */
  async json(
    method: "GET" | "POST",
    path: string,
    init: RequestInitLite,
    options?: CallOptions,
  ): Promise<unknown> {
    const response = await this.request(method, path, init, options);
    if (response.status < 200 || response.status >= 300) {
      throw this.httpError(path, response, init.route);
    }
    try {
      return JSON.parse(response.text);
    } catch {
      throw new ProtocolError(`${path} answered with invalid JSON`, this.safeText(response.text));
    }
  }

  /**
   * One JSON-RPC request to `/mcp`; its `result` and the response headers.
   * Throws the typed error for an HTTP failure or a JSON-RPC `error`.
   */
  async rpcWithHeaders(
    method: string,
    params: unknown,
    mutating: boolean,
    options?: CallOptions,
  ): Promise<{ result: Record<string, unknown>; headers: Headers }> {
    this.#nextId += 1;
    const id = this.#nextId;
    const path = "/mcp";
    const response = await this.request(
      "POST",
      path,
      { json: { jsonrpc: "2.0", id, method, params }, route: "mcp", mutating },
      options,
    );
    if (response.status < 200 || response.status >= 300) {
      throw this.httpError(path, response, "mcp");
    }
    const message = parseJsonRpcMessage(response.text);
    if (!isRecord(message)) {
      throw new ProtocolError(
        `${method} answered with no JSON-RPC message`,
        this.safeText(response.text),
      );
    }
    if (message["id"] !== undefined && message["id"] !== null && message["id"] !== id) {
      throw new ProtocolError(`${method} answered a different request id`);
    }
    const error = message["error"];
    if (error !== undefined) throw this.rpcError(method, error);
    const result = message["result"];
    if (!isRecord(result)) {
      throw new ProtocolError(`${method} answered with no result`, this.safeText(response.text));
    }
    return { result, headers: response.headers };
  }

  /** {@link rpcWithHeaders} without the headers. */
  async rpc(
    method: string,
    params: unknown,
    mutating: boolean,
    options?: CallOptions,
  ): Promise<Record<string, unknown>> {
    return (await this.rpcWithHeaders(method, params, mutating, options)).result;
  }

  /** Send a JSON-RPC notification (no id, no result expected). */
  async notify(method: string, options?: CallOptions): Promise<void> {
    const path = "/mcp";
    const response = await this.request(
      "POST",
      path,
      { json: { jsonrpc: "2.0", method }, route: "mcp", mutating: false },
      options,
    );
    if (response.status < 200 || response.status >= 300) {
      throw this.httpError(path, response, "mcp");
    }
  }

  /**
   * Keep the `Mcp-Session-Id` a stateful server issued on `initialize`, unless
   * a newer handshake has superseded this one. `epoch` is the value
   * `resetSession` returned when this handshake began; if it is no longer the
   * current epoch a later `resetSession` (a concurrent refresh) owns the
   * session, so the stale id is dropped rather than overwriting the live one
   * (#2047). Returns how the handshake resolved: it opened a session
   * (`adopted`), the server is stateless (`stateless`), or it was superseded.
   */
  adoptSession(headers: Headers, epoch: number): "adopted" | "stateless" | "superseded" {
    if (epoch !== this.#sessionEpoch) return "superseded";
    const value = headers.get(SESSION_HEADER);
    if (value === null) return "stateless";
    if (!/^[\x21-\x7e]{1,256}$/.test(value)) {
      throw new ProtocolError("initialize answered with an invalid Mcp-Session-Id");
    }
    this.#sessionId = value;
    return "adopted";
  }

  /**
   * Forget a stateful session, so the next probe starts a new one, and return
   * the new epoch the caller passes back to `adoptSession`.
   */
  resetSession(): number {
    this.#sessionId = undefined;
    return ++this.#sessionEpoch;
  }

  /** Decode a JSON-RPC `error` member into the matching error class. */
  rpcError(method: string, error: unknown): RpcError {
    if (!isRecord(error)) {
      return new RpcError(0, `${method} failed with a malformed JSON-RPC error`, undefined);
    }
    const code = typeof error["code"] === "number" ? error["code"] : 0;
    const text = typeof error["message"] === "string" ? this.safeText(error["message"]) : "";
    const data = this.safeData(error["data"]);
    const message = `${method} failed (JSON-RPC ${code})${text === "" ? "" : `: ${text}`}`;
    if (
      code === JSON_RPC_INVALID_REQUEST &&
      isRecord(data) &&
      data["reason"] === "precondition_failed"
    ) {
      const field = (key: string): string | null =>
        typeof data[key] === "string" ? data[key] : null;
      return new PreconditionFailedError(
        message,
        data,
        field("path") ?? "",
        field("expected_page_id"),
        field("current_page_id"),
      );
    }
    if (code === JSON_RPC_INVALID_PARAMS) return new InvalidParamsError(message, data);
    return new RpcError(code, message, data);
  }

  /** Serialized form: never includes the token. */
  toJSON(): Record<string, unknown> {
    return { serverUrl: this.serverUrl, token: this.#token === undefined ? undefined : "[redacted]" };
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `Transport ${JSON.stringify(this.toJSON())}`;
  }
}
