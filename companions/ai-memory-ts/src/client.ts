/**
 * {@link AiMemoryClient} and its namespaces.
 *
 * @module
 */

import { ApiNamespace } from "./api.js";
import {
  decodeIdentity,
  probeApi,
  probeIdentity,
  type Identity,
  type ServerCapabilities,
} from "./capabilities.js";
import { argsObject, Core, requireText, wire } from "./core.js";
import {
  answeredScope,
  array,
  bool,
  briefing,
  handoff,
  list,
  message,
  nullableStr,
  num,
  observationHit,
  observationsResult,
  optStr,
  queryHit,
  record,
  relatedPage,
  requiredList,
  sessionActivity,
  statusCounts,
  str,
  untrusted,
} from "./decode.js";
import { ArgumentError, ProtocolError, ScopeError } from "./errors.js";
import { validateScope, validateScopes, type Scope } from "./scope.js";
import { Transport, type CallOptions, type ConnectionOptions } from "./transport.js";
import type {
  AcceptHandoffArgs,
  AcceptHandoffResult,
  BeginHandoffArgs,
  BeginHandoffResult,
  Briefing,
  BriefingArgs,
  BatchWritePage,
  CancelHandoffArgs,
  CancelHandoffResult,
  CancelMessageArgs,
  CancelMessageResult,
  DeletePageArgs,
  DeletePageResult,
  ExploreArgs,
  ExploreResult,
  FeedbackArgs,
  FeedbackResult,
  ListHandoffsArgs,
  ListHandoffsResult,
  ListMessagesArgs,
  ListMessagesResult,
  ObservationsArgs,
  ObservationsResult,
  Page,
  PageChange,
  PageMetadata,
  PagePatchEdit,
  PatchPageArgs,
  PatchPageResult,
  PopMessageArgs,
  ProjectQueryArgs,
  PopMessageResult,
  QueryArgs,
  QueryResult,
  Raw,
  ReadPageArgs,
  ResolveWriteTargetArgs,
  ResolvedWriteTarget,
  RecentArgs,
  RecentResult,
  RelationName,
  ScopedArgs,
  SendMessageArgs,
  SendMessageResult,
  StatusResult,
  Tier,
  UpdatePageOptions,
  WritePageArgs,
  WritePageResult,
  WritePagesBatchArgs,
  WritePagesBatchResult,
} from "./types.js";

/** Options of {@link AiMemoryClient}. */
export interface ClientOptions extends ConnectionOptions {
  /** Default scope for project calls. A call's own `scope` overrides it. */
  scope?: Scope;
}

/** Options of {@link AiMemoryClient.capabilities}. */
export interface CapabilitiesOptions extends CallOptions {
  /** Probe the server again instead of returning the cached report. */
  refresh?: boolean;
}

/** Options of {@link AiMemoryClient.callTool}. */
export interface CallToolOptions extends CallOptions {
  /**
   * Declare the tool read-only, so a timeout or abort is reported with
   * `uncertain: false`. Default false: an unknown tool may change state.
   */
  readOnly?: boolean;
}

const TIERS: readonly string[] = ["working", "episodic", "semantic", "procedural"];
const RELATIONS: readonly RelationName[] = ["causes", "fixes", "contradicts"];

function scopeWire(scope: Scope): { workspace: string; project: string } {
  return { workspace: scope.workspace, project: scope.project };
}

function existingTargetWire(
  target: unknown,
  where: string,
): { workspace: string; project?: string; project_id?: string } {
  const value = argsObject(target as object, `${where}.target`) as Record<string, unknown>;
  const workspace = requireText(value["workspace"], "target.workspace", where);
  const project = value["project"];
  const projectId = value["projectId"];
  if ((project === undefined) === (projectId === undefined)) {
    throw new ArgumentError(`${where}: target needs exactly one of project or projectId`);
  }
  if (project !== undefined) {
    return { workspace, project: requireText(project, "target.project", where) };
  }
  return { workspace, project_id: requireText(projectId, "target.projectId", where) };
}

/** The metadata a page's frontmatter carries, in write-argument form. */
function metadataFromFrontmatter(frontmatter: Raw): PageMetadata {
  const meta: PageMetadata = {};
  const text = (key: string): string | undefined => {
    const value = frontmatter[key];
    return typeof value === "string" ? value : undefined;
  };
  const texts = (key: string): string[] | undefined => {
    const value = frontmatter[key];
    return Array.isArray(value) && value.every((item) => typeof item === "string")
      ? (value as string[])
      : undefined;
  };
  const title = text("title");
  if (title !== undefined) meta.title = title;
  const tier = text("tier");
  if (tier !== undefined && TIERS.includes(tier)) meta.tier = tier as Tier;
  const tags = texts("tags");
  if (tags !== undefined) meta.tags = tags;
  if (frontmatter["pinned"] === true) meta.pinned = true;
  const kind = text("kind");
  if (kind !== undefined) meta.kind = kind;
  const entities = texts("entities");
  if (entities !== undefined) meta.entities = entities;
  const abstractText = text("abstract");
  if (abstractText !== undefined) meta.abstract = abstractText;
  const expiresAt = text("expires_at");
  if (expiresAt !== undefined) meta.expiresAt = expiresAt;
  const relations = frontmatter["relations"];
  if (relations !== null && typeof relations === "object" && !Array.isArray(relations)) {
    const kept: Partial<Record<RelationName, string[]>> = {};
    for (const name of RELATIONS) {
      const targets = (relations as Record<string, unknown>)[name];
      if (Array.isArray(targets) && targets.every((item) => typeof item === "string")) {
        kept[name] = targets as string[];
      }
    }
    if (Object.keys(kept).length > 0) meta.relations = kept;
  }
  return meta;
}

