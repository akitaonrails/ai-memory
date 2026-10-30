/**
 * Typed TypeScript client for an ai-memory server.
 *
 * Start with {@link AiMemoryClient}. Root-only lifecycle operations live in a
 * separate entry point, `ai-memory-ts/admin`.
 *
 * @packageDocumentation
 */

export {
  AiMemoryClient,
  HandoffsNamespace,
  JobsNamespace,
  MessagesNamespace,
  PagesNamespace,
  type CallToolOptions,
  type CapabilitiesOptions,
  type ClientOptions,
} from "./client.js";
export { ApiNamespace } from "./api.js";
export {
  MINIMUM_SERVER_VERSION,
  SUPPORTED_PROTOCOL_VERSIONS,
  type Identity,
  type ServerCapabilities,
  type ServerInfo,
  type ToolCapability,
} from "./capabilities.js";
export {
  AbortError,
  AiMemoryError,
  ArgumentError,
  AuthError,
  ConfigError,
  ForbiddenError,
  HttpError,
  InvalidParamsError,
  NotFoundError,
  PREVIEW_LIMIT,
  PreconditionFailedError,
  ProtocolError,
  RateLimitedError,
  RpcError,
  ScopeError,
  TimeoutError,
  ToolError,
  TransportError,
  UnsupportedServerError,
  preview,
  type ErrorCode,
} from "./errors.js";
export type { Scope } from "./scope.js";
export {
  DEFAULT_TIMEOUT_MS,
  type CallOptions,
  type ConnectionOptions,
  type FetchLike,
} from "./transport.js";
export type * from "./types.js";
