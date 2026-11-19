/**
 * Shared plumbing behind every namespace: the transport, the default scope,
 * the cached tool index and tool-call decoding.
 *
 * @internal
 * @module
 */

import {
  initialize,
  probeTools,
  requireFeature,
  type SessionInfo,
  type ToolIndex,
} from "./capabilities.js";
import { AbortError, ArgumentError, ConfigError, ProtocolError, TimeoutError, ToolError } from "./errors.js";
import { resolveScope, type Scope } from "./scope.js";
import type { CallOptions, Transport } from "./transport.js";

/** Drop `undefined` entries so they never reach the wire. */
export function wire(entries: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(entries)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}

/** A required non-empty string argument. */
export function requireText(value: unknown, name: string, where: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new ArgumentError(`${where}: ${name} must be a non-empty string`);
  }
  return value;
}

/** The argument object itself, or an empty one when omitted. */
export function argsObject<T extends object>(value: T | undefined, where: string): T {
  if (value === undefined) return {} as T;
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ArgumentError(`${where}: arguments must be an object`);
  }
  return value;
}

const MAX_TIMEOUT_MS = 2_147_483_647;

/** Wait for client-owned initialization without lending it a caller's cancellation. */
function waitForShared<T>(shared: Promise<T>, options?: CallOptions): Promise<T> {
  const signal = options?.signal;
  if (signal?.aborted === true) {
    return Promise.reject(new AbortError("aborted by the caller while waiting for initialization", false));
  }
  const timeoutMs = options?.timeoutMs;
  if (
    timeoutMs !== undefined &&
    (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > MAX_TIMEOUT_MS)
  ) {
    return Promise.reject(
      new ConfigError(`timeoutMs must be an integer between 1 and ${MAX_TIMEOUT_MS}`),
    );
  }
  if (signal === undefined && timeoutMs === undefined) return shared;

  return new Promise<T>((resolve, reject) => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const cleanup = (): void => {
      if (timer !== undefined) clearTimeout(timer);
      signal?.removeEventListener("abort", aborted);
    };
    const aborted = (): void => {
      cleanup();
      reject(new AbortError("aborted by the caller while waiting for initialization", false));
    };
    signal?.addEventListener("abort", aborted, { once: true });
    if (timeoutMs !== undefined) {
      timer = setTimeout(() => {
        cleanup();
        reject(new TimeoutError(`initialization got no response within ${timeoutMs} ms`, false));
      }, timeoutMs);
    }
    shared.then(
      (value) => {
        cleanup();
        resolve(value);
      },
      (error: unknown) => {
        cleanup();
        reject(error);
      },
    );
  });
}

/** The first `{ type: "text", text }` item of a tool result's content. */
function firstText(content: unknown): string | undefined {
  if (!Array.isArray(content)) return undefined;
  for (const item of content as unknown[]) {
    if (item === null || typeof item !== "object") continue;
    const { type, text } = item as { type?: unknown; text?: unknown };
    if (type === "text" && typeof text === "string") return text;
  }
  return undefined;
}

/** The state one client shares with its namespaces. */
export class Core {
  readonly transport: Transport;
  readonly defaultScope: Scope | undefined;
  #session: Promise<SessionInfo> | undefined;
  #index: Promise<ToolIndex> | undefined;

  constructor(transport: Transport, defaultScope: Scope | undefined) {
    this.transport = transport;
    this.defaultScope = defaultScope;
  }

  /** The call's scope, else the default, else a ScopeError. */
  scope(callScope: Scope | undefined, where: string): Scope {
    return resolveScope(callScope, this.defaultScope, where);
  }

  /** The handshake is needed even by calls that skip schema checks. */
  #initialize(): Promise<SessionInfo> {
    if (this.#session === undefined) {
      const session = initialize(this.transport);
      this.#session = session;
      session.catch(() => {
        if (this.#session === session) this.#session = undefined;
      });
    }
    return this.#session;
  }

  /** A refresh can replace the handshake while a request is waiting for it. */
  async #withSession<T>(send: (info: SessionInfo) => Promise<T>, options?: CallOptions): Promise<T> {
    if (options?.signal?.aborted === true) {
      throw new AbortError("aborted by the caller before initialization", false);
    }
    const deadline = options?.timeoutMs === undefined ? undefined : performance.now() + options.timeoutMs;
    for (;;) {
      const session = this.#initialize();
      const info = await waitForShared(session, options);
      // Invoke send in the same turn as the identity check, before another
      // refresh can clear the transport's session id.
      if (this.#session === session) return send(info);
      if (deadline !== undefined) {
        const remaining = Math.ceil(deadline - performance.now());
        if (remaining <= 0) throw new TimeoutError("timed out waiting for initialization", false);
        options = { ...options, timeoutMs: remaining };
      }
    }
  }

  /** `initialize` + `tools/list`, probed once per client and cached. */
  toolIndex(options?: CallOptions): Promise<ToolIndex> {
    if (options?.signal?.aborted === true) {
      return Promise.reject(new AbortError("aborted by the caller before initialization", false));
    }
    if (this.#index === undefined) {
      const probe = this.#withSession((info) => probeTools(this.transport, info));
      const session = this.#session;
      this.#index = probe;
      // A failed probe is not cached: the next call probes again.
      probe.catch(() => {
        if (this.#index === probe) {
          this.#index = undefined;
          // A raw caller may already have started a new handshake after the
          // old one failed; this probe must not discard that pending retry.
          if (this.#session === session) this.#session = undefined;
        }
      });
    }
    return waitForShared(this.#index, options);
  }

  /** Drop the handshake and tool index so refresh opens a new session. */
  forgetToolIndex(): void {
    this.#session = undefined;
    this.#index = undefined;
  }

  /**
   * Call a wrapped tool: check the server lists the tool and every argument
   * sent, then call it and decode its JSON payload.
   */
  async invoke(
    tool: string,
    args: Record<string, unknown>,
    mutating: boolean,
    options?: CallOptions,
  ): Promise<unknown> {
    const index = await this.toolIndex(options);
    requireFeature(index, tool, Object.keys(args));
    return this.callTool(tool, args, mutating, options);
  }

  /** `tools/call` without feature checks; returns the decoded tool JSON. */
  async callTool(
    tool: string,
    args: Record<string, unknown>,
    mutating: boolean,
    options?: CallOptions,
  ): Promise<unknown> {
    const result = await this.#withSession(
      () => this.transport.rpc("tools/call", { name: tool, arguments: args }, mutating, options),
      options,
    );
    const text = firstText(result["content"]);
    if (result["isError"] === true) {
      throw new ToolError(tool, this.transport.safeText(text ?? "no detail"));
    }
    if (text === undefined) throw new ProtocolError(`${tool} returned no text content`);
    try {
      return JSON.parse(text);
    } catch {
      throw new ProtocolError(`${tool} returned text that is not JSON`, this.transport.safeText(text));
    }
  }
}
