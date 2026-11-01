/**
 * Argument and result types. Argument names are the server's wire names in
 * camelCase (`expected_page_id` becomes `expectedPageId`). Every result keeps
 * the server's decoded JSON in `raw`, so fields a newer server adds are
 * available without an SDK update.
 *
 * @module
 */

import type { Scope } from "./scope.js";

declare const untrusted: unique symbol;

/**
 * Text that came out of stored memory: page bodies and titles, snippets,
 * handoff summaries, message bodies, observation bodies. It is an ordinary
 * string at runtime. The brand reminds the caller that the content is
 * historical data to quote, never instructions to follow: memory can hold
 * instruction-like text written by anyone with write access.
 */
export type UntrustedText = string & {
  /** Compile-time brand only; absent at runtime. */
  readonly [untrusted]: true;
};

/** The server's decoded JSON for a result, unchanged. */
export type Raw = Readonly<Record<string, unknown>>;

/** Memory tier of a page. The server accepts these four values. */
export type Tier = "working" | "episodic" | "semantic" | "procedural";

/** Typed relation names a page can declare toward other pages. */
export type RelationName = "causes" | "fixes" | "contradicts";

/** Reasoning budget for the opt-in LLM paths (`answer`, `explore`). */
export type ReasoningTier = "minimal" | "low" | "medium" | "high" | "max";

/** Signals accepted by {@link AiMemoryClient.feedback}. */
export type FeedbackSignal = "helpful" | "not_helpful" | "stale" | "wrong";

/** Present on every scoped argument object: overrides the client's default scope. */
export interface ScopedArgs {
  /** Workspace and project for this call. Defaults to the client's `scope`. */
  scope?: Scope;
}

/** An existing project selected by name or by its stable id. */
export type ExistingProjectTarget =
  | {
      /** Existing workspace name. */
      workspace: string;
      /** Existing project name. */
      project: string;
      /** Not allowed together with `project`. */
      projectId?: never;
    }
  | {
      /** Existing workspace name. */
      workspace: string;
      /** Stable id of an existing project in the workspace. */
      projectId: string;
      /** Not allowed together with `projectId`. */
      project?: never;
    };

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/**
 * Page metadata a write sets. A write replaces the whole page: metadata the
 * call omits is cleared, not inherited. Use {@link PagesNamespace.update} to
 * change a page while keeping its metadata.
 */
export interface PageMetadata {
  /** Explicit title. Prefer a `# H1` on the first line of the body instead. */
  title?: string;
  /** Memory tier. Default `semantic`. */
  tier?: Tier;
  /** Tags. */
  tags?: string[];
  /** Pin the page so the decay sweep skips it. */
  pinned?: boolean;
  /** Semantic kind, at most 64 characters. */
  kind?: string;
  /** Up to 10 entity names; the server lowercases and deduplicates them. */
  entities?: string[];
  /** Short summary, at most 1024 characters. */
  abstract?: string;
  /** Typed relations to existing pages (`path`, `project:path`, `workspace/project:path`). */
  relations?: Partial<Record<RelationName, string[]>>;
  /** TTL: an RFC 3339 instant or a bare `YYYY-MM-DD` date. Expired pages are hidden. */
  expiresAt?: string;
}

/** Arguments of {@link PagesNamespace.write}. */
export interface WritePageArgs extends PageMetadata, ScopedArgs {
  /** Relative wiki path, for example `notes/retries.md`. */
  path: string;
  /** Markdown body. Start it with `# Title`. */
  body: string;
  /**
   * Write only if this is still the page's latest version id (the `pageId`
   * a read or an earlier write returned). Otherwise the call fails with
   * {@link PreconditionFailedError} and writes nothing.
   */
  expectedPageId?: string;
  /** Write only if no page exists at `path` yet. Not combinable with `expectedPageId`. */
  createOnly?: boolean;
}

/** Result of {@link PagesNamespace.write}. */
export interface WritePageResult {
  /** The new version id; pass it as `expectedPageId` to the next conditional write. */
  readonly pageId: string;
  /** Canonical wiki path written. */
  readonly path: string;
  /** Git checkpoint of the write, when the server reported one. */
  readonly checkpoint: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link PagesNamespace.resolveWriteTarget}. */
export interface ResolveWriteTargetArgs {
  /** Exact existing destination; this operation never creates a scope. */
  target: ExistingProjectTarget;
}