/**
 * Page tools: write, read, delete and a read-modify-write `update`.
 * Reach it through {@link AiMemoryClient.pages}.
 */
export class PagesNamespace {
  readonly #core: Core;

  /** @internal */
  constructor(core: Core) {
    this.#core = core;
  }

  /**
   * Verify write access to an exact existing destination and return its
   * canonical name and stable id. The operation never creates or promotes a
   * scope. Bulk importers should resolve every destination before writing.
   */
  async resolveWriteTarget(
    args: ResolveWriteTargetArgs,
    options?: CallOptions,
  ): Promise<ResolvedWriteTarget> {
    const where = "pages.resolveWriteTarget";
    const a = argsObject(args, where);
    const decoded = await this.#core.invoke(
      "memory_resolve_write_target",
      existingTargetWire(a.target, where),
      false,
      options,
    );
    const o = record(decoded, "memory_resolve_write_target");
    return {
      workspace: str(o, "workspace", "memory_resolve_write_target"),
      project: str(o, "project", "memory_resolve_write_target"),
      projectId: str(o, "project_id", "memory_resolve_write_target"),
      raw: o,
    };
  }

  /**
   * Atomically write one to 100 complete page versions into one existing
   * project. Every entry must use `createOnly` or `expectedPageId`; one
   * mismatch leaves the entire batch unchanged.
   */
  async writeBatch(
    args: WritePagesBatchArgs,
    options?: CallOptions,
  ): Promise<WritePagesBatchResult> {
    const where = "pages.writeBatch";
    const a = argsObject(args, where);
    const target = existingTargetWire(a.target, where);
    if (!Array.isArray(a.pages) || a.pages.length === 0 || a.pages.length > 100) {
      throw new ArgumentError(`${where}: pages must contain 1-100 entries`);
    }
    const pages = a.pages.map((page: BatchWritePage, index: number) => {
      const itemWhere = `${where}.pages[${index}]`;
      const item = argsObject(page, itemWhere);
      requireText(item.path, "path", itemWhere);
      if (typeof item.body !== "string") {
        throw new ArgumentError(`${itemWhere}: body must be a string`);
      }
      if ((item.expectedPageId === undefined) === (item.createOnly !== true)) {
        throw new ArgumentError(`${itemWhere}: pass exactly one of expectedPageId or createOnly`);
      }
      if (item.expectedPageId !== undefined) {
        requireText(item.expectedPageId, "expectedPageId", itemWhere);
      }
      return wire({
        path: item.path,
        body: item.body,
        title: item.title,
        tier: item.tier,
        tags: item.tags,
        pinned: item.pinned,
        kind: item.kind,
        entities: item.entities,
        abstract: item.abstract,
        relations: item.relations,
        expires_at: item.expiresAt,
        expected_page_id: item.expectedPageId,
        create_only: item.createOnly === true ? true : undefined,
      });
    });
    const decoded = await this.#core.invoke(
      "memory_write_pages_batch",
      { ...target, pages },
      true,
      options,
    );
    const o = record(decoded, "memory_write_pages_batch");
    return {
      pages: requiredList(o, "pages", "memory_write_pages_batch", (value, itemWhere) => {
        const item = record(value, itemWhere);
        return {
          index: num(item, "index", itemWhere),
          path: str(item, "path", itemWhere),
          pageId: str(item, "page_id", itemWhere),
          raw: item,
        };
      }),
      checkpoint: nullableStr(o, "checkpoint", "memory_write_pages_batch"),
      raw: o,
    };
  }

  /**
   * Write (create or replace) one page. A write replaces the whole page:
   * metadata the call omits is cleared. Use {@link PagesNamespace.update} to
   * keep it.
   *
   * Calls `memory_write_page`. May create the scope when it does not exist.
   *
   * @throws {@link PreconditionFailedError} when `expectedPageId` is stale or
   * `createOnly` finds a page; nothing is written.
   * @throws {@link UnsupportedServerError} on a server older than 3.0.0 when a
   * conditional argument is used.
   */
  async write(args: WritePageArgs, options?: CallOptions): Promise<WritePageResult> {
    const where = "pages.write";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.path, "path", where);
    if (typeof a.body !== "string") throw new ArgumentError(`${where}: body must be a string`);
    if (a.expectedPageId !== undefined && a.createOnly === true) {
      throw new ArgumentError(`${where}: expectedPageId and createOnly cannot be combined`);
    }
    if (a.expectedPageId !== undefined) requireText(a.expectedPageId, "expectedPageId", where);
    const decoded = await this.#core.invoke(
      "memory_write_page",
      wire({
        ...scopeWire(scope),
        path: a.path,
        body: a.body,
        title: a.title,
        tier: a.tier,
        tags: a.tags,
        pinned: a.pinned,
        kind: a.kind,
        entities: a.entities,
        abstract: a.abstract,
        relations: a.relations,
        expires_at: a.expiresAt,
        expected_page_id: a.expectedPageId,
        create_only: a.createOnly === true ? true : undefined,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_write_page");
    return {
      pageId: str(o, "page_id", "memory_write_page"),
      path: str(o, "path", "memory_write_page"),
      checkpoint: nullableStr(o, "checkpoint", "memory_write_page"),
      raw: o,
    };
  }

  /**
   * Atomically patch an existing page by Markdown heading while preserving
   * all frontmatter. Every edit is resolved against the same base version.
   */
  async patch(args: PatchPageArgs, options?: CallOptions): Promise<PatchPageResult> {
    const where = "pages.patch";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.path, "path", where);
    requireText(a.expectedPageId, "expectedPageId", where);
    if (!Array.isArray(a.edits) || a.edits.length === 0) {
      throw new ArgumentError(`${where}: edits must be a non-empty array`);
    }
    const edits = a.edits.map((edit: PagePatchEdit, index: number) => {
      const item = argsObject(edit, `${where}.edits[${index}]`);
      requireText(item.op, "op", `${where}.edits[${index}]`);
      if (typeof item.content !== "string") {
        throw new ArgumentError(`${where}.edits[${index}]: content must be a string`);
      }
      if (item.op === "replace_section") {
        requireText(item.anchor, "anchor", `${where}.edits[${index}]`);
        requireText(item.sectionSha256, "sectionSha256", `${where}.edits[${index}]`);
      } else if (item.op === "add_section") {
        requireText(item.anchor, "anchor", `${where}.edits[${index}]`);
      } else if (item.op === "append") {
        if (item.target === "section") requireText(item.anchor, "anchor", `${where}.edits[${index}]`);
        else if (item.target !== "end_of_page") {
          throw new ArgumentError(`${where}.edits[${index}]: unknown append target`);
        }
      } else {
        throw new ArgumentError(`${where}.edits[${index}]: unknown operation`);
      }
      return wire({
        op: item.op,
        target: "target" in item ? item.target : undefined,
        anchor: item.anchor,
        section_sha256: "sectionSha256" in item ? item.sectionSha256 : undefined,
        content: item.content,
      });
    });
    const decoded = await this.#core.invoke(
      "memory_patch_page",
      wire({
        ...scopeWire(scope),
        path: a.path,
        expected_page_id: a.expectedPageId,
        edits,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_patch_page");
    return {
      pageId: str(o, "page_id", "memory_patch_page"),
      path: str(o, "path", "memory_patch_page"),
      checkpoint: nullableStr(o, "checkpoint", "memory_patch_page"),
      raw: o,
    };
  }

  /**
   * Read one page by exact `path`, or the top hit of a `query`.
   *
   * Calls `memory_read_page`. Reads never create a scope: a missing
   * workspace or project is an {@link InvalidParamsError}; a missing page is
   * an {@link RpcError}.
   */
  async read(args: ReadPageArgs, options?: CallOptions): Promise<Page> {
    const where = "pages.read";
    const a = argsObject(args, where);
    if (a.global === true && a.scope !== undefined) {
      throw new ArgumentError(`${where}: global cannot be combined with scope`);
    }
    const scope = a.global === true ? undefined : this.#core.scope(a.scope, where);
    const hasPath = a.path !== undefined;
    const hasQuery = a.query !== undefined;
    if (hasPath === hasQuery) {
      throw new ArgumentError(`${where}: pass exactly one of path or query`);
    }
    if (hasPath) requireText(a.path, "path", where);
    else requireText(a.query, "query", where);
    const decoded = await this.#core.invoke(
      "memory_read_page",
      wire({
        ...(scope === undefined ? { scope: "global" } : scopeWire(scope)),
        path: a.path,
        query: a.query,
        include_related: a.includeRelated === true ? true : undefined,
        related_depth: a.relatedDepth,
        max_chars: a.maxChars,
      }),
      false,
      options,
    );
    const o = record(decoded, "memory_read_page");
    const frontmatter = o["frontmatter"];
    const sections = requiredList(o, "sections", "memory_read_page", (value, where) => {
      const section = record(value, where);
      return {
        anchor: str(section, "anchor", where),
        level: num(section, "level", where),
        sha256: str(section, "sha256", where),
      };
    });
    return {
      pageId: str(o, "page_id", "memory_read_page"),
      path: str(o, "path", "memory_read_page"),
      title: untrusted(str(o, "title", "memory_read_page")),
      body: untrusted(str(o, "body", "memory_read_page")),
      truncated: typeof o["truncated"] === "boolean" ? o["truncated"] : undefined,
      totalChars: typeof o["total_chars"] === "number" ? o["total_chars"] : undefined,
      frontmatter:
        frontmatter !== null && typeof frontmatter === "object" && !Array.isArray(frontmatter)
          ? (frontmatter as Raw)
          : {},
      sections,
      sectionsTruncated: bool(o, "sections_truncated", "memory_read_page"),
      related: Array.isArray(o["related"])
        ? list(o, "related", "memory_read_page", relatedPage)
        : undefined,
      raw: o,
    };
  }

  /**
   * Delete one page (every stored version). Idempotent. The wiki is
   * checkpointed first, so `AdminClient.restorePage` can bring it back.
   *
   * Calls `memory_delete_page`.
   *
   * @throws {@link PreconditionFailedError} when `expectedPageId` is stale.
   */
  async delete(args: DeletePageArgs, options?: CallOptions): Promise<DeletePageResult> {
    const where = "pages.delete";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.path, "path", where);
    if (a.expectedPageId !== undefined) requireText(a.expectedPageId, "expectedPageId", where);
    const decoded = await this.#core.invoke(
      "memory_delete_page",
      wire({ ...scopeWire(scope), path: a.path, expected_page_id: a.expectedPageId }),
      true,
      options,
    );
    const o = record(decoded, "memory_delete_page");
    return {
      path: str(o, "path", "memory_delete_page"),
      deleted: bool(o, "deleted", "memory_delete_page"),
      preCheckpoint: nullableStr(o, "pre_checkpoint", "memory_delete_page"),
      checkpoint: nullableStr(o, "checkpoint", "memory_delete_page"),
      raw: o,
    };
  }

  /**
   * Read a page, apply `change`, and write it back only if nobody changed it
   * in between (`expectedPageId`). The page's current metadata (title, tier,
   * tags, pinned, kind, entities, abstract, relations, TTL) is carried over
   * unless `change` sets it; `null` clears a field.
   *
   * One attempt, no retry: on a concurrent change it throws
   * {@link PreconditionFailedError} and writes nothing, so the caller can
   * re-run it. The page must exist; create new pages with
   * `write({ createOnly: true })`.
   *
   * Calls `memory_read_page`, then `memory_write_page`.
   */
  async update(
    path: string,
    change: (current: Page) => PageChange | Promise<PageChange>,
    options: UpdatePageOptions & CallOptions = {},
  ): Promise<WritePageResult> {
    const where = "pages.update";
    requireText(path, "path", where);
    if (typeof change !== "function") throw new ArgumentError(`${where}: change must be a function`);
    const { scope: callScope, signal, timeoutMs } = argsObject(options, where);
    const scope = this.#core.scope(callScope, where);
    const callOptions: CallOptions = {};
    if (signal !== undefined) callOptions.signal = signal;
    if (timeoutMs !== undefined) callOptions.timeoutMs = timeoutMs;
    const current = await this.read({ path, scope }, callOptions);
    const delta = await change(current);
    if (delta === null || typeof delta !== "object" || Array.isArray(delta)) {
      throw new ArgumentError(`${where}: change must return an object`);
    }
    const merged: Record<string, unknown> = { ...metadataFromFrontmatter(current.frontmatter) };
    for (const [key, value] of Object.entries(delta)) {
      if (key === "body") continue;
      if (value === null) delete merged[key];
      else if (value !== undefined) merged[key] = value;
    }
    const body = delta.body ?? current.body;
    return this.write(
      { ...(merged as PageMetadata), path: current.path, body, expectedPageId: current.pageId, scope },
      callOptions,
    );
  }
}

