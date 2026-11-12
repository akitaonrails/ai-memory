/**
 * The read-only `/api/v1` routes. They need a server started with
 * `--enable-api` (or `--enable-web`); without it every route is a
 * {@link NotFoundError}. Limits clamp server-side to `1..=100` (`1..=200` for
 * handoff history and observations); the SDK passes the caller's value through.
 *
 * @module
 */

import { MINIMUM_SERVER_VERSION } from "./capabilities.js";
import { argsObject, type Core, requireText } from "./core.js";
import {
  apiHandoff,
  apiPage,
  apiSearchHit,
  array,
  briefing,
  observationsResult,
  pageSummary,
  projectSummary,
  record,
  requiredList,
  sessionSummary,
  workspaceSummary,
} from "./decode.js";
import { ArgumentError, ProtocolError, ScopeError, UnsupportedServerError } from "./errors.js";
import { validateScopes, type Scope } from "./scope.js";
import type { CallOptions } from "./transport.js";
import type {
  ApiHandoff,
  ApiHandoffsArgs,
  ApiObservationsArgs,
  ApiPageArgs,
  ApiPageResult,
  ApiRecentArgs,
  ApiSearchArgs,
  ApiSearchHit,
  ApiSessionsArgs,
  Briefing,
  ObservationsResult,
  PageSummary,
  ProjectSummary,
  ScopedArgs,
  SessionSummary,
  WorkspaceSummary,
} from "./types.js";

/** Exclusive cutoff that lists every page. */
const EPOCH = "1970-01-01T00:00:00Z";
/** Bound on cursor rounds so a hostile server cannot loop the client. */
const DEFAULT_MAX_ROUNDS = 1_000;

/**
 * One URL path segment. Dot segments are refused: the URL parser would
 * resolve them and send the request to a different route. A literal percent
 * sign is escaped before the URL parser sees it.
 */
function segment(value: string, name: string, where: string): string {
  if (value === "" || value === "." || value === "..") {
    throw new ArgumentError(`${where}: ${name} must not be empty or a dot segment`);
  }
  return encodeURIComponent(value);
}

function projectPath(scope: Scope, where: string): string {
  return (
    `/api/v1/workspaces/${segment(scope.workspace, "workspace", where)}` +
    `/projects/${segment(scope.project, "project", where)}`
  );
}

function numberParam(value: number | undefined): string | undefined {
  return value === undefined ? undefined : String(value);
}

/**
 * Read-only `/api/v1` routes. Reach it through {@link AiMemoryClient.api}.
 */
export class ApiNamespace {
  readonly #core: Core;

  /** @internal */
  constructor(core: Core) {
    this.#core = core;
  }

