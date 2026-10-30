/**
 * Result decoders. Each checks the fields its type promises, raises
 * {@link ProtocolError} when one is missing or mistyped, and keeps the full
 * server JSON in `raw`. Unknown fields pass through in `raw` untouched.
 *
 * @internal
 * @module
 */

import { ProtocolError } from "./errors.js";
import type {
  ActivityWindow,
  AnsweredScope,
  ApiHandoff,
  ApiPage,
  ApiSearchHit,
  Briefing,
  SessionActivity,
  Handoff,
  Message,
  Observation,
  ObservationHit,
  ObservationsResult,
  PageSummary,
  ProcessingJobKind,
  ProcessingJobStatus,
  ProjectSummary,
  QueryHit,
  Raw,
  RelatedPage,
  SessionSummary,
  StatusCounts,
  UntrustedText,
  WorkspaceSummary,
} from "./types.js";

/** Mark stored-memory text with the {@link UntrustedText} brand. */
export function untrusted(text: string): UntrustedText {
  return text as UntrustedText;
}

/** A JSON object, or a {@link ProtocolError} naming where it was expected. */
export function record(value: unknown, where: string): Raw {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ProtocolError(`${where}: expected a JSON object`);
  }
  return value as Raw;
}

/** A JSON array, or a {@link ProtocolError}. */
export function array(value: unknown, where: string): readonly unknown[] {
  if (!Array.isArray(value)) throw new ProtocolError(`${where}: expected a JSON array`);
  return value;
}

/** A required string field. */
export function str(o: Raw, key: string, where: string): string {
  const value = o[key];
  if (typeof value !== "string") {
    throw new ProtocolError(`${where}: field ${key} is missing or not a string`);
  }
  return value;
}

/** An optional string field (`undefined` when absent or null). */
export function optStr(o: Raw, key: string, where: string): string | undefined {
  const value = o[key];
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "string") throw new ProtocolError(`${where}: field ${key} is not a string`);
  return value;
}

/** A string-or-null field; absent counts as null. */
export function nullableStr(o: Raw, key: string, where: string): string | null {
  return optStr(o, key, where) ?? null;
}

/** A required number field. */
export function num(o: Raw, key: string, where: string): number {
  const value = o[key];
  if (typeof value !== "number") {
    throw new ProtocolError(`${where}: field ${key} is missing or not a number`);
  }
  return value;
}

/** An optional number field with a default. */
export function numOr(o: Raw, key: string, fallback: number, where: string): number {
  const value = o[key];
  if (value === undefined || value === null) return fallback;
  if (typeof value !== "number") throw new ProtocolError(`${where}: field ${key} is not a number`);
  return value;
}

/** A required boolean field. */
export function bool(o: Raw, key: string, where: string): boolean {
  const value = o[key];
  if (typeof value !== "boolean") {
    throw new ProtocolError(`${where}: field ${key} is missing or not a boolean`);
  }
  return value;
}

/** An optional boolean, false when absent. */
export function flag(o: Raw, key: string): boolean {
  return o[key] === true;
}

/** An array of strings; absent counts as empty. */
export function strings(o: Raw, key: string, where: string): string[] {
  const value = o[key];
  if (value === undefined || value === null) return [];
  return array(value, `${where}.${key}`).map((item, index) => {
    if (typeof item !== "string") {
      throw new ProtocolError(`${where}: ${key}[${index}] is not a string`);
    }
    return item;
  });
}

/** Decode each element of an array field with `decode`; absent counts as empty. */
export function list<T>(
  o: Raw,
  key: string,
  where: string,
  decode: (value: unknown, where: string) => T,
): T[] {
  const value = o[key];
  if (value === undefined || value === null) return [];
  return array(value, `${where}.${key}`).map((item, index) =>
    decode(item, `${where}.${key}[${index}]`),
  );
}

/** A required array field decoded with `decode`. */
export function requiredList<T>(
  o: Raw,
  key: string,
  where: string,
  decode: (value: unknown, where: string) => T,
): T[] {
  if (!Array.isArray(o[key])) {
    throw new ProtocolError(`${where}: field ${key} is missing or not an array`);
  }
  return list(o, key, where, decode);
}

/** A `PageHit` (single-project, scopes, recent) or a `PageHitWithMeta` (global). */
export function queryHit(value: unknown, where: string): QueryHit {
  const o = record(value, where);
  return {
    id: optStr(o, "id", where),
    path: str(o, "path", where),
    title: untrusted(str(o, "title", where)),
    snippet: untrusted(optStr(o, "snippet", where) ?? ""),
    rank: numOr(o, "rank", 0, where),
    superseded: flag(o, "superseded"),
    pinned: flag(o, "pinned"),
    workspace: optStr(o, "workspace_name", where) ?? optStr(o, "workspace", where),
    project: optStr(o, "project_name", where) ?? optStr(o, "project", where),
    raw: o,
  };
}

