/**
 * Classification of every server MCP tool, read by the schema drift test,
 * the static wire-name check and the README table generator. A server tool
 * missing from this table fails the drift test until it is classified.
 *
 * @internal
 * @module
 */

/** A tool the SDK wraps. */
export interface WrappedTool {
  readonly status: "wrapped";
  /** SDK methods that call it. */
  readonly methods: readonly string[];
  /** Phase of the design plan that added it. */
  readonly phase: 1 | 2;
  /**
   * Every wire argument a wrapper can send. The static check confirms each
   * exists in `schema/tools.json`; the unit suite confirms a wrapper sends
   * nothing outside this list.
   */
  readonly sends: readonly string[];
}

/** A tool the SDK deliberately leaves to `callTool`. */
export interface UnwrappedTool {
  readonly status: "not_exposed";
  /** Why it has no wrapper. */
  readonly reason: string;
}

/** Either classification. */
export type ToolClassification = WrappedTool | UnwrappedTool;

const SCOPE = ["workspace", "project"] as const;

/** Every MCP tool the server lists, keyed by tool name. */
export const TOOL_COVERAGE: Readonly<Record<string, ToolClassification>> = {
  memory_resolve_write_target: {
    status: "wrapped",
    methods: ["pages.resolveWriteTarget"],
    phase: 2,
    sends: ["workspace", "project", "project_id"],
  },
  memory_write_pages_batch: {
    status: "wrapped",
    methods: ["pages.writeBatch"],
    phase: 2,
    sends: ["workspace", "project", "project_id", "pages"],
  },
  memory_write_page: {
    status: "wrapped",
    methods: ["pages.write", "pages.update"],
    phase: 1,
    sends: [
      ...SCOPE,
      "path",
      "body",
      "title",
      "tier",
      "tags",
      "pinned",
      "kind",
      "entities",
      "abstract",
      "relations",
      "expires_at",
      "expected_page_id",
      "create_only",
    ],
  },
  memory_patch_page: {
    status: "wrapped",
    methods: ["pages.patch"],
    phase: 2,
    sends: [...SCOPE, "path", "expected_page_id", "edits"],
  },
  memory_read_page: {
    status: "wrapped",
    methods: ["pages.read", "pages.update"],
    phase: 1,
    sends: [...SCOPE, "scope", "path", "query", "include_related", "related_depth", "max_chars"],
  },
  memory_delete_page: {
    status: "wrapped",
    methods: ["pages.delete"],
    phase: 1,
    sends: [...SCOPE, "path", "expected_page_id"],
  },
  memory_query: {
    status: "wrapped",
    methods: ["query"],
    phase: 1,
    sends: [
      ...SCOPE,
      "query",
      "limit",
      "scopes",
      "global",
      "include_expired",
      "include_superseded",
      "pin_first",
      "explain",
      "as_of",
      "answer",
      "reasoning",
      "kind",
      "tier",
      "path_prefix",
      "tag",
    ],
  },
  memory_status: { status: "wrapped", methods: ["status"], phase: 1, sends: [...SCOPE] },
  memory_handoff_begin: {
    status: "wrapped",
    methods: ["handoffs.begin"],
    phase: 1,
    sends: [...SCOPE, "summary", "open_questions", "next_steps", "files_touched", "shared"],
  },
  memory_handoff_list: {
    status: "wrapped",
    methods: ["handoffs.list"],
    phase: 1,
    sends: [...SCOPE, "any_owner", "limit"],
  },
  memory_handoff_accept: {
    status: "wrapped",
    methods: ["handoffs.accept"],
    phase: 1,
    sends: [...SCOPE, "handoff_id", "any_owner"],
  },
  memory_handoff_cancel: {
    status: "wrapped",
    methods: ["handoffs.cancel"],
    phase: 1,
    sends: [...SCOPE, "handoff_id", "any_owner"],
  },
  memory_recent: { status: "wrapped", methods: ["recent"], phase: 2, sends: [...SCOPE, "limit"] },
  memory_message_send: {
    status: "wrapped",
    methods: ["messages.send"],
    phase: 2,
    sends: [
      "to_workspace",
      "to_project",
      "from_workspace",
      "from_project",
      "body",
      "subject",
      "nature",
      "evidence",
    ],
  },
  memory_message_list: {
    status: "wrapped",
    methods: ["messages.list"],
    phase: 2,
    sends: [...SCOPE, "box", "limit"],
  },
  memory_message_pop: {
    status: "wrapped",
    methods: ["messages.pop"],
    phase: 2,
    sends: [...SCOPE, "message_id", "agent"],
  },
  memory_message_cancel: {
    status: "wrapped",
    methods: ["messages.cancel"],
    phase: 2,
    sends: [...SCOPE, "message_id"],
  },
  memory_briefing: {
    status: "wrapped",
    methods: ["briefing"],
    phase: 2,
    sends: [...SCOPE, "recent_pages_limit", "settled_first"],
  },
  memory_explore: {
    status: "wrapped",
    methods: ["explore"],
    phase: 2,
    sends: [...SCOPE, "focus", "recent_pages_limit", "reasoning"],
  },
  memory_read_session_observations: {
    status: "wrapped",
    methods: ["observations"],
    phase: 2,
    sends: [
      ...SCOPE,
      "session_id",
      "limit",
      "offset",
      "order",
      "kinds",
      "query",
      "body_max_chars",
    ],
  },
  memory_feedback: {
    status: "wrapped",
    methods: ["feedback"],
    phase: 2,
    sends: [...SCOPE, "path", "signal", "reason"],
  },
  memory_consolidate: {
    status: "not_exposed",
    reason: "maintenance job that needs the server's LLM; use callTool",
  },
  memory_lint: {
    status: "not_exposed",
    reason: "maintenance job that needs the server's LLM; use callTool",
  },
  memory_auto_improve: {
    status: "not_exposed",
    reason: "maintenance job that needs the server's LLM; use callTool",
  },
  memory_forget_sweep: {
    status: "not_exposed",
    reason: "changes retention for a whole project; use callTool",
  },
  memory_install_self_routing: {
    status: "not_exposed",
    reason: "writes coding-agent instruction files; specific to harnesses",
  },
  memory_restore_page: {
    status: "not_exposed",
    reason: "admin-only restore of one page from a git checkpoint; use callTool",
  },
};

/** Convert a wire name to the SDK's camelCase argument name. */
export function camelCase(wire: string): string {
  return wire.replace(/_([a-z])/g, (_match, letter: string) => letter.toUpperCase());
}
