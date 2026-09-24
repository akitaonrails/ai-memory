//! The companion's own durable queue: one private SQLite database per queue
//! directory. It never opens ai-memory's database or wiki.
//!
//! The binding fixes the destination, producer identity and scope. Pending
//! events keep their body values until acknowledgement. Receipts retain keys
//! and content hashes for duplicate recognition during the retry window.
//! Session records reject a native session changing agents within one queue.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::fsguard;
use crate::identity::{MAX_BODY_BYTES, RETRY_WINDOW_MS, TERMINAL_EVENTS, ValidEvent};

/// Undelivered events allowed in one queue.
pub const MAX_PENDING_ITEMS: i64 = 50_000;
/// Undelivered body bytes allowed in one queue.
pub const MAX_PENDING_BYTES: i64 = 64 * 1024 * 1024;
/// Pending events plus retained receipts. Admission reserves one receipt for
/// each new event so acknowledgement cannot exceed the limit.
pub const MAX_RECEIPTS: i64 = 200_000;
/// Session-to-agent pins retained at once, enforced the same way.
pub const MAX_SESSION_PINS: i64 = 50_000;

const SCHEMA_VERSION: &str = "1";
const IDENTITY: &str = "ai-memory-relay-queue";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS binding(
    id            INTEGER PRIMARY KEY CHECK(id = 1),
    server_url    TEXT NOT NULL,
    producer      TEXT NOT NULL,
    actor         TEXT NOT NULL,
    workspace     TEXT NOT NULL,
    project       TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS pending(
    seq              INTEGER PRIMARY KEY AUTOINCREMENT,
    ingest_key       TEXT NOT NULL UNIQUE,
    event_id         TEXT NOT NULL,
    agent            TEXT NOT NULL,
    event            TEXT NOT NULL,
    session_id       TEXT NOT NULL,
    cwd              TEXT NOT NULL,
    body_json        TEXT NOT NULL,
    body_sha256      TEXT NOT NULL,
    body_bytes       INTEGER NOT NULL,
    first_seen_ms    INTEGER NOT NULL,
    first_attempt_ms INTEGER,
    attempts         INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT
);
CREATE INDEX IF NOT EXISTS pending_session ON pending(agent, session_id, seq);
CREATE TABLE IF NOT EXISTS receipt(
    ingest_key       TEXT PRIMARY KEY,
    body_sha256      TEXT NOT NULL,
    first_attempt_ms INTEGER NOT NULL,
    delivered_at_ms  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS receipt_first_attempt ON receipt(first_attempt_ms);
CREATE TABLE IF NOT EXISTS session_agent(
    session_id   TEXT PRIMARY KEY,
    agent        TEXT NOT NULL,
    last_seen_ms INTEGER NOT NULL
);
";

/// What `init` bound this queue directory to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub server_url: String,
    pub producer: String,
    pub actor: String,
    pub workspace: String,
    pub project: String,
}

/// Whether `init` created the binding or recognized an identical one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindOutcome {
    Created,
    Recognized,
}

/// Per-file result of `enqueue`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnqueueReport {
    pub accepted: usize,
    /// Same id, byte-identical body: already pending or already delivered.
    pub recognized: usize,
}

/// One item selected for delivery.
#[derive(Debug, Clone)]
pub struct PendingItem {
    pub seq: i64,
    pub ingest_key: String,
    pub event_id: String,
    pub agent: String,
    pub event: String,
    pub session_id: String,
    pub body_json: String,
    pub body_bytes: i64,
    pub first_seen_ms: i64,
    pub first_attempt_ms: Option<i64>,
}

impl PendingItem {
    /// Session coordinate: one head per `(agent, session_id)` per batch.
    pub fn session(&self) -> (String, String) {
        (self.agent.clone(), self.session_id.clone())
    }
}

/// A batch plus what was held back while building it.
#[derive(Debug, Default, Clone)]
pub struct Batch {
    pub items: Vec<PendingItem>,
    /// Sessions whose head is past the retry window. The whole session is held:
    /// skipping its head and sending the next event would reorder it.
    pub blocked_sessions: usize,
    /// Sessions skipped because the byte budget filled first.
    pub budget_deferred: usize,
}

/// Counters for `status`. Deliberately payload-free.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub pending_items: i64,
    pub pending_bytes: i64,
    pub pending_sessions: i64,
    pub attempted_items: i64,
    pub expired_items: i64,
    pub oldest_pending_age_ms: i64,
    pub max_attempts: i64,
    pub receipts: i64,
    pub known_sessions: i64,
}

