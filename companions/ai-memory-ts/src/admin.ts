/**
 * Root-only lifecycle operations: list checkpoints, restore a page, purge a
 * session or a whole project. Imported from a separate entry point,
 * `ai-memory-ts/admin`, so an application that only reads and writes pages
 * never loads code that can purge a project.
 *
 * ```ts
 * import { AdminClient } from "ai-memory-ts/admin";
 *
 * const admin = new AdminClient({ serverUrl, token: rootToken });
 * const preview = await admin.purgeSession({ workspace, project, sessionId });
 * // preview.dryRun === true: counts only, nothing deleted
 * await admin.purgeSession({ workspace, project, sessionId, dryRun: false, confirm: true });
 * ```
 *
 * Every `/admin/*` route is root-only once the server has a DB user or
 * trusted-proxy identities; a user `aim_` key is always user-level and gets
 * {@link ForbiddenError}.
 *
 * @module
 */

import { requireText } from "./core.js";
import { array, num, optStr, record, str, strings } from "./decode.js";
import { ArgumentError, ProtocolError } from "./errors.js";
import { validateScope } from "./scope.js";
import { Transport, type CallOptions, type ConnectionOptions } from "./transport.js";
import type { Raw } from "./types.js";

export {
  AbortError,
  AiMemoryError,
  ArgumentError,
  AuthError,
  ConfigError,
  ForbiddenError,
  HttpError,
  NotFoundError,
  ProtocolError,
  RateLimitedError,
  ScopeError,
  TimeoutError,
  TransportError,
} from "./errors.js";
export type { CallOptions, ConnectionOptions, FetchLike } from "./transport.js";

/** Options of {@link AdminClient}: connection settings only, no default scope. */
export type AdminClientOptions = ConnectionOptions;

/** One wiki git checkpoint. */
export interface Checkpoint {
  /** Full commit id; usable as `rev` in {@link AdminClient.restorePage}. */
  readonly oid: string;
  /** First 12 hex characters. */
  readonly shortOid: string;
  /** Commit time in seconds since the Unix epoch. */
  readonly time: number;
  /** First line of the commit message (`memory_delete_page: <path>` for a delete). */
  readonly summary: string;
}

/** Arguments of {@link AdminClient.restorePage}. */
export interface RestorePageArgs {
  /** Workspace; must exist. */
  workspace: string;
  /** Project; must exist. */
  project: string;
  /** Wiki path to restore. */
  path: string;
  /**
   * A git revision that still holds the page: a delete's `preCheckpoint`, or
   * `<checkpoint>~1` when `preCheckpoint` was `null`.
   */
  rev: string;
}

/** Result of {@link AdminClient.restorePage}. */
export interface RestorePageResult {
  /** The new latest version id of the restored page. */
  readonly pageId: string;
  /** Path restored. */
  readonly path: string;
  /** The revision it came from. */
  readonly restoredFrom: string;
  /** Checkpoint taken before the restore, when the tree had changes. */
  readonly preCheckpoint: string | undefined;
  /** Checkpoint recording the restore, when it changed the tree. */
  readonly checkpoint: string | undefined;
  /** The server's decoded JSON. */
  readonly raw: Raw;
}

/** Flags shared by both purges. */
interface PurgeFlags {
  /**
   * Preview only. Default `true`: the server reports the counts a purge
   * would produce and deletes nothing.
   */
  dryRun?: boolean;
  /** Required, together with `dryRun: false`, for an irreversible purge. */
  confirm?: boolean;
  /** Rebuild the FTS indexes and `VACUUM` after a real purge to reclaim space. */
  compact?: boolean;
}

/** Arguments of {@link AdminClient.purgeSession}. */
export interface PurgeSessionArgs extends PurgeFlags {
  /** Workspace; must exist. */
  workspace: string;
  /** Project; must exist and own the session. */
  project: string;
  /** Full session UUID. */
  sessionId: string;
}

/** Arguments of {@link AdminClient.purgeProject}. */
export interface PurgeProjectArgs extends PurgeFlags {
  /** Workspace; must exist. */
  workspace: string;
  /** Project; must exist. */
  project: string;
  /**
   * Purge even while a managed workstream under the project holds a live run
   * lease. The running agent then loses the lease row and cannot save its
   * history. Without it the server answers 409.
   */
  force?: boolean;
}

