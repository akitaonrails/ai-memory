/**
 * Server capability probing: `initialize`, `tools/list`, `GET /identity` and
 * whether `/api/v1` is mounted.
 *
 * Feature gating reads the tool schemas, not the version string, so a
 * development build reports what it actually serves.
 *
 * @module
 */

import { availability, TOOL_COVERAGE } from "./coverage.js";
import { NotFoundError, ProtocolError, UnsupportedServerError } from "./errors.js";
import type { CallOptions, Transport } from "./transport.js";

/**
 * The oldest server release the SDK supports: 3.0.0 added conditional page
 * writes and deletes (`expected_page_id`, `create_only`) and the version id
 * on page reads.
 */
export const MINIMUM_SERVER_VERSION = "3.0.0";

/** MCP protocol versions this SDK speaks, newest first. */
export const SUPPORTED_PROTOCOL_VERSIONS: readonly string[] = ["2025-03-26", "2024-11-05"];

/** The SDK's identity in the MCP `initialize` handshake. */
const CLIENT_INFO = { name: "ai-memory-ts", version: "0.1.0" };

/** One tool the server lists, reduced to what feature gating needs. */
export interface ToolCapability {
  /** Input-schema property names (wire names). */
  readonly properties: readonly string[];
}

/** Who the server thinks the caller is (`GET /identity`). */
export interface Identity {
  /** Server version string. */
  readonly version: string;
  /** Authority level of this caller. */
  readonly level: "root" | "user" | "anonymous";
  /** Qualified operator key (`user:alice`), or `null` on an unauthenticated server. */
  readonly operator: string | null;
  /** True when the server tells operators apart (it has DB users or proxy identities). */
  readonly distinguishesOperators: boolean;
}

/** `serverInfo` from the MCP handshake. */
export interface ServerInfo {
  /** Server name (`ai-memory`). */
  readonly name: string;
  /** Server version string. */
  readonly version: string;
}

/** What {@link AiMemoryClient.capabilities} reports. */
export interface ServerCapabilities {
  /** `serverInfo` from `initialize`. */
  readonly server: ServerInfo;
  /** The MCP protocol version the server agreed to. */
  readonly protocolVersion: string;
  /** True when `initialize` opened a stateful session (`--http-stateful`). */
  readonly statefulSession: boolean;
  /** Every tool the server lists, keyed by name. */
  readonly tools: Readonly<Record<string, ToolCapability>>;
  /** The caller's identity, or `null` when the server has no `/identity` route. */
  readonly identity: Identity | null;
  /** True when `/api/v1` answers (the server runs with `--enable-api` or `--enable-web`). */
  readonly apiMounted: boolean;
  /**
   * True when the server has the conditional-write seam this SDK requires
   * (`memory_write_page.expected_page_id`), that is, it is 3.0.0 or newer.
   */
  readonly meetsMinimumVersion: boolean;
}

