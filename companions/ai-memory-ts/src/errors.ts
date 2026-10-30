/**
 * The error taxonomy. Every error the SDK raises extends {@link AiMemoryError}
 * and carries a stable {@link AiMemoryError.code}, so callers can branch with
 * `instanceof` or with a `switch` on the code.
 *
 * Server-supplied text in an error goes through {@link preview}: at most 300
 * characters, control characters replaced. An error never contains the bearer
 * token, a request header or a request body.
 *
 * @module
 */

/** Longest server-supplied string an error carries. */
export const PREVIEW_LIMIT = 300;

/**
 * Bound untrusted server text and neutralise control characters so it cannot
 * drive a terminal or flood a log. Same bounds as the wikisync companion.
 */
export function preview(text: string): string {
  let out = "";
  let count = 0;
  for (const ch of text) {
    if (count >= PREVIEW_LIMIT) break;
    const code = ch.codePointAt(0) ?? 0;
    out += code < 0x20 || (code >= 0x7f && code <= 0x9f) ? "?" : ch;
    count += 1;
  }
  return out;
}

/** Stable machine-readable error codes, one per error class. */
export type ErrorCode =
  | "config"
  | "argument"
  | "scope"
  | "transport"
  | "timeout"
  | "aborted"
  | "http"
  | "unauthorized"
  | "forbidden"
  | "not_found"
  | "rate_limited"
  | "rpc"
  | "invalid_params"
  | "precondition_failed"
  | "tool"
  | "protocol"
  | "unsupported_server";

/** Base class of every error the SDK raises. */
export class AiMemoryError extends Error {
  /** Stable code for callers who prefer a `switch` over `instanceof`. */
  readonly code: ErrorCode;

  /** @internal */
  constructor(code: ErrorCode, message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.code = code;
    this.name = new.target.name;
  }
}

/**
 * The client configuration is unusable: an invalid server URL, a URL that
 * embeds credentials, a query or a fragment, a bad timeout, or a token sent
 * over plain HTTP to a non-loopback host without `allowInsecureHttp`.
 */
export class ConfigError extends AiMemoryError {
  /** @internal */
  constructor(message: string) {
    super("config", message);
  }
}

/**
 * An argument failed validation before any request was sent: for example
 * `pages.read` without exactly one of `path` and `query`, or an admin purge
 * that sets `dryRun: false` without `confirm: true`.
 */
export class ArgumentError extends AiMemoryError {
  /** @internal */
  constructor(message: string) {
    super("argument", message);
  }
}

/**
 * The call had no scope, half of one, an empty name, or combined a scope with
 * `global: true`. Raised before any request is sent.
 */
export class ScopeError extends AiMemoryError {
  /** @internal */
  constructor(message: string) {
    super("scope", message);
  }
}

/**
 * The request did not complete at the network level: DNS failure, refused
 * connection, TLS error, or a redirect (the SDK never follows one, so a bearer
 * token cannot reach another origin). `cause` is a sanitized summary.
 */
export class TransportError extends AiMemoryError {
  /** @internal */
  constructor(message: string, cause?: Error) {
    super("transport", message, cause === undefined ? undefined : { cause });
  }
}

/**
 * The client's own deadline expired. When `uncertain` is true the call could
 * change state and the request had been sent: the server may or may not have
 * applied it. Reconcile before retrying (see the troubleshooting guide).
 */
export class TimeoutError extends AiMemoryError {
  /** True for a mutating call whose request was already sent. */
  readonly uncertain: boolean;

  /** @internal */
  constructor(message: string, uncertain: boolean) {
    super("timeout", message);
    this.uncertain = uncertain;
  }
}

/**
 * The caller's `AbortSignal` fired. `uncertain` has the same meaning as on
 * {@link TimeoutError}.
 */
export class AbortError extends AiMemoryError {
  /** True for a mutating call whose request was already sent. */
  readonly uncertain: boolean;

  /** @internal */
  constructor(message: string, uncertain: boolean) {
    super("aborted", message);
    this.uncertain = uncertain;
  }
}

/** The server answered with a non-2xx HTTP status. */
export class HttpError extends AiMemoryError {
  /** HTTP status code. */
  readonly status: number;
  /** Bounded, control-character-free preview of the response body. */
  readonly preview: string;
  /** A short pointer to the usual cause, when there is one. */
  readonly hint: string | undefined;

  /** @internal */
  constructor(
    status: number,
    message: string,
    bodyPreview: string,
    hint?: string,
    code: ErrorCode = "http",
  ) {
    super(code, message);
    this.status = status;
    this.preview = bodyPreview;
    this.hint = hint;
  }
}

/** HTTP 401: the bearer token is missing, wrong, expired or revoked. */
export class AuthError extends HttpError {
  /** @internal */
  constructor(message: string, bodyPreview: string, hint?: string) {
    super(401, message, bodyPreview, hint, "unauthorized");
  }
}