/** Result of {@link AdminClient.purgeSession}. */
export interface PurgeSessionReport {
  /** True for a preview: nothing was deleted. */
  readonly dryRun: boolean;
  /** Session purged (or previewed). */
  readonly sessionId: string;
  /** Observations deleted. */
  readonly observationsDeleted: number;
  /** Handoffs the session authored, deleted. */
  readonly handoffsDeleted: number;
  /** Page rows deleted, superseded versions included. */
  readonly pagesDeleted: number;
  /** Distinct page paths whose rows were deleted. */
  readonly removedPaths: readonly string[];
  /** The server's decoded JSON (collateral counts, files, checkpoints). */
  readonly raw: Raw;
}

/** Result of {@link AdminClient.purgeProject}. */
export interface PurgeProjectReport {
  /** True for a preview: nothing was deleted. */
  readonly dryRun: boolean;
  /** Page rows deleted. */
  readonly pagesDeleted: number;
  /** Sessions deleted. */
  readonly sessionsDeleted: number;
  /** Observations deleted. */
  readonly observationsDeleted: number;
  /** Handoffs deleted. */
  readonly handoffsDeleted: number;
  /** The server's decoded JSON (collateral counts, files, checkpoints). */
  readonly raw: Raw;
}

/**
 * Decide whether a purge is a preview, refusing an irreversible call that
 * lacks either flag before anything is sent.
 */
function purgeMode(args: PurgeFlags, where: string): { dryRun: boolean } {
  if (args.dryRun !== undefined && typeof args.dryRun !== "boolean") {
    throw new ArgumentError(`${where}: dryRun must be a boolean`);
  }
  const dryRun = args.dryRun !== false;
  if (!dryRun && args.confirm !== true) {
    throw new ArgumentError(
      `${where}: an irreversible purge needs dryRun: false and confirm: true in the same call`,
    );
  }
  return { dryRun };
}

function adminArgs<T extends object>(value: T, where: string): T {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new ArgumentError(`${where}: arguments must be an object`);
  }
  return value;
}

/**
 * Client for the root-only `/admin/*` lifecycle routes. Every method needs
 * the server's root token once the server has DB users.
 */
export class AdminClient {
  readonly #transport: Transport;

  /** @throws {@link ConfigError} for an unusable URL, token or timeout. */
  constructor(options: AdminClientOptions) {
    this.#transport = new Transport(options);
  }

  /** The server URL the client talks to. */
  get serverUrl(): string {
    return this.#transport.serverUrl;
  }