/** Canonical existing destination returned by a write-access preflight. */
export interface ResolvedWriteTarget {
  /** Canonical workspace name. */
  readonly workspace: string;
  /** Canonical project name. */
  readonly project: string;
  /** Stable project id. */
  readonly projectId: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** One complete, conditional page version in an atomic batch. */
export type BatchWritePage = PageMetadata & {
  /** Relative wiki path. */
  path: string;
  /** Complete Markdown body for this version. */
  body: string;
} &
  (
    | {
        /** Require this latest page version. */
        expectedPageId: string;
        /** Not allowed together with `expectedPageId`. */
        createOnly?: never;
      }
    | {
        /** Require the path to be absent. */
        createOnly: true;
        /** Not allowed together with `createOnly`. */
        expectedPageId?: never;
      }
  );

/** Arguments of {@link PagesNamespace.writeBatch}. */
export interface WritePagesBatchArgs {
  /** Exact existing destination; batch writes never create a scope. */
  target: ExistingProjectTarget;
  /** One to 100 complete page versions, committed atomically. */
  pages: readonly BatchWritePage[];
}

/** One page written by {@link PagesNamespace.writeBatch}. */
export interface BatchWritePageResult {
  /** Zero-based request position. */
  readonly index: number;
  /** Canonical wiki path written. */
  readonly path: string;
  /** New page version id. */
  readonly pageId: string;
  /** The server's decoded item JSON. */
  readonly raw: Raw;
}

/** Result of {@link PagesNamespace.writeBatch}. */
export interface WritePagesBatchResult {
  /** Results in request order. */
  readonly pages: readonly BatchWritePageResult[];
  /** One git checkpoint for the whole batch, when reported. */
  readonly checkpoint: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A bounded heading token returned by `memory_read_page`. */
export interface PageSection {
  /** Heading text without Markdown markers. */
  readonly anchor: string;
  /** ATX heading level, 1 through 6. */
  readonly level: number;
  /** SHA-256 of the exact section bytes, including its heading. */
  readonly sha256: string;
}

/** One atomic anchored page edit. */
export type PagePatchEdit =
  | {
      /** Append operation. */
      op: "append";
      /** Insert before the next same-or-higher-level heading. */
      target: "section";
      /** Unique existing heading. */
      anchor: string;
      /** Markdown to append. */
      content: string;
    }
  | {
      /** Append operation. */
      op: "append";
      /** Insert at the end of the page. */
      target: "end_of_page";
      /** An EOF append has no heading anchor. */
      anchor?: never;
      /** Markdown to append. */
      content: string;
    }
  | {
      /** Add a new section at EOF. */
      op: "add_section";
      /** Unique ATX level 2-6 heading, for example `## Decisions`. */
      anchor: string;
      /** New section body. */
      content: string;
    }
  | {
      /** Replace one existing section body. */
      op: "replace_section";
      /** Unique existing heading. */
      anchor: string;
      /** Exact section token returned by {@link Page.sections}. */
      sectionSha256: string;
      /** Replacement section body, excluding the heading. */
      content: string;
    };

/** Arguments of {@link PagesNamespace.patch}. */
export interface PatchPageArgs extends ScopedArgs {
  /** Existing page path. */
  path: string;
  /** Required latest page version returned by a read or mutation. */
  expectedPageId: string;
  /** Bounded batch resolved against one base body and committed atomically. */
  edits: readonly PagePatchEdit[];
}

/** Result of {@link PagesNamespace.patch}. */
export type PatchPageResult = WritePageResult;

/** Options shared by both forms of {@link ReadPageArgs}. */
export interface ReadPageBase extends ScopedArgs {
  /** Read from the reserved cross-project global scope. Cannot be combined with `scope`. */
  global?: boolean;
  /** Also walk the link graph and return the reachable pages in `related`. */
  includeRelated?: boolean;
  /** Hops to walk with `includeRelated` (default 1, capped at 3 by the server). */
  relatedDepth?: number;
  /** Cap the returned body at this many characters (default 12000, 500 to 64000). */
  maxChars?: number;
}

/** Arguments of {@link PagesNamespace.read}: exactly one of `path` or `query`. */
export type ReadPageArgs =
  | (ReadPageBase & {
      /** Exact wiki path. */
      path: string;
      /** Not allowed together with `path`. */
      query?: never;
    })
  | (ReadPageBase & {
      /** Search query; the top hit's full page is returned. */
      query: string;
      /** Not allowed together with `query`. */
      path?: never;
    });

/** A page reachable from the page read, when `includeRelated` was set. */
export interface RelatedPage {
  /** Wiki path. */
  readonly path: string;
  /** Page title. */
  readonly title: UntrustedText;
  /** Page kind. */
  readonly kind: string | undefined;
  /** Workspace of the related page. */
  readonly workspace: string | undefined;
  /** Project of the related page. */
  readonly project: string | undefined;
  /** Hops from the page read. */
  readonly depth: number | undefined;
  /** `link` or `backlink`. */
  readonly direction: string | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A full page, as {@link PagesNamespace.read} returns it. */
export interface Page {
  /** Latest version id: the token for a conditional write or delete. */
  readonly pageId: string;
  /** Wiki path. */
  readonly path: string;
  /** Page title. */
  readonly title: UntrustedText;
  /** Markdown body, cut at `maxChars` with a visible marker when `truncated`. */
  readonly body: UntrustedText;
  /** Whether the server cut the body at `maxChars`. */
  readonly truncated: boolean | undefined;
  /** Length in characters of the untruncated body. */
  readonly totalChars: number | undefined;
  /** Parsed frontmatter (tags, tier, pinned and other metadata). Untrusted. */
  readonly frontmatter: Readonly<Record<string, unknown>>;
  /** Bounded heading hashes for anchored patches. */
  readonly sections: readonly PageSection[];
  /** True when the page has more sections than the response cap. */
  readonly sectionsTruncated: boolean;
  /** Pages reachable through links, present only with `includeRelated`. */
  readonly related: readonly RelatedPage[] | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link PagesNamespace.delete}. */
export interface DeletePageArgs extends ScopedArgs {
  /** Exact wiki path to delete. */
  path: string;
  /** Delete only if this is still the page's latest version id. */
  expectedPageId?: string;
}

/** Result of {@link PagesNamespace.delete}. */
export interface DeletePageResult {
  /** Path deleted. */
  readonly path: string;
  /** True when the page is gone (deletes are idempotent). */
  readonly deleted: boolean;
  /**
   * Checkpoint taken before the delete, or `null` when the tree was already
   * clean. Restore from it (or `<checkpoint>~1`) with `AdminClient.restorePage`.
   */
  readonly preCheckpoint: string | null;
  /** Checkpoint recording the delete, or `null` when nothing changed. */
  readonly checkpoint: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/**
 * A change for {@link PagesNamespace.update}: the fields to set. Omitted
 * fields keep their current value; `null` clears a metadata field.
 */
export type PageChange = {
  /** New body. Omit to keep the current one. */
  body?: string;
} & { [K in keyof PageMetadata]?: PageMetadata[K] | null };

/** Options of {@link PagesNamespace.update}. */
export interface UpdatePageOptions extends ScopedArgs {}

// ---------------------------------------------------------------------------
// Query, recent, status
// ---------------------------------------------------------------------------

interface QueryBase {
  /** FTS5 query expression, for example `"retry policy"` or `retry OR backoff`. */
  query: string;
  /** Maximum hits (server default 10, clamped to 100). */
  limit?: number;
  /** Also return pages whose TTL has passed. */
  includeExpired?: boolean;
  /** Attach score provenance to hits (diagnostics). */
  explain?: boolean;
  /** Keep only pages of this kind (ASCII case-insensitive; custom kinds allowed). */
  kind?: string;
  /** Keep only pages in this memory tier. */
  tier?: Tier;
  /** Keep only pages whose project-relative path starts with this literal prefix. */
  pathPrefix?: string;
  /** Keep only pages whose frontmatter `tags` contain this exact tag (ASCII case-insensitive). */
  tag?: string;
}

/** Search one project: the call's `scope` or the client default. */
export interface ProjectQueryArgs extends QueryBase, ScopedArgs {
  /** Not allowed: use {@link MultiProjectQueryArgs}. */
  scopes?: never;
  /** Not global. */
  global?: false;
  /** Also return superseded versions, each marked `superseded: true`. */
  includeSuperseded?: boolean;
  /** Prepend the project's pinned pages ahead of the hits. */
  pinFirst?: boolean;
  /** Time travel: what the store knew at this ISO-8601 instant. */
  asOf?: string;
  /** Ask the server's LLM (if configured) for a cited answer over the hits. */
  answer?: boolean;
  /** Token budget tier for `answer`. */
  reasoning?: ReasoningTier;
}

/** Search several named projects at once. */
export interface MultiProjectQueryArgs extends QueryBase {
  /** Projects to search; each needs both names. */
  scopes: readonly Scope[];
  /** Not allowed together with `scopes`. */
  scope?: never;
  /** Not global. */
  global?: false;
  /** Also return superseded versions. */
  includeSuperseded?: boolean;
  /** Ask the server's LLM (if configured) for a cited answer over the hits. */
  answer?: boolean;
  /** Token budget tier for `answer`. */
  reasoning?: ReasoningTier;
}

/** Search every project the caller may read. Takes no scope. */
export interface GlobalQueryArgs extends QueryBase {
  /** Cross-project search. */
  global: true;
  /** Not allowed: a global query takes no scope. */
  scope?: never;
  /** Not allowed: a global query takes no scope. */
  scopes?: never;
}

/** Arguments of {@link AiMemoryClient.query}. */
export type QueryArgs = ProjectQueryArgs | MultiProjectQueryArgs | GlobalQueryArgs;

/** One search hit. */
export interface QueryHit {
  /** Version id of the page hit (absent on global hits). */
  readonly id: string | undefined;
  /** Wiki path. */
  readonly path: string;
  /** Page title. */
  readonly title: UntrustedText;
  /** FTS5 snippet with `<mark>` around matches. */
  readonly snippet: UntrustedText;
  /** Rank; lower is better. */
  readonly rank: number;
  /** True for an older version returned because of `includeSuperseded`. */
  readonly superseded: boolean;
  /** True for a pinned page prepended by `pinFirst`. */
  readonly pinned: boolean;
  /** Workspace of the hit; set on global hits only. */
  readonly workspace: string | undefined;
  /** Project of the hit; set on global hits only. */
  readonly project: string | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A raw-observation fallback hit, returned when compiled pages miss. */
export interface ObservationHit {
  /** Observation id. */
  readonly id: string;
  /** Session the observation belongs to. */
  readonly sessionId: string;
  /** Observation kind. */
  readonly kind: string;
  /** Observation title. */
  readonly title: UntrustedText;
  /** FTS5 snippet. */
  readonly snippet: UntrustedText;
  /** Rank; lower is better. */
  readonly rank: number;
  /** Creation time (ISO 8601). */
  readonly createdAt: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** An LLM-synthesized answer over the top hits. */
export interface QueryAnswer {
  /** The answer text, written by the server's model from stored memory. */
  readonly text: UntrustedText;
  /** Page paths the answer drew from. */
  readonly citations: readonly string[];
}

/** Optional server-side context admission receipt for answer synthesis. */
export interface QueryAnswerContext {
  /** Number of source snippets admitted to the model input. */
  readonly admitted: number;
  /** Number of retrieval snippets not admitted. Retrieval hits stay complete. */
  readonly omitted: number;
  /** Number of admitted snippets clipped on a UTF-8 boundary. */
  readonly clipped: number;
  /** Conservative byte-based input estimate, including schema and envelope allowance. */
  readonly estimatedInputTokens: number;
  /** Output token allowance reserved for the requested reasoning tier. */
  readonly reservedOutputTokens: number;
  /** Server-configured combined context target; not a tokenizer guarantee. */
  readonly maxContextTokens: number;
}

/** Result of {@link AiMemoryClient.query}. */
export interface QueryResult {
  /** Page hits. A global query puts its cross-project hits here, with `workspace` and `project` set. */
  readonly hits: readonly QueryHit[];
  /** Standing preferences from the reserved `_global` scope (single-project queries). */
  readonly globalScopeHits: readonly QueryHit[];
  /** Raw-observation fallback hits. */
  readonly rawHits: readonly ObservationHit[];
  /** The synthesized answer, when `answer` was set and the server has an LLM. */
  readonly answer: QueryAnswer | undefined;
  /** Why no answer was produced, when `answer` was set. */
  readonly answerUnavailable: string | undefined;
  /** Present only when the server applied its optional answer context budget. */
  readonly answerContext?: QueryAnswerContext;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link AiMemoryClient.recent}. */
export interface RecentArgs extends ScopedArgs {
  /** Maximum pages (server default 10, clamped to 100). */
  limit?: number;
}

/** Result of {@link AiMemoryClient.recent}. */
export interface RecentResult {
  /** Most recently updated pages, newest first. */
  readonly hits: readonly QueryHit[];
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** The scope a tool answered for. */
export interface AnsweredScope {
  /** Workspace name. */
  readonly workspace: string;
  /** Project name. */
  readonly project: string;
  /** How the server chose it; `explicit` for every SDK call. */
  readonly resolvedBy: string;
}

/** Lifetime counts of one project. */
export interface StatusCounts {
  /** Latest page versions. */
  readonly pagesLatest: number;
  /** All page versions, superseded included. */
  readonly pagesAll: number;
  /** Sessions. */
  readonly sessions: number;
  /** Observations. */
  readonly observations: number;
}

/** Result of {@link AiMemoryClient.status}. */
export interface StatusResult {
  /** Counts for the scope. */
  readonly counts: StatusCounts;
  /** The scope the counts belong to. */
  readonly scope: AnsweredScope;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

// ---------------------------------------------------------------------------
// Briefing, explore, observations, feedback
// ---------------------------------------------------------------------------

/** A page reference in a briefing or listing. */
export interface PageSummary {
  /** Wiki path. */
  readonly path: string;
  /** Page title. */
  readonly title: UntrustedText;
  /** Page kind (`note`, `rule`, `decision`, ...). */
  readonly kind: string | undefined;
  /** Memory tier, when the server sends it. */
  readonly tier: string | undefined;
  /** Last update (ISO 8601). */
  readonly updatedAt: string | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Activity over a window of days. */
export interface ActivityWindow {
  /** Window length in days. */
  readonly days: number;
  /** Sessions in the window. */
  readonly sessions: number;
  /** Observations in the window. */
  readonly observations: number;
  /** Pages updated in the window. */
  readonly pagesUpdated: number;
}

/** A structured project snapshot (no LLM call). */
export interface Briefing {
  /** Lifetime counts. */
  readonly counts: StatusCounts;
  /** Last 7 days. */
  readonly activity7d: ActivityWindow;
  /** Last 30 days. */
  readonly activity30d: ActivityWindow;
  /** Time of the newest observation, or `null`. */
  readonly lastObservationAt: string | null;
  /** Open handoffs this caller can see. */
  readonly pendingHandoffCount: number;
  /** `_rules/` pages. */
  readonly rules: readonly PageSummary[];
  /** `_slots/` pages. */
  readonly slots: readonly PageSummary[];
  /** Most recently updated pages. */
  readonly recentPages: readonly PageSummary[];
  /**
   * Sessions with the newest captured work, newest first (`memory_briefing`
   * only; empty from `/api/v1`). An entry with `ended: false` that is not
   * `current` was cut off (a quota limit, a crash) or is still running.
   */
  readonly recentSessions: readonly SessionActivity[];
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** One session's newest captured work, from `memory_briefing` or `memory_handoff_list`. */
export interface SessionActivity {
  /** Session id; pass it to {@link AiMemoryClient.observations}. */
  readonly sessionId: string;
  /** Agent kind as stored (`claude-code`, `open-code`, ...). */
  readonly agentKind: string;
  /** Captured session cwd, if any. */
  readonly cwd: string | undefined;
  /** Time of the session's newest prompt, tool, stop or compaction observation. */
  readonly lastActivityAt: string;
  /** Whether the session ended at or after that work. */
  readonly ended: boolean;
  /** Whether a handoff from the session was written at or after that work. */
  readonly handedOff: boolean;
  /** The caller's own session (`memory_briefing` only, when the client forwards its id). */
  readonly current: boolean;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link AiMemoryClient.briefing}. */
export interface BriefingArgs extends ScopedArgs {
  /** How many recent pages to include (default 10, max 100). */
  recentPagesLimit?: number;
  /** Lead with the project's settled rule and decision pages. */
  settledFirst?: boolean;
}

/** Arguments of {@link AiMemoryClient.explore}. */
export interface ExploreArgs extends ScopedArgs {
  /** Topic to bias the digest toward. */
  focus?: string;
  /** How many recent pages the underlying briefing considers. */
  recentPagesLimit?: number;
  /** Token budget tier when the server has an LLM. */
  reasoning?: ReasoningTier;
}

/** Result of {@link AiMemoryClient.explore}. */
export interface ExploreResult {
  /** The LLM digest, or `null` when the server has no provider. */
  readonly prose: UntrustedText | null;
  /** Why there is no prose, when there is none. */
  readonly reason: string | undefined;
  /** The structured briefing the digest is based on, when the server returned it. */
  readonly briefing: Briefing | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Observation kinds the server records. */
export type ObservationKind =
  | "session-start"
  | "user-prompt"
  | "pre-tool-use"
  | "post-tool-use"
  | "pre-compact"
  | "post-compaction"
  | "notification"
  | "stop"
  | "session-end"
  | "other";

/** Arguments of {@link AiMemoryClient.observations}. */
export interface ObservationsArgs extends ScopedArgs {
  /**
   * Session id. Omit for the newest other session with captured work in the
   * project, ended or cut off (the caller's cwd first), or the latest
   * completed one when nothing worked in the last 30 days.
   */
  sessionId?: string;
  /** Maximum observations (server default 20, clamped to 200). */
  limit?: number;
  /** Observations to skip. */
  offset?: number;
  /** `asc` (capture order, default) or `desc`. */
  order?: "asc" | "desc";
  /** Keep only these kinds. */
  kinds?: ObservationKind[];
  /** Full-text query restricted to the session. */
  query?: string;
  /** Cap each body at this many characters (200 to 16384, default 1000). */
  bodyMaxChars?: number;
}

/** A session that touched a project. */
export interface SessionSummary {
  /** Session id. */
  readonly sessionId: string;
  /** Harness that ran it. */
  readonly agentKind: string | undefined;
  /** Working directory it ran in. */
  readonly cwd: string | null;
  /** Start time (ISO 8601). */
  readonly startedAt: string | undefined;
  /** End time, or `null` while open. */
  readonly endedAt: string | null;
  /** Observations it left in this project. */
  readonly observationCount: number;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** One captured observation. */
export interface Observation {
  /** Observation id. */
  readonly id: string;
  /** Session id. */
  readonly sessionId: string;
  /** Kind. */
  readonly kind: string;
  /** Title. */
  readonly title: UntrustedText;
  /** Sanitized, possibly truncated body. */
  readonly body: UntrustedText;
  /** Importance in 1..10. */
  readonly importance: number;
  /** Capture time (ISO 8601). */
  readonly createdAt: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** One page of a session's observations. */
export interface ObservationsResult {
  /** The session read. */
  readonly session: SessionSummary;
  /** Observations in the requested order. */
  readonly observations: readonly Observation[];
  /** Matching observations in scope, for paging on `offset`. */
  readonly total: number;
  /** Offset applied. */
  readonly offset: number;
  /** Limit applied after clamping. */
  readonly limit: number;
  /** Rows the same session left in other projects. */
  readonly elidedOtherScope: number;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link AiMemoryClient.feedback}. */
export interface FeedbackArgs extends ScopedArgs {
  /** Exact wiki path of the page rated. */
  path: string;
  /** The signal. */
  signal: FeedbackSignal;
  /** Optional reason (one line, at most 500 characters stored). */
  reason?: string;
}

/** Result of {@link AiMemoryClient.feedback}. */
export interface FeedbackResult {
  /** True when the signal was recorded. */
  readonly recorded: boolean;
  /** Path rated. */
  readonly path: string;
  /** Signal recorded. */
  readonly signal: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

// ---------------------------------------------------------------------------
// Handoffs
// ---------------------------------------------------------------------------

/** Arguments of {@link HandoffsNamespace.begin}. */
export interface BeginHandoffArgs extends ScopedArgs {
  /** Short summary of where the work stopped. */
  summary: string;
  /** Questions for the next run. */
  openQuestions?: string[];
  /** Suggested next steps. */
  nextSteps?: string[];
  /** Files touched. */
  filesTouched?: string[];
  /** Publish to everyone in the project instead of keeping it for this operator. */
  shared?: boolean;
}

/** Result of {@link HandoffsNamespace.begin}. */
export interface BeginHandoffResult {
  /** Id to accept or cancel exactly this handoff. */
  readonly handoffId: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A handoff as `memory_handoff_list` and `memory_handoff_accept` return it. */
export interface Handoff {
  /** Handoff id. */
  readonly id: string;
  /** Summary. */
  readonly summary: UntrustedText;
  /** Open questions. */
  readonly openQuestions: readonly UntrustedText[];
  /** Next steps. */
  readonly nextSteps: readonly UntrustedText[];
  /** Files touched. */
  readonly filesTouched: readonly string[];
  /** State when the server read the row (`open`, `accepted`, `expired`). */
  readonly state: string;
  /** Creation time. */
  readonly createdAt: string | undefined;
  /** Owning operator key, or `null` for a shared or unattributed handoff. */
  readonly owner: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link HandoffsNamespace.list}. */
export interface ListHandoffsArgs extends ScopedArgs {
  /** Root-only recovery: also list other operators' handoffs. */
  anyOwner?: boolean;
  /** Maximum handoffs (default 50, clamped to 200). */
  limit?: number;
}

/** Result of {@link HandoffsNamespace.list}. */
export interface ListHandoffsResult {
  /** Open handoffs this caller may claim. */
  readonly handoffs: readonly Handoff[];
  /**
   * A session that never ended and holds newer work than every open handoff,
   * when there is one: read it before relying on an older handoff.
   */
  readonly newerUnendedSession: SessionActivity | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link HandoffsNamespace.accept}. */
export interface AcceptHandoffArgs extends ScopedArgs {
  /** Claim exactly this open handoff. Omit to claim the latest eligible one. */
  handoffId?: string;
  /** Root-only recovery: also consider other operators' handoffs. */
  anyOwner?: boolean;
}

/** Result of {@link HandoffsNamespace.accept}. A handoff is claimed once. */
export type AcceptHandoffResult =
  | {
      /** This call claimed the handoff. */
      readonly status: "claimed";
      /** The claimed handoff. */
      readonly handoff: Handoff;
      /** The server's decoded JSON. */
      readonly raw: Raw;
    }
  | {
      /** The calling session's SessionStart hook claimed it (not reachable from the SDK). */
      readonly status: "consumed_by_hook";
      /** The handoff, when the server returned it. */
      readonly handoff: Handoff | null;
      /** The server's decoded JSON. */
      readonly raw: Raw;
    }
  | {
      /** Nothing left for this caller to claim. */
      readonly status: "none_pending";
      /** Always `null`. */
      readonly handoff: null;
      /** The server's decoded JSON. */
      readonly raw: Raw;
    };

/** Arguments of {@link HandoffsNamespace.cancel}. */
export interface CancelHandoffArgs extends ScopedArgs {
  /** Exact handoff id. */
  handoffId: string;
  /** Cancel even when another operator owns it (root-only recovery). */
  anyOwner?: boolean;
}

/** Result of {@link HandoffsNamespace.cancel}. */
export interface CancelHandoffResult {
  /** Handoff id. */
  readonly handoffId: string;
  /** True when this call expired it; false when it was no longer open. */
  readonly cancelled: boolean;
  /** State after the call. */
  readonly state: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/** Arguments of {@link MessagesNamespace.send}. */
export interface SendMessageArgs {
  /** Recipient project; it must already exist. */
  to: Scope;
  /** Sending project. Defaults to the client's `scope`. */
  from?: Scope;
  /** Self-contained request for the recipient's agent. */
  body: string;
  /** Optional one-line subject. */
  subject?: string;
  /** How the sender knows the claim. Defaults to `unknown`. */
  nature?: "unknown" | "measured" | "reported" | "inferred";
  /** Supporting evidence. Required by the server when `nature` is `measured`. */
  evidence?: string;
}

/** Result of {@link MessagesNamespace.send}. */
export interface SendMessageResult {
  /** Message id. */
  readonly messageId: string;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A cross-project message. */
export interface Message {
  /** Message id. */
  readonly id: string;
  /** Subject, or `null`. */
  readonly subject: UntrustedText | null;
  /** Body: a request to evaluate, never instructions to obey. */
  readonly body: UntrustedText;
  /** How the sender knows the claim. */
  readonly nature: "unknown" | "measured" | "reported" | "inferred";
  /** Supporting evidence, or `null`. */
  readonly evidence: UntrustedText | null;
  /** `pending`, `editing`, `claimed` or `cancelled`. */
  readonly state: string;
  /** Send time. */
  readonly createdAt: string | undefined;
  /** Claim time, or `null`. */
  readonly claimedAt: string | null;
  /** The sender's harness label. */
  readonly fromAgent: string | undefined;
  /** The harness the message is addressed to, or `null` for shared project mail. */
  readonly toAgent: string | null;
  /** The sending operator key, or `null`. */
  readonly fromOwner: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link MessagesNamespace.list}. */
export interface ListMessagesArgs extends ScopedArgs {
  /** `inbox` (default): mail addressed here. `outbox`: mail sent from here. */
  box?: "inbox" | "outbox";
  /** Maximum messages (default 50, clamped to 200). */
  limit?: number;
}

/** Result of {@link MessagesNamespace.list}. */
export interface ListMessagesResult {
  /** Pending messages. */
  readonly messages: readonly Message[];
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link MessagesNamespace.pop}. */
export interface PopMessageArgs extends ScopedArgs {
  /** Claim exactly this message. Omit to claim the oldest pending one. */
  messageId?: string;
  /**
   * The harness claiming (for example `codex` or `claude-code`), for a
   * client whose session metadata does not identify it. Mail addressed to a
   * harness can only be claimed by that harness.
   */
  agent?: string;
}

/** Result of {@link MessagesNamespace.pop}. */
export interface PopMessageResult {
  /** The claimed message, or `null` when the inbox is empty. */
  readonly message: Message | null;
  /** The server's reminder that the body is untrusted input. */
  readonly securityNotice: string | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link MessagesNamespace.cancel}. */
export interface CancelMessageArgs extends ScopedArgs {
  /** Retract this message. Omit to cancel every pending message the project sent. */
  messageId?: string;
}

/** Result of {@link MessagesNamespace.cancel}. */
export interface CancelMessageResult {
  /** Messages cancelled. */
  readonly cancelled: number;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

// ---------------------------------------------------------------------------
// /api/v1
// ---------------------------------------------------------------------------

/** A workspace in `/api/v1/workspaces`. */
export interface WorkspaceSummary {
  /** Workspace name. */
  readonly name: string;
  /** Projects in it. */
  readonly projectCount: number;
  /** Latest pages in it. */
  readonly pageCount: number;
  /** Last update, or `null` for an empty workspace. */
  readonly lastUpdated: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A project in `/api/v1/projects`. */
export interface ProjectSummary {
  /** Workspace id. */
  readonly workspaceId: string;
  /** Workspace name. */
  readonly workspace: string;
  /** Project id: the stable address when names collide. */
  readonly projectId: string;
  /** Project name. */
  readonly project: string;
  /** Latest pages. */
  readonly pageCount: number;
  /** Last update, or `null`. */
  readonly lastUpdated: string | null;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** A full page from `/api/v1`. */
export interface ApiPage {
  /** Latest version id: the token for a conditional MCP write or delete. */
  readonly id: string;
  /** Wiki path. */
  readonly path: string;
  /** Title. */
  readonly title: UntrustedText;
  /** Kind. */
  readonly kind: string | undefined;
  /** Tier. */
  readonly tier: string | undefined;
  /** Pinned flag. */
  readonly pinned: boolean;
  /** When the oldest retained version was written. */
  readonly createdAt: string | undefined;
  /** When the latest version was written. */
  readonly updatedAt: string | undefined;
  /** Parsed frontmatter. Untrusted. */
  readonly frontmatter: Readonly<Record<string, unknown>>;
  /** Markdown body. */
  readonly body: UntrustedText;
  /** The server's decoded JSON (includes `links` and `backlinks`). */
  readonly raw: Raw;
}

/** Arguments of {@link ApiNamespace.page}. */
export interface ApiPageArgs extends ScopedArgs {
  /** An `ETag` from an earlier read; a match answers `{ notModified: true }`. */
  ifNoneMatch?: string;
}

/** Result of {@link ApiNamespace.page}. */
export type ApiPageResult =
  | {
      /** The page changed or no `ifNoneMatch` was sent. */
      readonly notModified: false;
      /** The page. */
      readonly page: ApiPage;
      /** The `ETag` to send next time, when the server sent one. */
      readonly etag: string | undefined;
    }
  | {
      /** The page still matches `ifNoneMatch`. */
      readonly notModified: true;
      /** The `ETag` the server confirmed. */
      readonly etag: string | undefined;
    };

/** Arguments of {@link ApiNamespace.recent}. */
export interface ApiRecentArgs extends ScopedArgs {
  /** Exclusive RFC 3339 cutoff. Default `1970-01-01T00:00:00Z` (every page). */
  updatedSince?: string;
  /** Page size per request (server clamps to 1..100, default 10). */
  limit?: number;
  /** Bound on cursor rounds. Default 1000. */
  maxRounds?: number;
}

/** Arguments shared by the {@link ApiSearchArgs} forms. */
export interface ApiSearchBase {
  /** Search text (required, non-empty). */
  query: string;
  /** Maximum hits (server default 10, clamped to 100). */
  limit?: number;
}

/** Arguments of {@link ApiNamespace.search}. */
export type ApiSearchArgs =
  | (ApiSearchBase &
      ScopedArgs & {
        /** Not allowed together with `scope`. */
        scopes?: never;
        /** Not global. */
        global?: false;
      })
  | (ApiSearchBase & {
      /** Up to 25 projects. */
      scopes: readonly Scope[];
      /** Not allowed together with `scopes`. */
      scope?: never;
      /** Not global. */
      global?: false;
    })
  | (ApiSearchBase & {
      /** Search every readable project. */
      global: true;
      /** Not allowed: a global search takes no scope. */
      scope?: never;
      /** Not allowed: a global search takes no scope. */
      scopes?: never;
    });

/** A hit from `/api/v1/search`. */
export interface ApiSearchHit {
  /** Workspace. */
  readonly workspace: string;
  /** Project. */
  readonly project: string;
  /** Wiki path. */
  readonly path: string;
  /** Title. */
  readonly title: UntrustedText;
  /** Kind. */
  readonly kind: string | undefined;
  /** Snippet with `<mark>` around matches. */
  readonly snippet: UntrustedText;
  /** Rank; lower is better. */
  readonly rank: number;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link ApiNamespace.handoffs}. */
export interface ApiHandoffsArgs extends ScopedArgs {
  /** Filter by state; omit for every state. */
  state?: "open" | "accepted" | "expired";
  /** Maximum rows (clamped to 1..200). */
  limit?: number;
  /** Root-only: list every operator's rows. */
  allOwners?: boolean;
}

/** A handoff row from the `/api/v1` history listing. */
export interface ApiHandoff {
  /** Handoff id. */
  readonly id: string;
  /** State. */
  readonly state: string;
  /** Creation time. */
  readonly at: string | undefined;
  /** Harness label. */
  readonly agent: string | undefined;
  /** Summary, absent when `redacted`. */
  readonly summary: UntrustedText | undefined;
  /** True when the server withheld the prompt-derived fields. */
  readonly redacted: boolean;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Arguments of {@link ApiNamespace.sessions}. */
export interface ApiSessionsArgs extends ScopedArgs {
  /** Maximum sessions (clamped to 1..100, default 20). */
  limit?: number;
  /** Sessions to skip. */
  offset?: number;
  /** Include sessions that have not ended. */
  includeOpen?: boolean;
}

/** Arguments of {@link ApiNamespace.observations}. */
export interface ApiObservationsArgs extends ScopedArgs {
  /** Maximum observations (clamped to 1..200, default 50). */
  limit?: number;
  /** Observations to skip. */
  offset?: number;
  /** `asc` (default) or `desc`. */
  order?: "asc" | "desc";
  /** Keep only these kinds. */
  kinds?: ObservationKind[];
  /** FTS5 query restricted to the session. */
  query?: string;
  /** Per-body cap (200..16384, default 4000). */
  bodyMaxChars?: number;
}