/**
 * Handoffs pass context between runs: one run begins a handoff, a later run
 * claims it. A handoff is claimed exactly once. Reach it through
 * {@link AiMemoryClient.handoffs}.
 */
export class HandoffsNamespace {
  readonly #core: Core;

  /** @internal */
  constructor(core: Core) {
    this.#core = core;
  }

  /**
   * Leave a handoff for the next run. It belongs to this operator unless
   * `shared` is set. Calls `memory_handoff_begin`; may create the scope.
   */
  async begin(args: BeginHandoffArgs, options?: CallOptions): Promise<BeginHandoffResult> {
    const where = "handoffs.begin";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.summary, "summary", where);
    const decoded = await this.#core.invoke(
      "memory_handoff_begin",
      wire({
        ...scopeWire(scope),
        summary: a.summary,
        open_questions: a.openQuestions,
        next_steps: a.nextSteps,
        files_touched: a.filesTouched,
        shared: a.shared === true ? true : undefined,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_handoff_begin");
    return { handoffId: str(o, "handoff_id", "memory_handoff_begin"), raw: o };
  }

  /** List open handoffs without claiming any. Calls `memory_handoff_list`. */
  async list(args?: ListHandoffsArgs, options?: CallOptions): Promise<ListHandoffsResult> {
    const where = "handoffs.list";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const decoded = await this.#core.invoke(
      "memory_handoff_list",
      wire({
        ...scopeWire(scope),
        any_owner: a.anyOwner === true ? true : undefined,
        limit: a.limit,
      }),
      false,
      options,
    );
    const o = record(decoded, "memory_handoff_list");
    const newer = o["newer_unended_session"];
    return {
      handoffs: requiredList(o, "handoffs", "memory_handoff_list", handoff),
      newerUnendedSession:
        newer === undefined || newer === null
          ? undefined
          : sessionActivity(newer, "memory_handoff_list.newer_unended_session"),
      raw: o,
    };
  }