/** The handshake and tool list, cached per client. @internal */
export interface ToolIndex {
  server: { name: string; version: string };
  protocolVersion: string;
  statefulSession: boolean;
  tools: Record<string, ToolCapability>;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** The handshake result, shared by raw and wrapped tool calls. @internal */
export type SessionInfo = Omit<ToolIndex, "tools">;

/** Run `initialize` plus `notifications/initialized` when stateful. @internal */
export async function initialize(transport: Transport, options?: CallOptions): Promise<SessionInfo> {
  transport.resetSession();
  const newest = SUPPORTED_PROTOCOL_VERSIONS[0] ?? "2024-11-05";
  const { result, headers } = await transport.rpcWithHeaders(
    "initialize",
    { protocolVersion: newest, capabilities: {}, clientInfo: CLIENT_INFO },
    false,
    options,
  );
  const protocolVersion = result["protocolVersion"];
  const serverInfo = result["serverInfo"];
  const server = {
    name: isRecord(serverInfo) && typeof serverInfo["name"] === "string" ? serverInfo["name"] : "",
    version:
      isRecord(serverInfo) && typeof serverInfo["version"] === "string" ? serverInfo["version"] : "",
  };
  if (typeof protocolVersion !== "string") {
    throw new ProtocolError("initialize answered without a protocolVersion");
  }
  if (!SUPPORTED_PROTOCOL_VERSIONS.includes(protocolVersion)) {
    throw new UnsupportedServerError(
      `MCP protocol version ${transport.safeText(protocolVersion)}`,
      MINIMUM_SERVER_VERSION,
      server.version,
    );
  }
  const statefulSession = transport.adoptSession(headers);
  if (statefulSession) await transport.notify("notifications/initialized", options);
  return {
    server: { name: transport.safeText(server.name), version: transport.safeText(server.version) },
    protocolVersion,
    statefulSession,
  };
}

/** List schemas after the shared handshake. @internal */
export async function probeTools(
  transport: Transport,
  session: SessionInfo,
  options?: CallOptions,
): Promise<ToolIndex> {
  const listed = await transport.rpc("tools/list", {}, false, options);
  const rawTools = listed["tools"];
  if (!Array.isArray(rawTools)) throw new ProtocolError("tools/list answered without tools");
  const tools: Record<string, ToolCapability> = {};
  for (const tool of rawTools) {
    if (!isRecord(tool) || typeof tool["name"] !== "string") continue;
    const schema = tool["inputSchema"];
    const properties =
      isRecord(schema) && isRecord(schema["properties"]) ? Object.keys(schema["properties"]) : [];
    tools[tool["name"]] = { properties };
  }
  return { ...session, tools };
}

/**
 * Refuse a call the server cannot honour: the tool is missing, or it lacks an
 * argument the call sends (an older server would ignore an unknown argument
 * and change the call's meaning, for example turn a guarded write into an
 * unconditional one).
 * @internal
 */
export function requireFeature(index: ToolIndex, tool: string, args: Iterable<string>): void {
  const capability = index.tools[tool];
  if (capability === undefined) {
    const entry = TOOL_COVERAGE[tool];
    const when = entry === undefined ? undefined : availability(entry);
    const note = when === undefined ? undefined : `the server lists it only when ${when}`;
    throw new UnsupportedServerError(
      `the ${tool} tool`,
      MINIMUM_SERVER_VERSION,
      index.server.version,
      note,
    );
  }
  for (const arg of args) {
    if (!capability.properties.includes(arg)) {
      throw new UnsupportedServerError(
        `${tool}.${arg}`,
        MINIMUM_SERVER_VERSION,
        index.server.version,
      );
    }
  }
}

/** `GET /identity`, or `null` when the server has no such route. @internal */
export async function probeIdentity(
  transport: Transport,
  options?: CallOptions,
): Promise<Identity | null> {
  let body: unknown;
  try {
    body = await transport.json(
      "GET",
      "/identity",
      { route: "identity", mutating: false },
      options,
    );
  } catch (error) {
    if (error instanceof NotFoundError) return null;
    throw error;
  }
  return decodeIdentity(transport, body);
}

/** Decode an `/identity` body. @internal */
export function decodeIdentity(transport: Transport, body: unknown): Identity {
  if (!isRecord(body)) throw new ProtocolError("/identity answered with a non-object");
  const level = body["level"];
  const version = body["version"];
  const operator = body["operator"];
  if (level !== "root" && level !== "user" && level !== "anonymous") {
    throw new ProtocolError("/identity answered without a known level");
  }
  if (typeof version !== "string") throw new ProtocolError("/identity answered without a version");
  if (operator !== null && typeof operator !== "string") {
    throw new ProtocolError("/identity answered with a malformed operator");
  }
  return {
    version: transport.safeText(version),
    level,
    operator,
    distinguishesOperators: body["distinguishes_operators"] === true,
  };
}

/** True when `/api/v1` answers; a 404 means the API is not mounted. @internal */
export async function probeApi(transport: Transport, options?: CallOptions): Promise<boolean> {
  try {
    await transport.json("GET", "/api/v1/workspaces", { route: "api", mutating: false }, options);
    return true;
  } catch (error) {
    if (error instanceof NotFoundError) return false;
    throw error;
  }
}