/**
 * HTTP 403: the caller is authenticated but not allowed, for example a user
 * key on a root-only admin route, a restricted project without a grant, or a
 * `Host` header outside `AI_MEMORY_ALLOWED_HOSTS`.
 */
export class ForbiddenError extends HttpError {
  /** @internal */
  constructor(message: string, bodyPreview: string, hint?: string) {
    super(403, message, bodyPreview, hint, "forbidden");
  }
}

/**
 * HTTP 404: an unknown workspace, project, page or session, or a route the
 * server does not mount (every `/api/v1` route is a 404 on a server started
 * without `--enable-api`).
 */
export class NotFoundError extends HttpError {
  /** @internal */
  constructor(message: string, bodyPreview: string, hint?: string) {
    super(404, message, bodyPreview, hint, "not_found");
  }
}

/** HTTP 429: the server is saturated or rate limiting the caller. */
export class RateLimitedError extends HttpError {
  /** @internal */
  constructor(message: string, bodyPreview: string, hint?: string) {
    super(429, message, bodyPreview, hint, "rate_limited");
  }
}

/**
 * The server answered with a JSON-RPC `error` member. ai-memory reports scope
 * and access refusals this way (`-32602` for invalid parameters, `-32603` for
 * internal errors, which include access refusals and admission rejections).
 */
export class RpcError extends AiMemoryError {
  /** JSON-RPC error code. */
  readonly rpcCode: number;
  /** Bounded copy of the server's `error.data`, if any. */
  readonly data: unknown;

  /** @internal */
  constructor(
    rpcCode: number,
    message: string,
    data: unknown,
    code: ErrorCode = "rpc",
  ) {
    super(code, message);
    this.rpcCode = rpcCode;
    this.data = data;
  }
}

/** A JSON-RPC `-32602` (invalid params) error. */
export class InvalidParamsError extends RpcError {
  /** @internal */
  constructor(message: string, data: unknown) {
    super(-32602, message, data, "invalid_params");
  }
}

/**
 * A conditional write or delete was refused because the page is no longer the
 * expected version (or, for `createOnly`, already exists). Nothing changed.
 * Re-read the page and decide.
 */
export class PreconditionFailedError extends RpcError {
  /** The page path the precondition was checked against. */
  readonly path: string;
  /** The version the caller expected; `null` for a `createOnly` write. */
  readonly expectedPageId: string | null;
  /** The page's current latest version; `null` when no page exists. */
  readonly currentPageId: string | null;

  /** @internal */
  constructor(
    message: string,
    data: unknown,
    path: string,
    expectedPageId: string | null,
    currentPageId: string | null,
  ) {
    super(-32600, message, data, "precondition_failed");
    this.path = path;
    this.expectedPageId = expectedPageId;
    this.currentPageId = currentPageId;
  }
}

/** The tool ran and reported its own failure (`result.isError == true`). */
export class ToolError extends AiMemoryError {
  /** The tool that failed. */
  readonly tool: string;
  /** Bounded preview of the tool's error text. */
  readonly detail: string;

  /** @internal */
  constructor(tool: string, detail: string) {
    super("tool", `${tool} reported an error: ${detail}`);
    this.tool = tool;
    this.detail = detail;
  }
}

/**
 * The response did not have the shape the SDK expects: no JSON-RPC message in
 * the body, a missing `result`, tool JSON that does not decode, or a result
 * missing a promised field.
 */
export class ProtocolError extends AiMemoryError {
  /** Bounded preview of the offending text, when there is one. */
  readonly preview: string;

  /** @internal */
  constructor(message: string, bodyPreview = "") {
    super("protocol", message);
    this.preview = bodyPreview;
  }
}

/**
 * The server lacks a tool, an argument or an endpoint the call needs. Raised
 * before anything is written. Upgrade the server to at least
 * `minimumVersion`.
 */
export class UnsupportedServerError extends AiMemoryError {
  /** The missing feature, for example `memory_write_page.expected_page_id`. */
  readonly feature: string;
  /** The version the server reported, when known. */
  readonly serverVersion: string | undefined;
  /** The oldest server release that has the feature. */
  readonly minimumVersion: string;

  /** @internal */
  constructor(feature: string, minimumVersion: string, serverVersion?: string, note?: string) {
    super(
      "unsupported_server",
      `the server does not support ${feature}; ` +
        (note ?? `ai-memory ${minimumVersion} or newer is required`) +
        (serverVersion === undefined ? "" : ` (server reports ${preview(serverVersion)})`),
    );
    this.feature = feature;
    this.minimumVersion = minimumVersion;
    this.serverVersion = serverVersion;
  }
}