  /**
   * Claim a handoff: the one named by `handoffId`, or the latest eligible
   * one. Calls `memory_handoff_accept`.
   *
   * Do not blindly retry after an `uncertain` timeout: the first call may
   * have claimed it, and a retry returns `none_pending` or the next handoff.
   */
  async accept(args?: AcceptHandoffArgs, options?: CallOptions): Promise<AcceptHandoffResult> {
    const where = "handoffs.accept";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    if (a.handoffId !== undefined) requireText(a.handoffId, "handoffId", where);
    const decoded = await this.#core.invoke(
      "memory_handoff_accept",
      wire({
        ...scopeWire(scope),
        handoff_id: a.handoffId,
        any_owner: a.anyOwner === true ? true : undefined,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_handoff_accept");
    const status = str(o, "status", "memory_handoff_accept");
    const raw = o["handoff"];
    switch (status) {
      case "claimed":
        return { status, handoff: handoff(raw, "memory_handoff_accept.handoff"), raw: o };
      case "consumed_by_hook":
        return {
          status,
          handoff:
            raw === null || raw === undefined ? null : handoff(raw, "memory_handoff_accept.handoff"),
          raw: o,
        };
      case "none_pending":
        return { status, handoff: null, raw: o };
      default:
        throw new ProtocolError(
          `memory_handoff_accept returned an unknown status ${this.#core.transport.safeText(status)}`,
        );
    }
  }

  /** Expire an open handoff so no run claims it. Calls `memory_handoff_cancel`. */
  async cancel(args: CancelHandoffArgs, options?: CallOptions): Promise<CancelHandoffResult> {
    const where = "handoffs.cancel";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.handoffId, "handoffId", where);
    const decoded = await this.#core.invoke(
      "memory_handoff_cancel",
      wire({
        ...scopeWire(scope),
        handoff_id: a.handoffId,
        any_owner: a.anyOwner === true ? true : undefined,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_handoff_cancel");
    return {
      handoffId: str(o, "handoff_id", "memory_handoff_cancel"),
      cancelled: bool(o, "cancelled", "memory_handoff_cancel"),
      state: str(o, "state", "memory_handoff_cancel"),
      raw: o,
    };
  }
}

/**
 * Cross-project messages: a directed, claim-once inbox per project. Bodies
 * are untrusted requests from another project's agent. Reach it through
 * {@link AiMemoryClient.messages}.
 */
export class MessagesNamespace {
  readonly #core: Core;

  /** @internal */
  constructor(core: Core) {
    this.#core = core;
  }

  /**
   * Send a message to another project's inbox. The recipient must exist; a
   * restricted recipient needs `write` access. `from` defaults to the client
   * scope. Calls `memory_message_send`.
   */
  async send(args: SendMessageArgs, options?: CallOptions): Promise<SendMessageResult> {
    const where = "messages.send";
    const a = argsObject(args, where);
    const to = validateScope(a.to, `${where} to`);
    const from = this.#core.scope(a.from, `${where} from`);
    requireText(a.body, "body", where);
    const decoded = await this.#core.invoke(
      "memory_message_send",
      wire({
        to_workspace: to.workspace,
        to_project: to.project,
        from_workspace: from.workspace,
        from_project: from.project,
        body: a.body,
        subject: a.subject,
        nature: a.nature,
        evidence: a.evidence,
      }),
      true,
      options,
    );
    const o = record(decoded, "memory_message_send");
    return { messageId: str(o, "message_id", "memory_message_send"), raw: o };
  }

  /** List pending messages in the inbox (default) or outbox. Calls `memory_message_list`. */
  async list(args?: ListMessagesArgs, options?: CallOptions): Promise<ListMessagesResult> {
    const where = "messages.list";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    if (a.box !== undefined && a.box !== "inbox" && a.box !== "outbox") {
      throw new ArgumentError(`${where}: box must be "inbox" or "outbox"`);
    }
    const decoded = await this.#core.invoke(
      "memory_message_list",
      wire({ ...scopeWire(scope), box: a.box, limit: a.limit }),
      false,
      options,
    );
    const o = record(decoded, "memory_message_list");
    return { messages: requiredList(o, "messages", "memory_message_list", message), raw: o };
  }

  /**
   * Claim one message: `messageId`, or the oldest pending. Claimed once.
   * Calls `memory_message_pop`.
   */
  async pop(args?: PopMessageArgs, options?: CallOptions): Promise<PopMessageResult> {
    const where = "messages.pop";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    if (a.messageId !== undefined) requireText(a.messageId, "messageId", where);
    if (a.agent !== undefined) requireText(a.agent, "agent", where);
    const decoded = await this.#core.invoke(
      "memory_message_pop",
      wire({ ...scopeWire(scope), message_id: a.messageId, agent: a.agent }),
      true,
      options,
    );
    const o = record(decoded, "memory_message_pop");
    const raw = o["message"];
    return {
      message: raw === null || raw === undefined ? null : message(raw, "memory_message_pop.message"),
      securityNotice: optStr(o, "security_notice", "memory_message_pop"),
      raw: o,
    };
  }