/// The companion's queue database.
pub struct Queue {
    conn: Connection,
    path: PathBuf,
}

impl std::fmt::Debug for Queue {
    // Path only: never connection internals, never row data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queue").field("path", &self.path).finish()
    }
}

impl Queue {
    /// Open (or create) the queue database inside an already-prepared directory.
    ///
    /// Check an existing file's identity before schema writes. Schema and
    /// metadata commit together, so an interrupted initialization can reopen
    /// the empty database left by rollback.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(fsguard::DB_FILE);
        fsguard::check_queue_file(&path)?;
        for name in fsguard::SIDECARS {
            fsguard::check_queue_file(&dir.join(name))?;
        }
        if !path.exists() {
            // Own the mode before SQLite ever opens the file: the `-wal` and
            // `-shm` sidecars inherit the database's permissions, so creating it
            // under the ambient umask would make them world-readable.
            fsguard::create_private_file(&path)?;
            fsguard::check_queue_file(&path)?;
        }
        let mut conn = Connection::open(&path)
            .with_context(|| format!("open relay queue at {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        let state = classify(&conn, &path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;\nPRAGMA synchronous=FULL;\nPRAGMA foreign_keys=ON;",
        )
        .with_context(|| format!("configure relay queue at {}", path.display()))?;
        if state != DbState::Ready {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)
                .with_context(|| format!("initialize relay queue schema at {}", path.display()))?;
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES('identity', ?1), ('schema_version', ?2)",
                params![IDENTITY, SCHEMA_VERSION],
            )?;
            tx.commit()?;
        }
        Ok(Self { conn, path })
    }

    /// Path of the database, for operator-facing messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bind the queue to a destination, producer, actor and scope.
    ///
    /// Repeated initialization must preserve the destination and identity of
    /// queued events, including events whose acknowledgement was lost.
    pub fn bind(&mut self, binding: &Binding, now_ms: i64) -> Result<BindOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = match read_binding(&tx)? {
            Some(existing) if existing == *binding => BindOutcome::Recognized,
            Some(existing) => {
                let mut changed = Vec::new();
                let mut note = |label: &str, old: &str, new: &str| {
                    if old != new {
                        changed.push(format!("{label}: {old:?} -> {new:?}"));
                    }
                };
                note("server-url", &existing.server_url, &binding.server_url);
                note("producer", &existing.producer, &binding.producer);
                note("actor", &existing.actor, &binding.actor);
                note("workspace", &existing.workspace, &binding.workspace);
                note("project", &existing.project, &binding.project);
                bail!(
                    "binding mismatch: this queue is already bound ({}). \
                     Use a separate --queue-dir for a different destination or identity",
                    changed.join(", ")
                );
            }
            None => {
                tx.execute(
                    "INSERT INTO binding(id, server_url, producer, actor, workspace, project, created_at_ms)
                     VALUES(1, ?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        binding.server_url,
                        binding.producer,
                        binding.actor,
                        binding.workspace,
                        binding.project,
                        now_ms,
                    ],
                )?;
                BindOutcome::Created
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The binding, or a clear error telling the operator to run `init` first.
    pub fn binding(&self) -> Result<Binding> {
        read_binding(&self.conn)?.ok_or_else(|| {
            anyhow::anyhow!("queue is not bound yet; run `ai-memory-relay init` first")
        })
    }

    /// Persist a whole validated file, all-or-nothing.
    ///
    /// `first_seen_ms` is stamped here for reporting. It is *not* the retry
    /// clock: that starts at the first durable attempt, recorded before the
    /// first HTTP request (see [`Queue::stamp_attempt`]).
    pub fn enqueue(&mut self, events: &[ValidEvent], now_ms: i64) -> Result<EnqueueReport> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut items, mut bytes): (i64, i64) = tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(body_bytes), 0) FROM pending",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        // Replay protection is bounded, and the bound covers pending *and*
        // receipts: every pending item becomes a receipt when it is
        // acknowledged, so counting receipts alone would let the queue commit to
        // more protection than the cap allows. Pruning stays reserved for
        // entries past the 30-day window, so a full queue refuses new ids rather
        // than forgetting a recently delivered one. A duplicate is still
        // recognized while full, because recognizing it adds nothing.
        let receipts: i64 = tx.query_row("SELECT COUNT(*) FROM receipt", [], |r| r.get(0))?;
        let mut pins: i64 = tx.query_row("SELECT COUNT(*) FROM session_agent", [], |r| r.get(0))?;
        let mut report = EnqueueReport::default();
        let mut seen_keys: HashMap<&str, &str> = HashMap::new();
        let mut seen_sessions: HashMap<&str, &str> = HashMap::new();
        for event in events {
            // A session id belongs to exactly one wire agent. Two agents on one
            // native session is the producer bug the server can only answer with
            // an acknowledged SessionCollision drop.
            let bound: Option<String> = tx
                .query_row(
                    "SELECT agent FROM session_agent WHERE session_id = ?1",
                    params![event.session_id],
                    |r| r.get(0),
                )
                .optional()?;
            let bound = bound.or_else(|| {
                seen_sessions
                    .get(event.session_id.as_str())
                    .map(|a| (*a).to_owned())
            });
            if bound.is_none() {
                pins += 1;
                if pins > MAX_SESSION_PINS {
                    bail!(
                        "session pin table is full: {MAX_SESSION_PINS} session(s) already pinned \
                         to an agent. Nothing was enqueued and no pin was forgotten; pins are \
                         released as they pass the 30-day retry window"
                    );
                }
            }
            if let Some(bound) = bound
                && bound != event.agent
            {
                bail!(
                    "session identity conflict: session {:?} is already bound to agent {:?} in \
                     this queue, but item with event_id {} claims agent {:?}. Nothing was \
                     enqueued; a native session belongs to one harness",
                    event.session_id,
                    bound,
                    event.event_id,
                    event.agent
                );
            }
            seen_sessions.insert(&event.session_id, &event.agent);

            if let Some(previous) = seen_keys.insert(&event.ingest_key, &event.body_sha256)
                && previous != event.body_sha256
            {
                bail!(collision_message(
                    &event.ingest_key,
                    "another item in this file"
                ));
            }
            let known: Option<String> = tx
                .query_row(
                    "SELECT body_sha256 FROM pending WHERE ingest_key = ?1
                     UNION ALL
                     SELECT body_sha256 FROM receipt WHERE ingest_key = ?1",
                    params![event.ingest_key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(known) = known {
                if known != event.body_sha256 {
                    bail!(collision_message(
                        &event.ingest_key,
                        "an item already recorded in this queue"
                    ));
                }
                report.recognized += 1;
                continue;
            }
            let body_bytes = event.body_json.len() as i64;
            items += 1;
            bytes += body_bytes;
            if items > MAX_PENDING_ITEMS || bytes > MAX_PENDING_BYTES {
                bail!(
                    "queue is full: {items} items / {bytes} bytes would exceed the \
                     {MAX_PENDING_ITEMS}-item / {MAX_PENDING_BYTES}-byte limit. Nothing was \
                     enqueued and nothing was dropped; flush the queue first"
                );
            }
            // Reserve the receipt this item will need once it is acknowledged.
            let protected = receipts + items;
            if protected > MAX_RECEIPTS {
                bail!(
                    "replay protection is full: {receipts} acknowledged key(s) plus {items} \
                     pending would need {protected} slots, over the {MAX_RECEIPTS} limit. \
                     Nothing was enqueued and nothing was forgotten; slots are released as \
                     entries pass the 30-day retry window, or start a fresh --queue-dir"
                );
            }
            tx.execute(
                "INSERT INTO pending(
                     ingest_key, event_id, agent, event, session_id, cwd,
                     body_json, body_sha256, body_bytes, first_seen_ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    event.ingest_key,
                    event.event_id,
                    event.agent,
                    event.event,
                    event.session_id,
                    event.cwd,
                    event.body_json,
                    event.body_sha256,
                    body_bytes,
                    now_ms,
                ],
            )?;
            report.accepted += 1;
        }
        for (session_id, agent) in seen_sessions {
            tx.execute(
                "INSERT INTO session_agent(session_id, agent, last_seen_ms) VALUES(?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET last_seen_ms = excluded.last_seen_ms",
                params![session_id, agent, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(report)
    }

    /// Pick at most one pending head per `(agent, session_id)`, oldest first,
    /// within both an item count and a wire-byte budget.
    ///
    /// One head per session per batch is what makes a partial acknowledgement
    /// safe: two events of one session are never in flight together, so no ack
    /// subset can commit event 2 while event 1 is still pending, and a terminal
    /// event can never overtake an earlier one. `deferred` holds sessions this
    /// flush already stopped on; they are skipped without touching their order.
    pub fn select_batch(
        &self,
        limit: usize,
        byte_budget: usize,
        now_ms: i64,
        deferred: &HashSet<(String, String)>,
        cost_of: impl Fn(&PendingItem) -> usize,
    ) -> Result<Batch> {
        let limit = limit.clamp(1, crate::identity::MAX_BATCH_ITEMS);
        let mut stmt = self.conn.prepare(
            "SELECT seq, ingest_key, event_id, agent, event, session_id, body_json,
                    body_bytes, first_seen_ms, first_attempt_ms
             FROM pending ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let mut batch = Batch::default();
        let mut spent = 0usize;
        while let Some(row) = rows.next()? {
            if batch.items.len() >= limit {
                break;
            }
            let item = PendingItem {
                seq: row.get(0)?,
                ingest_key: row.get(1)?,
                event_id: row.get(2)?,
                agent: row.get(3)?,
                event: row.get(4)?,
                session_id: row.get(5)?,
                body_json: row.get(6)?,
                body_bytes: row.get(7)?,
                first_seen_ms: row.get(8)?,
                first_attempt_ms: row.get(9)?,
            };
            let session = item.session();
            if !seen.insert(session.clone()) {
                continue;
            }
            if deferred.contains(&session) {
                continue;
            }
            if is_expired(item.first_attempt_ms, now_ms) {
                batch.blocked_sessions += 1;
                continue;
            }
            let cost = cost_of(&item);
            if !batch.items.is_empty() && spent + cost > byte_budget {
                batch.budget_deferred += 1;
                continue;
            }
            debug_assert!(
                !TERMINAL_EVENTS.contains(&item.event.as_str()) || self.session_head(&item)?,
                "a terminal event was selected while its session had an older pending item"
            );
            spent += cost;
            batch.items.push(item);
        }
        Ok(batch)
    }

    fn session_head(&self, item: &PendingItem) -> Result<bool> {
        let older: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pending WHERE agent = ?1 AND session_id = ?2 AND seq < ?3",
            params![item.agent, item.session_id, item.seq],
            |r| r.get(0),
        )?;
        Ok(older == 0)
    }

    /// Record the durable start of the retry window, **before** the request.
    ///
    /// `COALESCE` is the whole point: a restart, a lost ack, or a later retry
    /// never moves the clock forward, so the 30-day guard measures from the
    /// first time this event was actually put on the wire.
    pub fn stamp_attempt(&mut self, keys: &[String], now_ms: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for key in keys {
            tx.execute(
                "UPDATE pending
                    SET first_attempt_ms = COALESCE(first_attempt_ms, ?2),
                        attempts = attempts + 1
                  WHERE ingest_key = ?1",
                params![key, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Move acknowledged items to compact receipts, in one transaction.
    ///
    /// An acknowledgement means delivered *or* deliberately dropped by server
    /// policy (a capture-protocol drop, a subagent drop, a session-collision
    /// drop). It never promises an observation was written.
    pub fn confirm(&mut self, keys: &[String], now_ms: i64) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut moved = 0;
        for key in keys {
            let row: Option<(String, Option<i64>, i64)> = tx
                .query_row(
                    "SELECT body_sha256, first_attempt_ms, first_seen_ms
                       FROM pending WHERE ingest_key = ?1",
                    params![key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((body_sha256, first_attempt_ms, first_seen_ms)) = row else {
                continue;
            };
            tx.execute(
                "INSERT INTO receipt(ingest_key, body_sha256, first_attempt_ms, delivered_at_ms)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(ingest_key) DO UPDATE SET delivered_at_ms = excluded.delivered_at_ms",
                params![
                    key,
                    body_sha256,
                    first_attempt_ms.unwrap_or(first_seen_ms),
                    now_ms
                ],
            )?;
            tx.execute("DELETE FROM pending WHERE ingest_key = ?1", params![key])?;
            moved += 1;
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Charge one item with a delivery failure. The class is a short label,
    /// never a server body and never payload.
    pub fn record_failure(&mut self, key: &str, class: &str) -> Result<()> {
        let class: String = class.chars().filter(|c| !c.is_control()).take(80).collect();
        self.conn.execute(
            "UPDATE pending SET last_error = ?2 WHERE ingest_key = ?1",
            params![key, class],
        )?;
        Ok(())
    }

    /// Release expired receipts and session records without pending events.
    pub fn prune(&mut self, now_ms: i64) -> Result<usize> {
        let cutoff = now_ms.saturating_sub(RETRY_WINDOW_MS);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipts = tx.execute(
            "DELETE FROM receipt WHERE first_attempt_ms < ?1",
            params![cutoff],
        )?;
        let sessions = tx.execute(
            "DELETE FROM session_agent WHERE last_seen_ms < ?1
               AND session_id NOT IN (SELECT session_id FROM pending)",
            params![cutoff],
        )?;
        tx.commit()?;
        Ok(receipts + sessions)
    }

    /// Payload-free counters.
    pub fn stats(&self, now_ms: i64) -> Result<Stats> {
        let cutoff = now_ms.saturating_sub(RETRY_WINDOW_MS);
        let (pending_items, pending_bytes, oldest, max_attempts, attempted_items) =
            self.conn.query_row(
                "SELECT COUNT(*), COALESCE(SUM(body_bytes), 0), COALESCE(MIN(first_seen_ms), 0),
                        COALESCE(MAX(attempts), 0), COUNT(first_attempt_ms)
                 FROM pending",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )?;
        let pending_sessions: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT agent, session_id FROM pending)",
            [],
            |r| r.get(0),
        )?;
        let expired_items: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pending WHERE first_attempt_ms IS NOT NULL
                                            AND first_attempt_ms <= ?1",
            params![cutoff],
            |r| r.get(0),
        )?;
        let receipts: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM receipt", [], |r| r.get(0))?;
        let known_sessions: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM session_agent", [], |r| r.get(0))?;
        Ok(Stats {
            pending_items,
            pending_bytes,
            pending_sessions,
            attempted_items,
            expired_items,
            oldest_pending_age_ms: if pending_items == 0 {
                0
            } else {
                now_ms.saturating_sub(oldest)
            },
            max_attempts,
            receipts,
            known_sessions,
        })
    }
}

/// Past the retry window an event may not be auto-resent.
///
/// Refuse the boundary itself. This comparison depends on an accurate host
/// clock; a backwards clock change can extend retries past server key expiry.
pub fn is_expired(first_attempt_ms: Option<i64>, now_ms: i64) -> bool {
    match first_attempt_ms {
        Some(started) => now_ms.saturating_sub(started) >= RETRY_WINDOW_MS,
        None => false,
    }
}

fn collision_message(key: &str, other: &str) -> String {
    format!(
        "identity collision: ingest key {key} was already used by {other} with different content. \
         The same producer event id must always carry the same body; nothing was enqueued. \
         Body content is deliberately not shown. Per-body limit: {MAX_BODY_BYTES} bytes"
    )
}

/// What an existing file turned out to be.
///
/// Schema and identity commit together. Recovery therefore accepts an empty
/// database or an identified queue; generic table names do not establish ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DbState {
    /// A SQLite file with no user objects: empty, or rolled back to empty.
    Fresh,
    /// A relay queue of this exact schema version.
    Ready,
}

/// Classify an existing file before touching it. Never mutates.
fn classify(conn: &Connection, path: &Path) -> Result<DbState> {
    let refuse = |detail: String| -> anyhow::Error {
        anyhow::anyhow!(
            "{} is not a usable relay queue ({detail}); nothing was modified. \
             Point --queue-dir at an empty directory or restore the original queue",
            path.display()
        )
    };
    // Tables, views, triggers and standalone indexes all count: any user object
    // means the file belongs to something, and that something is not this relay
    // unless it also carries the identity row.
    let objects: Result<i64, rusqlite::Error> = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    );
    // A file that is not a SQLite database fails right here, before any write.
    let objects = objects.map_err(|e| refuse(format!("cannot read its object list: {e}")))?;
    if objects == 0 {
        return Ok(DbState::Fresh);
    }
    let rows: Result<Vec<(String, String)>, rusqlite::Error> = (|| {
        let mut stmt =
            conn.prepare("SELECT key, value FROM meta WHERE key IN ('identity','schema_version')")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect()
    })();
    let meta: HashMap<String, String> = rows
        .map_err(|e| refuse(format!("it carries no readable relay metadata: {e}")))?
        .into_iter()
        .collect();
    match (
        meta.get("identity").map(String::as_str),
        meta.get("schema_version").map(String::as_str),
    ) {
        (Some(IDENTITY), Some(SCHEMA_VERSION)) => Ok(DbState::Ready),
        (Some(IDENTITY), Some(other)) => Err(refuse(format!(
            "schema version {other} but this build speaks {SCHEMA_VERSION}"
        ))),
        (Some(other), _) => Err(refuse(format!("identity is {other:?}"))),
        _ => Err(refuse(
            "it holds user objects but no relay identity row".into(),
        )),
    }
}

fn read_binding(conn: &Connection) -> Result<Option<Binding>> {
    Ok(conn
        .query_row(
            "SELECT server_url, producer, actor, workspace, project FROM binding WHERE id = 1",
            [],
            |r| {
                Ok(Binding {
                    server_url: r.get(0)?,
                    producer: r.get(1)?,
                    actor: r.get(2)?,
                    workspace: r.get(3)?,
                    project: r.get(4)?,
                })
            },
        )
        .optional()?)
}