  async #post(path: string, body: unknown, mutating: boolean, options?: CallOptions) {
    return this.#transport.json("POST", path, { json: body, route: "admin", mutating }, options);
  }

  /** Recent wiki checkpoints, newest first (up to 100). `GET /admin/checkpoints`. */
  async checkpoints(args: { limit?: number } = {}, options?: CallOptions): Promise<Checkpoint[]> {
    const a = adminArgs(args, "checkpoints");
    const body = await this.#transport.json(
      "GET",
      "/admin/checkpoints",
      {
        query: { limit: a.limit === undefined ? undefined : String(a.limit) },
        route: "admin",
        mutating: false,
      },
      options,
    );
    return array(body, "/admin/checkpoints").map((item, index) => {
      const where = `/admin/checkpoints[${index}]`;
      const o = record(item, where);
      return {
        oid: str(o, "oid", where),
        shortOid: str(o, "short_oid", where),
        time: num(o, "time", where),
        summary: this.#transport.safeText(str(o, "summary", where)),
      };
    });
  }

  /**
   * Write a page's content at `rev` back as its latest version. There is no
   * dry run: a successful restore writes a new version (the server checkpoints
   * first). A purged session summary is refused with HTTP 409.
   * `POST /admin/restore-page`.
   */
  async restorePage(args: RestorePageArgs, options?: CallOptions): Promise<RestorePageResult> {
    const where = "restorePage";
    const a = adminArgs(args, where);
    const scope = validateScope({ workspace: a.workspace, project: a.project }, where);
    requireText(a.path, "path", where);
    requireText(a.rev, "rev", where);
    const body = await this.#post(
      "/admin/restore-page",
      { workspace: scope.workspace, project: scope.project, path: a.path, rev: a.rev },
      true,
      options,
    );
    const o = record(body, "/admin/restore-page");
    return {
      pageId: str(o, "page_id", "/admin/restore-page"),
      path: str(o, "path", "/admin/restore-page"),
      restoredFrom: str(o, "restored_from", "/admin/restore-page"),
      preCheckpoint: optStr(o, "pre_checkpoint", "/admin/restore-page"),
      checkpoint: optStr(o, "checkpoint", "/admin/restore-page"),
      raw: o,
    };
  }

  /**
   * Purge one session: its observations, the handoffs it authored and the
   * pages it produced. Defaults to a dry run. An irreversible purge needs
   * `dryRun: false` and `confirm: true` together; otherwise the call is
   * refused with {@link ArgumentError} before anything is sent.
   * `POST /admin/purge-session`.
   */
  async purgeSession(args: PurgeSessionArgs, options?: CallOptions): Promise<PurgeSessionReport> {
    const where = "purgeSession";
    const a = adminArgs(args, where);
    const scope = validateScope({ workspace: a.workspace, project: a.project }, where);
    requireText(a.sessionId, "sessionId", where);
    const { dryRun } = purgeMode(a, where);
    const body = await this.#post(
      "/admin/purge-session",
      {
        workspace: scope.workspace,
        project: scope.project,
        session_id: a.sessionId,
        // A preview always sends confirm: false, so a server that ignored
        // dry_run would refuse instead of purging.
        confirm: !dryRun,
        dry_run: dryRun,
        compact: a.compact === true,
      },
      !dryRun,
      options,
    );
    const path = "/admin/purge-session";
    const o = record(body, path);
    const reportedDryRun = o["dry_run"] === true;
    if (dryRun && !reportedDryRun) {
      throw new ProtocolError(`${path}: the server did not confirm a dry run`);
    }
    return {
      dryRun: reportedDryRun,
      sessionId: str(o, "session_id", path),
      observationsDeleted: num(o, "observations_deleted", path),
      handoffsDeleted: num(o, "handoffs_deleted", path),
      pagesDeleted: num(o, "pages_deleted", path),
      removedPaths: strings(o, "removed_paths", path),
      raw: o,
    };
  }

  /**
   * Purge a whole project: every page, session, observation and handoff, and
   * its wiki directory. Defaults to a dry run. An irreversible purge needs
   * `dryRun: false` and `confirm: true` together. `force` is a separate flag
   * that purges despite a live managed-workstream lease.
   * `POST /admin/purge-project`.
   */
  async purgeProject(args: PurgeProjectArgs, options?: CallOptions): Promise<PurgeProjectReport> {
    const where = "purgeProject";
    const a = adminArgs(args, where);
    const scope = validateScope({ workspace: a.workspace, project: a.project }, where);
    const { dryRun } = purgeMode(a, where);
    if (a.force !== undefined && typeof a.force !== "boolean") {
      throw new ArgumentError(`${where}: force must be a boolean`);
    }
    const body = await this.#post(
      "/admin/purge-project",
      {
        workspace: scope.workspace,
        project: scope.project,
        confirm: !dryRun,
        dry_run: dryRun,
        compact: a.compact === true,
        force: a.force === true,
      },
      !dryRun,
      options,
    );
    const path = "/admin/purge-project";
    const o = record(body, path);
    const reportedDryRun = o["dry_run"] === true;
    if (dryRun && !reportedDryRun) {
      throw new ProtocolError(`${path}: the server did not confirm a dry run`);
    }
    return {
      dryRun: reportedDryRun,
      pagesDeleted: num(o, "pages_deleted", path),
      sessionsDeleted: num(o, "sessions_deleted", path),
      observationsDeleted: num(o, "observations_deleted", path),
      handoffsDeleted: num(o, "handoffs_deleted", path),
      raw: o,
    };
  }

  /** Serialized form: the server URL, never the token. */
  toJSON(): Record<string, unknown> {
    return this.#transport.toJSON();
  }

  /** @internal */
  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `AdminClient ${JSON.stringify(this.toJSON())}`;
  }
}