  /**
   * Retract `messageId`, or every pending message this project sent.
   * Calls `memory_message_cancel`.
   */
  async cancel(args?: CancelMessageArgs, options?: CallOptions): Promise<CancelMessageResult> {
    const where = "messages.cancel";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    if (a.messageId !== undefined) requireText(a.messageId, "messageId", where);
    const decoded = await this.#core.invoke(
      "memory_message_cancel",
      wire({ ...scopeWire(scope), message_id: a.messageId }),
      true,
      options,
    );
    const o = record(decoded, "memory_message_cancel");
    return { cancelled: num(o, "cancelled", "memory_message_cancel"), raw: o };
  }
}

/**
 * Typed client for an ai-memory server.
 *
 * ```ts
 * const memory = new AiMemoryClient({
 *   serverUrl: "http://127.0.0.1:49374",
 *   token: process.env.AI_MEMORY_AUTH_TOKEN,
 *   scope: { workspace: "demo", project: "app" },
 * });
 * await memory.pages.write({ path: "notes/retries.md", body: "# Retry policy\n..." });
 * const { hits } = await memory.query({ query: "retry policy" });
 * ```
 *
 * The client is a static MCP client: every project call carries an explicit
 * workspace and project. It sends no `cwd`, never relies on the server's
 * active project, makes no automatic retries and follows no redirects. The
 * first wrapped call probes the server once (`initialize` and `tools/list`)
 * and refuses calls the server cannot honour with
 * {@link UnsupportedServerError}.
 */
