/**
 * Classification of every server MCP tool, read by the schema drift test,
 * the static wire-name check and the README table generator. A server tool
 * missing from this table fails the drift test until it is classified.
 *
 * @internal
 * @module
 */

/**
 * Server feature a tool depends on. A server lists a tool with a `feature`
 * only when the operator turned the feature on (a fresh flag-off server must
 * not list it), except a `retainedAfterDisable` read-only tool, which a
 * flag-off server also lists while data the feature created still exists.
 */
export type ToolFeature = "chunking";

/**
 * The condition under which the server lists a gated tool, as a phrase that
 * completes "listed only when ...". Used in {@link UnsupportedServerError}
 * and the generated docs.
 */
export const FEATURE_CONDITIONS: Readonly<Record<ToolFeature, string>> = {
  chunking:
    "chunked session processing is enabled (`[chunking] enabled = true`, restart required)",
};

/** A tool the SDK wraps. */
export interface WrappedTool {
  readonly status: "wrapped";
  /** SDK methods that call it. */
  readonly methods: readonly string[];
  /** Phase of the design plan that added it. */
  readonly phase: 1 | 2 | 3;
  /**
   * Every wire argument a wrapper can send. The static check confirms each
   * exists in the matching snapshot; the unit suite confirms a wrapper sends
   * nothing outside this list.
   */
  readonly sends: readonly string[];
  /** Present only when the server lists the tool with this feature enabled. */
  readonly feature?: ToolFeature;
  /**
   * The read-only tool stays listed after the feature is turned off while
   * data the feature created still exists; start-type tools never do.
   */
  readonly retainedAfterDisable?: boolean;
}

/** A tool the SDK deliberately leaves to `callTool`. */
export interface UnwrappedTool {
  readonly status: "not_exposed";
  /** Why it has no wrapper. */
  readonly reason: string;
  /** Present only when the server lists the tool with this feature enabled. */
  readonly feature?: ToolFeature;
  /** See {@link WrappedTool.retainedAfterDisable}. */
  readonly retainedAfterDisable?: boolean;
}

/** Either classification. */
export type ToolClassification = WrappedTool | UnwrappedTool;

const SCOPE = ["workspace", "project"] as const;

/** Every MCP tool the server lists, keyed by tool name. */
export const TOOL_COVERAGE: Readonly<Record<string, ToolClassification>> = {
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
  memory_processing_job_start: {
    status: "wrapped",
    methods: ["jobs.start"],
    phase: 3,
    sends: [...SCOPE, "session_id", "kind"],
    feature: "chunking",
  },
  memory_processing_job_status: {
    status: "wrapped",
    methods: ["jobs.status"],
    phase: 3,
    sends: [...SCOPE, "job_id"],
    feature: "chunking",
    retainedAfterDisable: true,
  },
};

/**
 * The tool names a server lists: every unconditional tool, plus the tools of
 * each enabled feature. With `existingFeatureData`, a server that has the
 * feature off but still stores data an enabled run created also lists the
 * read-only tools marked `retainedAfterDisable`, never the others.
 * `schema/tools.json` is the fresh flag-off snapshot (`[]`, no data),
 * `schema/tools.<feature>.json` the snapshot with that feature on, and
 * `schema/tools.<feature>-retained.json` the flag-off snapshot of a data
 * directory an enabled run left jobs in.
 */
export function expectedTools(
  features: readonly ToolFeature[],
  existingFeatureData = false,
): string[] {
  return Object.entries(TOOL_COVERAGE)
    .filter(
      ([, entry]) =>
        entry.feature === undefined ||
        features.includes(entry.feature) ||
        (existingFeatureData && entry.retainedAfterDisable === true),
    )
    .map(([name]) => name);
}

/**
 * When the server lists a gated tool, as a phrase that completes "listed
 * only when ...". Used in {@link UnsupportedServerError} and the generated
 * docs; `undefined` for an unconditional tool.
 */
export function availability(entry: ToolClassification): string | undefined {
  if (entry.feature === undefined) return undefined;
  const condition = FEATURE_CONDITIONS[entry.feature];
  return entry.retainedAfterDisable === true ? `${condition}, or earlier jobs from an enabled run are still stored (read-only)` : condition;
}

/** Convert a wire name to the SDK's camelCase argument name. */
export function camelCase(wire: string): string {
  return wire.replace(/_([a-z])/g, (_match, letter: string) => letter.toUpperCase());
}
