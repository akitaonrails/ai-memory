/**
 * A scripted server behind an injected `fetch`, for the unit suite. It
 * records every request and answers `initialize`, `tools/list` (built from
 * the committed schema snapshot) and `tools/call` from per-tool handlers. No
 * network is involved.
 */

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import type { FetchLike } from "../../src/transport.js";
import type { NormalizedTools } from "./schema.js";

export interface RecordedRequest {
  readonly url: string;
  readonly path: string;
  readonly query: URLSearchParams;
  readonly method: string;
  readonly headers: Record<string, string>;
  readonly body: unknown;
  readonly signal: AbortSignal | undefined;
}

export type Handler = (request: RecordedRequest) => Response | Promise<Response>;
export type ToolHandler = (
  args: Record<string, unknown>,
  request: RecordedRequest,
) => unknown | Promise<unknown>;

const snapshotPath = fileURLToPath(new URL("../../../schema/tools.json", import.meta.url));
export const SNAPSHOT = JSON.parse(readFileSync(snapshotPath, "utf8")) as NormalizedTools;

const OPTIONAL_TEXT = { type: ["null", "string"] };

/**
 * The two tools a server lists only with `[chunking] enabled = true`, from
 * the frozen wire contract. The real snapshot is `schema/tools.chunking.json`;
 * the drift test compares it with a live server, this fixture only lets the
 * unit suite run without one.
 */
export const CHUNKING_TOOL_SCHEMAS: NormalizedTools = {
  memory_processing_job_start: {
    properties: {
      kind: { type: "string", enum: ["auto_improve", "consolidate", "consolidate_multi"] },
      project: OPTIONAL_TEXT,
      session_id: { type: "string" },
      workspace: OPTIONAL_TEXT,
    },
    required: ["kind", "session_id"],
  },
  memory_processing_job_status: {
    properties: { job_id: { type: "string" }, project: OPTIONAL_TEXT, workspace: OPTIONAL_TEXT },
    required: ["job_id"],
  },
};

/** A JSON response. */
export function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

/** A text response. */
export function text(body: string, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(body, { status, headers });
}

/** A tool result whose payload is JSON text, as ai-memory sends it. */
export function toolPayload(payload: unknown): Record<string, unknown> {
  return { content: [{ type: "text", text: JSON.stringify(payload) }], isError: false };
}

/** Marker a tool handler returns to answer with a JSON-RPC error instead. */
export class RpcFailure {
  constructor(
    readonly code: number,
    readonly message: string,
    readonly data?: unknown,
  ) {}
}

/**
 * A promise that settles only when the request's signal aborts. A real
 * pending fetch holds a socket open; this holds a timer instead, because the
 * SDK's deadline timer is unref'd and Node 22 would otherwise end the event
 * loop with the test still pending.
 */
export function hang(request: RecordedRequest): Promise<Response> {
  return new Promise((_resolve, reject) => {
    const signal = request.signal;
    if (signal === undefined) return;
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    const keepAlive = setInterval(() => undefined, 1_000);
    signal.addEventListener(
      "abort",
      () => {
        clearInterval(keepAlive);
        reject(signal.reason);
      },
      { once: true },
    );
  });
}

export class FakeServer {
  readonly requests: RecordedRequest[] = [];
  readonly tools = new Map<string, ToolHandler>();
  readonly routes = new Map<string, Handler>();
  /** Replaces the whole `/mcp` answer when set. */
  mcp: Handler | undefined;
  protocolVersion = "2025-03-26";
  serverVersion = "3.0.0";
  sessionId: string | undefined;
  /** Tool schemas served by `tools/list`; defaults to the snapshot. */
  schemas: NormalizedTools = structuredClone(SNAPSHOT);

  /** List the processing-job tools, as a server with chunking enabled does. */
  enableChunking(): void {
    Object.assign(this.schemas, structuredClone(CHUNKING_TOOL_SCHEMAS));
  }

  readonly fetch: FetchLike = async (input, init) => {
    const url = new URL(input);
    const headers: Record<string, string> = {};
    for (const [key, value] of Object.entries((init.headers ?? {}) as Record<string, string>)) {
      headers[key.toLowerCase()] = value;
    }
    const request: RecordedRequest = {
      url: input,
      path: url.pathname,
      query: url.searchParams,
      method: init.method ?? "GET",
      headers,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
      signal: init.signal ?? undefined,
    };
    this.requests.push(request);
    if (url.pathname.endsWith("/mcp")) {
      if (this.mcp !== undefined) return this.mcp(request);
      return this.#answerMcp(request);
    }
    const route = this.routes.get(`${request.method} ${url.pathname}`);
    if (route !== undefined) return route(request);
    return json({ error: "not found" }, 404);
  };

  /** Every `tools/call` request, in order. */
  toolCalls(): { name: string; args: Record<string, unknown> }[] {
    return this.requests
      .filter((r) => (r.body as { method?: string } | undefined)?.method === "tools/call")
      .map((r) => {
        const params = (r.body as { params: { name: string; arguments: Record<string, unknown> } })
          .params;
        return { name: params.name, args: params.arguments };
      });
  }

  /** The last `tools/call` arguments. */
  lastArgs(): Record<string, unknown> {
    const calls = this.toolCalls();
    const last = calls[calls.length - 1];
    if (last === undefined) throw new Error("no tools/call recorded");
    return last.args;
  }

  async #answerMcp(request: RecordedRequest): Promise<Response> {
    const body = request.body as { id?: number; method: string; params?: Record<string, unknown> };
    const reply = (result: unknown, headers: Record<string, string> = {}) =>
      json({ jsonrpc: "2.0", id: body.id, result }, 200, headers);
    switch (body.method) {
      case "initialize":
        return reply(
          {
            protocolVersion: this.protocolVersion,
            capabilities: { tools: {} },
            serverInfo: { name: "ai-memory", version: this.serverVersion },
          },
          this.sessionId === undefined ? {} : { "mcp-session-id": this.sessionId },
        );
      case "notifications/initialized":
        return new Response(null, { status: 202 });
      case "tools/list":
        return reply({
          tools: Object.entries(this.schemas).map(([name, schema]) => ({
            name,
            inputSchema: { type: "object", properties: schema.properties, required: schema.required },
          })),
        });
      case "tools/call": {
        const params = body.params as { name: string; arguments: Record<string, unknown> };
        const handler = this.tools.get(params.name);
        if (handler === undefined) {
          return json({ jsonrpc: "2.0", id: body.id, error: { code: -32601, message: "no handler" } });
        }
        const outcome = await handler(params.arguments, request);
        if (outcome instanceof Response) return outcome;
        if (outcome instanceof RpcFailure) {
          return json({
            jsonrpc: "2.0",
            id: body.id,
            error: { code: outcome.code, message: outcome.message, data: outcome.data },
          });
        }
        return reply(toolPayload(outcome));
      }
      default:
        return json({ jsonrpc: "2.0", id: body.id, error: { code: -32601, message: "unknown" } });
    }
  }
}