export class AiMemoryClient {
  readonly #core: Core;
  #capabilities: ServerCapabilities | undefined;

  /** Page tools. */
  readonly pages: PagesNamespace;
  /** Handoff tools. */
  readonly handoffs: HandoffsNamespace;
  /** Cross-project message tools. */
  readonly messages: MessagesNamespace;
  /** Read-only `/api/v1` routes (the server needs `--enable-api`). */
  readonly api: ApiNamespace;

  /**
   * @throws {@link ConfigError} for an unusable URL, token or timeout.
   * @throws {@link ScopeError} for an invalid default scope.
   */
  constructor(options: ClientOptions) {
    const transport = new Transport(options);
    const scope = options.scope === undefined ? undefined : validateScope(options.scope, "scope");
    this.#core = new Core(transport, scope);
    this.pages = new PagesNamespace(this.#core);
    this.handoffs = new HandoffsNamespace(this.#core);
    this.messages = new MessagesNamespace(this.#core);
    this.api = new ApiNamespace(this.#core);
  }

  /** The server URL the client talks to, without a trailing slash. */
  get serverUrl(): string {
    return this.#core.transport.serverUrl;
  }

  /** The default scope, if one was configured. */
  get scope(): Scope | undefined {
    return this.#core.defaultScope;
  }

  /**
   * Probe the server: `initialize`, `tools/list`, `GET /identity` and
   * whether `/api/v1` is mounted. Cached per client; pass `refresh: true`
   * to probe again.
   */
  async capabilities(options: CapabilitiesOptions = {}): Promise<ServerCapabilities> {
    const { refresh, ...callOptions } = options;
    if (refresh === true) {
      this.#core.forgetToolIndex();
      this.#capabilities = undefined;
    }
    if (this.#capabilities !== undefined) return this.#capabilities;
    const index = await this.#core.toolIndex(callOptions);
    const identity = await probeIdentity(this.#core.transport, callOptions);
    const apiMounted = await probeApi(this.#core.transport, callOptions);
    const write = index.tools["memory_write_page"];
    this.#capabilities = {
      server: index.server,
      protocolVersion: index.protocolVersion,
      statefulSession: index.statefulSession,
      tools: index.tools,
      identity,
      apiMounted,
      meetsMinimumVersion: write?.properties.includes("expected_page_id") === true,
    };
    return this.#capabilities;
  }

  /** Who the server thinks the caller is. Calls `GET /identity` (server 2.6.0+). */
  async identity(options?: CallOptions): Promise<Identity> {
    const body = await this.#core.transport.json(
      "GET",
      "/identity",
      { route: "identity", mutating: false },
      options,
    );
    return decodeIdentity(this.#core.transport, body);
  }

  /** Lifetime counts for one project. Calls `memory_status`. */
  async status(args?: ScopedArgs, options?: CallOptions): Promise<StatusResult> {
    const where = "status";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const decoded = await this.#core.invoke("memory_status", scopeWire(scope), false, options);
    const o = record(decoded, "memory_status");
    return {
      counts: statusCounts(o["counts"], "memory_status.counts"),
      scope: answeredScope(o["scope"], "memory_status.scope"),
      raw: o,
    };
  }

  /**
   * Search memory: one project (the call's `scope` or the default), several
   * (`scopes`), or every readable project (`global: true`, which takes no
   * scope). Calls `memory_query`.
   *
   * @throws {@link ScopeError} for a scope combined with `global`, or
   * `scope` combined with `scopes`.
   */
  async query(args: QueryArgs, options?: CallOptions): Promise<QueryResult> {
    const where = "query";
    const a = argsObject(args, where) as QueryArgs & Record<string, unknown>;
    requireText(a.query, "query", where);
    const common = {
      query: a.query,
      limit: a.limit,
      include_expired: a.includeExpired === true ? true : undefined,
      explain: a.explain === true ? true : undefined,
      kind: a.kind,
      tier: a.tier,
      path_prefix: a.pathPrefix,
      tag: a.tag,
    };
    let wireArgs: Record<string, unknown>;
    const isGlobal = a.global === true;
    if (isGlobal) {
      if (a.scope !== undefined || a.scopes !== undefined) {
        throw new ScopeError(`${where}: global: true takes no scope or scopes`);
      }
      wireArgs = wire({ ...common, global: true });
    } else {
      const extra = {
        include_superseded: a.includeSuperseded === true ? true : undefined,
        answer: a.answer === true ? true : undefined,
        reasoning: a.reasoning,
      };
      if (a.scopes !== undefined) {
        if (a.scope !== undefined) {
          throw new ScopeError(`${where}: pass scope or scopes, not both`);
        }
        wireArgs = wire({ ...common, ...extra, scopes: validateScopes(a.scopes, where) });
      } else {
        const project = a as ProjectQueryArgs;
        wireArgs = wire({
          ...scopeWire(this.#core.scope(a.scope, where)),
          ...common,
          ...extra,
          pin_first: project.pinFirst === true ? true : undefined,
          as_of: project.asOf,
        });
      }
    }
    const decoded = await this.#core.invoke("memory_query", wireArgs, false, options);
    const o = record(decoded, "memory_query");
    const answer = o["answer"];
    let decodedAnswer: QueryResult["answer"];
    if (answer !== undefined && answer !== null) {
      const ar = record(answer, "memory_query.answer");
      decodedAnswer = {
        text: untrusted(str(ar, "text", "memory_query.answer")),
        citations: array(ar["citations"] ?? [], "memory_query.answer.citations").filter(
          (item): item is string => typeof item === "string",
        ),
      };
    }
    const context = o["answer_context"];
    let decodedContext: QueryResult["answerContext"];
    if (context !== undefined && context !== null) {
      const c = record(context, "memory_query.answer_context");
      decodedContext = {
        admitted: num(c, "admitted", "memory_query.answer_context"),
        omitted: num(c, "omitted", "memory_query.answer_context"),
        clipped: num(c, "clipped", "memory_query.answer_context"),
        estimatedInputTokens: num(c, "estimated_input_tokens", "memory_query.answer_context"),
        reservedOutputTokens: num(c, "reserved_output_tokens", "memory_query.answer_context"),
        maxContextTokens: num(c, "max_context_tokens", "memory_query.answer_context"),
      };
    }
    return {
      hits: isGlobal
        ? list(o, "global_hits", "memory_query", queryHit)
        : requiredList(o, "hits", "memory_query", queryHit),
      globalScopeHits: list(o, "global_scope_hits", "memory_query", queryHit),
      rawHits: list(o, "raw_hits", "memory_query", observationHit),
      answer: decodedAnswer,
      ...(decodedContext === undefined ? {} : { answerContext: decodedContext }),
      answerUnavailable: optStr(o, "answer_unavailable", "memory_query"),
      raw: o,
    };
  }

  /** The most recently updated pages of one project. Calls `memory_recent`. */
  async recent(args?: RecentArgs, options?: CallOptions): Promise<RecentResult> {
    const where = "recent";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const decoded = await this.#core.invoke(
      "memory_recent",
      wire({ ...scopeWire(scope), limit: a.limit }),
      false,
      options,
    );
    const o = record(decoded, "memory_recent");
    return { hits: requiredList(o, "hits", "memory_recent", queryHit), raw: o };
  }

  /**
   * A structured snapshot of one project: counts, activity, rules, slots,
   * recent pages. No LLM call. Calls `memory_briefing`.
   */
  async briefing(args?: BriefingArgs, options?: CallOptions): Promise<Briefing> {
    const where = "briefing";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const decoded = await this.#core.invoke(
      "memory_briefing",
      wire({
        ...scopeWire(scope),
        recent_pages_limit: a.recentPagesLimit,
        settled_first: a.settledFirst === true ? true : undefined,
      }),
      false,
      options,
    );
    return briefing(decoded, "memory_briefing");
  }

  /**
   * A prose digest of one project when the server has an LLM provider,
   * otherwise the structured briefing with `prose: null`. Calls
   * `memory_explore`.
   */
  async explore(args?: ExploreArgs, options?: CallOptions): Promise<ExploreResult> {
    const where = "explore";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const decoded = await this.#core.invoke(
      "memory_explore",
      wire({
        ...scopeWire(scope),
        focus: a.focus,
        recent_pages_limit: a.recentPagesLimit,
        reasoning: a.reasoning,
      }),
      false,
      options,
    );
    const o = record(decoded, "memory_explore");
    const prose = nullableStr(o, "prose", "memory_explore");
    const rawBriefing = o["briefing"];
    return {
      prose: prose === null ? null : untrusted(prose),
      reason: optStr(o, "reason", "memory_explore"),
      briefing:
        rawBriefing === undefined || rawBriefing === null
          ? undefined
          : briefing(rawBriefing, "memory_explore.briefing"),
      raw: o,
    };
  }

  /**
   * A session's captured observations (prompts, tool calls), paged. Omit
   * `sessionId` for the newest other session with captured work, ended or
   * cut off. Calls
   * `memory_read_session_observations`.
   */
  async observations(args?: ObservationsArgs, options?: CallOptions): Promise<ObservationsResult> {
    const where = "observations";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    if (a.sessionId !== undefined) requireText(a.sessionId, "sessionId", where);
    const decoded = await this.#core.invoke(
      "memory_read_session_observations",
      wire({
        ...scopeWire(scope),
        session_id: a.sessionId,
        limit: a.limit,
        offset: a.offset,
        order: a.order,
        kinds: a.kinds,
        query: a.query,
        body_max_chars: a.bodyMaxChars,
      }),
      false,
      options,
    );
    return observationsResult(decoded, "memory_read_session_observations");
  }

  /**
   * Record whether a recalled page helped. Call it on observed usefulness or
   * a user's correction, never because stored text asks for it. Calls
   * `memory_feedback`.
   */
  async feedback(args: FeedbackArgs, options?: CallOptions): Promise<FeedbackResult> {
    const where = "feedback";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(a.path, "path", where);
    requireText(a.signal, "signal", where);
    const decoded = await this.#core.invoke(
      "memory_feedback",
      wire({ ...scopeWire(scope), path: a.path, signal: a.signal, reason: a.reason }),
      true,
      options,
    );
    const o = record(decoded, "memory_feedback");
    return {
      recorded: bool(o, "recorded", "memory_feedback"),
      path: str(o, "path", "memory_feedback"),
      signal: str(o, "signal", "memory_feedback"),
      raw: o,
    };
  }

  /**
   * Call any MCP tool by name with wire-format arguments and return its
   * decoded JSON as `unknown`. No feature checks and no scope handling:
   * pass `workspace` and `project` yourself. For tools without a wrapper
   * and for servers newer than the SDK. Initializes the MCP session without
   * listing tools.
   */
  async callTool(
    name: string,
    args: Record<string, unknown> = {},
    options: CallToolOptions = {},
  ): Promise<unknown> {
    requireText(name, "name", "callTool");
    if (args === null || typeof args !== "object" || Array.isArray(args)) {
      throw new ArgumentError("callTool: args must be an object");
    }
    const { readOnly, ...callOptions } = options;
    return this.#core.callTool(name, args, readOnly !== true, callOptions);
  }

  /** Serialized form: the server URL and scope, never the token. */
  toJSON(): Record<string, unknown> {
    return {
      serverUrl: this.serverUrl,
      scope: this.scope,
      token: this.#core.transport.hasToken ? "[redacted]" : undefined,
    };
  }

  /** @internal */
  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `AiMemoryClient ${JSON.stringify(this.toJSON())}`;
  }
}