/** A raw-observation fallback hit. */
export function observationHit(value: unknown, where: string): ObservationHit {
  const o = record(value, where);
  return {
    id: str(o, "id", where),
    sessionId: str(o, "session_id", where),
    kind: str(o, "kind", where),
    title: untrusted(str(o, "title", where)),
    snippet: untrusted(optStr(o, "snippet", where) ?? ""),
    rank: numOr(o, "rank", 0, where),
    createdAt: str(o, "created_at", where),
    raw: o,
  };
}

/** The `scope` object a tool answers with. */
export function answeredScope(value: unknown, where: string): AnsweredScope {
  const o = record(value, where);
  return {
    workspace: str(o, "workspace", where),
    project: str(o, "project", where),
    resolvedBy: optStr(o, "resolved_by", where) ?? "",
  };
}

/** `StatusCounts`. */
export function statusCounts(value: unknown, where: string): StatusCounts {
  const o = record(value, where);
  return {
    pagesLatest: num(o, "pages_latest", where),
    pagesAll: num(o, "pages_all", where),
    sessions: num(o, "sessions", where),
    observations: num(o, "observations", where),
  };
}

/** A page reference (`PageSummary`). */
export function pageSummary(value: unknown, where: string): PageSummary {
  const o = record(value, where);
  return {
    path: str(o, "path", where),
    title: untrusted(str(o, "title", where)),
    kind: optStr(o, "kind", where),
    tier: optStr(o, "tier", where),
    updatedAt: optStr(o, "updated_at", where),
    raw: o,
  };
}

function activity(value: unknown, where: string): ActivityWindow {
  const o = record(value, where);
  return {
    days: num(o, "days", where),
    sessions: num(o, "sessions", where),
    observations: num(o, "observations", where),
    pagesUpdated: num(o, "pages_updated", where),
  };
}

/** `BriefingSnapshot`, shared by `memory_briefing` and `/api/v1/.../briefing`. */
export function briefing(value: unknown, where: string): Briefing {
  const o = record(value, where);
  return {
    counts: statusCounts(o["counts"], `${where}.counts`),
    activity7d: activity(o["activity_7d"], `${where}.activity_7d`),
    activity30d: activity(o["activity_30d"], `${where}.activity_30d`),
    lastObservationAt: nullableStr(o, "last_observation_at", where),
    pendingHandoffCount: numOr(o, "pending_handoff_count", 0, where),
    rules: list(o, "rules", where, pageSummary),
    slots: list(o, "slots", where, pageSummary),
    recentPages: requiredList(o, "recent_pages", where, pageSummary),
    recentSessions: list(o, "recent_sessions", where, sessionActivity),
    raw: o,
  };
}

/** A `SessionActivity` row: one session's newest captured work. */
export function sessionActivity(value: unknown, where: string): SessionActivity {
  const o = record(value, where);
  return {
    sessionId: str(o, "session_id", where),
    agentKind: str(o, "agent_kind", where),
    cwd: optStr(o, "cwd", where),
    lastActivityAt: str(o, "last_activity_at", where),
    ended: bool(o, "ended", where),
    handedOff: flag(o, "handed_off"),
    current: flag(o, "current"),
    raw: o,
  };
}

/** A related page from `memory_read_page(include_related)`. */
export function relatedPage(value: unknown, where: string): RelatedPage {
  const o = record(value, where);
  return {
    path: str(o, "path", where),
    title: untrusted(optStr(o, "title", where) ?? ""),
    kind: optStr(o, "kind", where),
    workspace: optStr(o, "workspace", where),
    project: optStr(o, "project", where),
    depth: typeof o["depth"] === "number" ? o["depth"] : undefined,
    direction: optStr(o, "direction", where),
    raw: o,
  };
}

/** A handoff row from `memory_handoff_list` / `memory_handoff_accept`. */
export function handoff(value: unknown, where: string): Handoff {
  const o = record(value, where);
  return {
    id: str(o, "id", where),
    summary: untrusted(str(o, "summary", where)),
    openQuestions: strings(o, "open_questions", where).map(untrusted),
    nextSteps: strings(o, "next_steps", where).map(untrusted),
    filesTouched: strings(o, "files_touched", where),
    state: str(o, "state", where),
    createdAt: optStr(o, "created_at", where),
    owner: nullableStr(o, "owner_user", where),
    raw: o,
  };
}

/** A message row. */
export function message(value: unknown, where: string): Message {
  const o = record(value, where);
  const subject = nullableStr(o, "subject", where);
  const evidence = nullableStr(o, "evidence", where);
  const nature = optStr(o, "nature", where) ?? "unknown";
  if (!["unknown", "measured", "reported", "inferred"].includes(nature)) {
    throw new ProtocolError(`${where}.nature must be a supported message nature`);
  }
  return {
    id: str(o, "id", where),
    subject: subject === null ? null : untrusted(subject),
    body: untrusted(str(o, "body", where)),
    nature: nature as Message["nature"],
    evidence: evidence === null ? null : untrusted(evidence),
    state: str(o, "state", where),
    createdAt: optStr(o, "created_at", where),
    claimedAt: nullableStr(o, "claimed_at", where),
    fromAgent: optStr(o, "from_agent", where),
    toAgent: nullableStr(o, "to_agent", where),
    fromOwner: nullableStr(o, "from_owner_user", where),
    raw: o,
  };
}