  async #get(path: string, query: Record<string, string | undefined>, options?: CallOptions) {
    return this.#core.transport.json("GET", path, { query, route: "api", mutating: false }, options);
  }

  /** Every workspace the caller can see. `GET /api/v1/workspaces`. */
  async workspaces(options?: CallOptions): Promise<WorkspaceSummary[]> {
    const body = await this.#get("/api/v1/workspaces", {}, options);
    return array(body, "/api/v1/workspaces").map((item, index) =>
      workspaceSummary(item, `/api/v1/workspaces[${index}]`),
    );
  }

  /** Projects, optionally in one workspace. `GET /api/v1/projects`. */
  async projects(
    args: { workspace?: string } = {},
    options?: CallOptions,
  ): Promise<ProjectSummary[]> {
    const a = argsObject(args, "api.projects");
    if (a.workspace !== undefined) requireText(a.workspace, "workspace", "api.projects");
    const body = await this.#get("/api/v1/projects", { workspace: a.workspace }, options);
    return array(body, "/api/v1/projects").map((item, index) =>
      projectSummary(item, `/api/v1/projects[${index}]`),
    );
  }

  /** Latest pages of one project. `GET /api/v1/workspaces/{w}/projects/{p}/pages`. */
  async pages(args?: ScopedArgs, options?: CallOptions): Promise<PageSummary[]> {
    const where = "api.pages";
    const scope = this.#core.scope(argsObject(args, where).scope, where);
    const path = `${projectPath(scope, where)}/pages`;
    const body = await this.#get(path, {}, options);
    return array(body, path).map((item, index) => pageSummary(item, `${path}[${index}]`));
  }

  /**
   * One full page, with its `ETag`. Pass a previous `ETag` as `ifNoneMatch`
   * to get `{ notModified: true }` when it has not changed. The page's `id`
   * is the version token for a conditional write.
   * `GET /api/v1/workspaces/{w}/projects/{p}/pages/{path}`.
   */
  async page(path: string, args?: ApiPageArgs, options?: CallOptions): Promise<ApiPageResult> {
    const where = "api.page";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(path, "path", where);
    const route = `${projectPath(scope, where)}/pages/${path
      .split("/")
      .map((part) => segment(part, "path segment", where))
      .join("/")}`;
    const headers: Record<string, string> = {};
    if (a.ifNoneMatch !== undefined) {
      requireText(a.ifNoneMatch, "ifNoneMatch", where);
      if (!/^[\x20-\x7e]+$/.test(a.ifNoneMatch)) {
        throw new ArgumentError(`${where}: ifNoneMatch must be printable ASCII`);
      }
      headers["If-None-Match"] = a.ifNoneMatch;
    }
    const transport = this.#core.transport;
    const response = await transport.request(
      "GET",
      route,
      { headers, route: "api", mutating: false },
      options,
    );
    const etag = response.headers.get("etag") ?? undefined;
    if (response.status === 304) return { notModified: true, etag };
    if (response.status < 200 || response.status >= 300) {
      throw transport.httpError(route, response, "api");
    }
    let body: unknown;
    try {
      body = JSON.parse(response.text);
    } catch {
      throw new ProtocolError(`${route} answered with invalid JSON`, transport.safeText(response.text));
    }
    return { notModified: false, page: apiPage(body, route), etag };
  }

  /**
   * Pages updated after `updatedSince`, oldest first, following
   * `next_cursor` until the server says there are no more. The iterator
   * stops with {@link ProtocolError} after `maxRounds` requests. Superseded,
   * expired and deleted pages are not listed.
   * `GET /api/v1/workspaces/{w}/projects/{p}/recent?updated_since=...`.
   *
   * Scope and arguments are checked when this is called, before any request.
   */
  recent(args?: ApiRecentArgs, options?: CallOptions): AsyncIterableIterator<PageSummary> {
    const where = "api.recent";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const since = a.updatedSince ?? EPOCH;
    requireText(since, "updatedSince", where);
    const maxRounds = a.maxRounds ?? DEFAULT_MAX_ROUNDS;
    if (!Number.isInteger(maxRounds) || maxRounds < 1) {
      throw new ArgumentError(`${where}: maxRounds must be a positive integer`);
    }
    const path = `${projectPath(scope, where)}/recent`;
    const limit = numberParam(a.limit);
    const get = this.#get.bind(this);
    return (async function* (): AsyncGenerator<PageSummary> {
      let cursor: string | undefined;
      for (let round = 0; ; round += 1) {
        if (round >= maxRounds) {
          throw new ProtocolError(`${path}: pagination did not finish within ${maxRounds} rounds`);
        }
        const query =
          cursor === undefined ? { updated_since: since, limit } : { cursor, limit };
        const body = await get(path, query, options);
        if (Array.isArray(body)) {
          throw new UnsupportedServerError(
            "incremental /api/v1 recent",
            MINIMUM_SERVER_VERSION,
          );
        }
        const o = record(body, path);
        for (const page of requiredList(o, "pages", path, pageSummary)) yield page;
        const next = o["next_cursor"];
        if (next === null || next === undefined) return;
        // Server cursors are URL-safe base64 of at most 8192 characters.
        if (typeof next !== "string" || !/^[\x21-\x7e]{1,8192}$/.test(next)) {
          throw new ProtocolError(`${path}: next_cursor is malformed`);
        }
        cursor = next;
      }
    })();
  }

  /**
   * Search pages. One project (the call's `scope` or the default) and
   * `scopes` use the same hybrid ranker as `query`; `global: true` is the
   * cross-project FTS5 search. `GET` or `POST /api/v1/search`.
   */
  async search(args: ApiSearchArgs, options?: CallOptions): Promise<ApiSearchHit[]> {
    const where = "api.search";
    const a = argsObject(args, where) as ApiSearchArgs & Record<string, unknown>;
    requireText(a.query, "query", where);
    const limit = numberParam(a.limit);
    let body: unknown;
    if (a.global === true) {
      if (a.scope !== undefined || a.scopes !== undefined) {
        throw new ScopeError(`${where}: global: true takes no scope or scopes`);
      }
      body = await this.#get("/api/v1/search", { q: a.query, limit }, options);
    } else if (a.scopes !== undefined) {
      if (a.scope !== undefined) throw new ScopeError(`${where}: pass scope or scopes, not both`);
      const scopes = validateScopes(a.scopes, where);
      body = await this.#core.transport.json(
        "POST",
        "/api/v1/search",
        {
          json: { q: a.query, scopes, ...(a.limit === undefined ? {} : { limit: a.limit }) },
          route: "api",
          mutating: false,
        },
        options,
      );
    } else {
      const scope = this.#core.scope(a.scope as Scope | undefined, where);
      body = await this.#get(
        "/api/v1/search",
        { q: a.query, workspace: scope.workspace, project: scope.project, limit },
        options,
      );
    }
    return array(body, "/api/v1/search").map((item, index) =>
      apiSearchHit(item, `/api/v1/search[${index}]`),
    );
  }

  /** The structured briefing. `GET /api/v1/workspaces/{w}/projects/{p}/briefing`. */
  async briefing(
    args: ScopedArgs & { limit?: number } = {},
    options?: CallOptions,
  ): Promise<Briefing> {
    const where = "api.briefing";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const path = `${projectPath(scope, where)}/briefing`;
    return briefing(await this.#get(path, { limit: numberParam(a.limit) }, options), path);
  }

  /**
   * Handoff history (every state unless `state` is set), owner-filtered.
   * `GET /api/v1/workspaces/{w}/projects/{p}/handoffs`.
   */
  async handoffs(args?: ApiHandoffsArgs, options?: CallOptions): Promise<ApiHandoff[]> {
    const where = "api.handoffs";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const path = `${projectPath(scope, where)}/handoffs`;
    const body = await this.#get(
      path,
      {
        state: a.state,
        limit: numberParam(a.limit),
        all_owners: a.allOwners === true ? "true" : undefined,
      },
      options,
    );
    return requiredList(record(body, path), "handoffs", path, apiHandoff);
  }

  /** Sessions that touched a project, newest first. `GET .../sessions`. */
  async sessions(args?: ApiSessionsArgs, options?: CallOptions): Promise<SessionSummary[]> {
    const where = "api.sessions";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    const path = `${projectPath(scope, where)}/sessions`;
    const body = await this.#get(
      path,
      {
        limit: numberParam(a.limit),
        offset: numberParam(a.offset),
        include_open: a.includeOpen === true ? "true" : undefined,
      },
      options,
    );
    return requiredList(record(body, path), "sessions", path, sessionSummary);
  }

  /** One session's observations, paged. `GET .../sessions/{id}/observations`. */
  async observations(
    sessionId: string,
    args?: ApiObservationsArgs,
    options?: CallOptions,
  ): Promise<ObservationsResult> {
    const where = "api.observations";
    const a = argsObject(args, where);
    const scope = this.#core.scope(a.scope, where);
    requireText(sessionId, "sessionId", where);
    const path = `${projectPath(scope, where)}/sessions/${segment(sessionId, "sessionId", where)}/observations`;
    const body = await this.#get(
      path,
      {
        limit: numberParam(a.limit),
        offset: numberParam(a.offset),
        order: a.order,
        kinds: a.kinds === undefined ? undefined : a.kinds.join(","),
        q: a.query,
        body_max_chars: numberParam(a.bodyMaxChars),
      },
      options,
    );
    return observationsResult(body, path);
  }
}