/** A `SessionSummary`. */
export function sessionSummary(value: unknown, where: string): SessionSummary {
  const o = record(value, where);
  return {
    sessionId: str(o, "session_id", where),
    agentKind: optStr(o, "agent_kind", where),
    cwd: nullableStr(o, "cwd", where),
    startedAt: optStr(o, "started_at", where),
    endedAt: nullableStr(o, "ended_at", where),
    observationCount: numOr(o, "observation_count", 0, where),
    raw: o,
  };
}

/** An `ObservationRecord`. */
export function observation(value: unknown, where: string): Observation {
  const o = record(value, where);
  return {
    id: str(o, "id", where),
    sessionId: str(o, "session_id", where),
    kind: str(o, "kind", where),
    title: untrusted(str(o, "title", where)),
    body: untrusted(str(o, "body", where)),
    importance: numOr(o, "importance", 0, where),
    createdAt: str(o, "created_at", where),
    raw: o,
  };
}

/** One page of session observations (MCP tool and `/api/v1` share the shape). */
export function observationsResult(value: unknown, where: string): ObservationsResult {
  const o = record(value, where);
  return {
    session: sessionSummary(o["session"], `${where}.session`),
    observations: requiredList(o, "observations", where, observation),
    total: num(o, "total", where),
    offset: numOr(o, "offset", 0, where),
    limit: num(o, "limit", where),
    elidedOtherScope: numOr(o, "elided_other_scope", 0, where),
    raw: o,
  };
}

/** A `WorkspaceSummary`. */
export function workspaceSummary(value: unknown, where: string): WorkspaceSummary {
  const o = record(value, where);
  return {
    name: str(o, "workspace_name", where),
    projectCount: numOr(o, "project_count", 0, where),
    pageCount: numOr(o, "page_count", 0, where),
    lastUpdated: nullableStr(o, "last_updated", where),
    raw: o,
  };
}

/** A `ProjectSummary`. */
export function projectSummary(value: unknown, where: string): ProjectSummary {
  const o = record(value, where);
  return {
    workspaceId: str(o, "workspace_id", where),
    workspace: str(o, "workspace_name", where),
    projectId: str(o, "project_id", where),
    project: str(o, "project_name", where),
    pageCount: numOr(o, "page_count", 0, where),
    lastUpdated: nullableStr(o, "last_updated", where),
    raw: o,
  };
}

/** A full `/api/v1` page. */
export function apiPage(value: unknown, where: string): ApiPage {
  const o = record(value, where);
  const frontmatter = o["frontmatter"];
  return {
    id: str(o, "id", where),
    path: str(o, "path", where),
    title: untrusted(str(o, "title", where)),
    kind: optStr(o, "kind", where),
    tier: optStr(o, "tier", where),
    pinned: flag(o, "pinned"),
    createdAt: optStr(o, "created_at", where),
    updatedAt: optStr(o, "updated_at", where),
    frontmatter:
      frontmatter !== null && typeof frontmatter === "object" && !Array.isArray(frontmatter)
        ? (frontmatter as Raw)
        : {},
    body: untrusted(str(o, "body_markdown", where)),
    raw: o,
  };
}

/** An `/api/v1/search` hit. */
export function apiSearchHit(value: unknown, where: string): ApiSearchHit {
  const o = record(value, where);
  return {
    workspace: str(o, "workspace", where),
    project: str(o, "project", where),
    path: str(o, "path", where),
    title: untrusted(str(o, "title", where)),
    kind: optStr(o, "kind", where),
    snippet: untrusted(optStr(o, "snippet", where) ?? ""),
    rank: numOr(o, "rank", 0, where),
    raw: o,
  };
}

/** A handoff row from the `/api/v1` listing. */
export function apiHandoff(value: unknown, where: string): ApiHandoff {
  const o = record(value, where);
  const summary = optStr(o, "summary", where);
  return {
    id: str(o, "id", where),
    state: str(o, "state", where),
    at: optStr(o, "at", where),
    agent: optStr(o, "agent", where),
    summary: summary === undefined ? undefined : untrusted(summary),
    redacted: flag(o, "redacted"),
    raw: o,
  };
}

/** The status of a processing job (`memory_processing_job_start` and `_status`). */
export function processingJobStatus(value: unknown, where: string): ProcessingJobStatus {
  const o = record(value, where);
  return {
    jobId: str(o, "job_id", where),
    sessionId: str(o, "session_id", where),
    kind: str(o, "kind", where) as ProcessingJobKind,
    state: str(o, "state", where),
    chunksTotal: num(o, "chunks_total", where),
    chunksDone: num(o, "chunks_done", where),
    callsUsed: num(o, "calls_used", where),
    maxCalls: num(o, "max_calls", where),
    waitMs: num(o, "wait_ms", where),
    execMs: num(o, "exec_ms", where),
    lastError: nullableStr(o, "last_error", where),
    result: o["result"] ?? null,
    raw: o,
  };
}
