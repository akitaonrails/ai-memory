//! CLI command operations and reports.
//!
//! Reports include queue metadata and counts. Event bodies and bearer tokens
//! are kept out of diagnostics.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::ack::{self, BatchAck};
use crate::fsguard;
use crate::http::{self, BatchItem, Sender, Token};
use crate::identity::{self, InputEvent, MAX_BATCH_ITEMS, ValidEvent};
use crate::queue::{self, Batch, BindOutcome, Binding, PendingItem, Queue};

/// Largest `enqueue --file` input accepted, before parsing.
pub const MAX_INPUT_BYTES: u64 = 32 * 1024 * 1024;
/// Most events accepted from one input file.
pub const MAX_INPUT_ITEMS: usize = 10_000;
/// Wire-byte budget for one batch. The server's `/hook` body limit is 10 MiB
/// (`serve.rs`); core's own spool drain uses 8 MiB, and so does this.
pub const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// JSON framing charged per item on top of its URL and body: `{"url":"","body":},`.
const ITEM_FRAMING_BYTES: usize = 32;
/// Transport retries per batch, on top of the first attempt.
const TRANSPORT_RETRIES: usize = 2;
/// Request timeout budget per item; a batch gets this times its item count.
const PER_EVENT_TIMEOUT: Duration = Duration::from_secs(2);
/// Absolute ceiling for one batch request, however many items it carries.
const MAX_BATCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Wall-clock ceiling for one whole flush.
const TOTAL_BUDGET: Duration = Duration::from_secs(900);
/// Ceiling for one backoff sleep between transport retries.
const MAX_BACKOFF: Duration = Duration::from_secs(2);
/// TCP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The one knob a `flush` exposes. Everything else is a constant above, so the
/// public surface cannot be tuned into an unbounded run.
#[derive(Debug, Clone)]
pub struct FlushOptions {
    /// Batches attempted in one flush. Finite by construction.
    pub max_batches: usize,
}

impl Default for FlushOptions {
    fn default() -> Self {
        Self { max_batches: 64 }
    }
}

/// What a command wants the process to exit with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub summary: Vec<String>,
    /// Work remains: unacknowledged items are still queued.
    pub pending: bool,
    /// Something went wrong. The queue is intact either way.
    pub failure: Option<String>,
}

impl Report {
    fn ok(summary: Vec<String>) -> Self {
        Self {
            summary,
            pending: false,
            failure: None,
        }
    }

    /// 0 = drained, 2 = failure, 3 = nothing lost but work remains.
    pub fn exit_code(&self) -> i32 {
        if self.failure.is_some() {
            2
        } else if self.pending {
            3
        } else {
            0
        }
    }
}

/// `init`: bind a queue directory to one destination, producer, actor and scope.
pub fn init(
    dir: &Path,
    server_url: &str,
    producer: &str,
    actor: &str,
    workspace: &str,
    project: &str,
) -> Result<Report> {
    identity::check_producer(producer).map_err(anyhow::Error::msg)?;
    identity::check_actor(actor).map_err(anyhow::Error::msg)?;
    identity::check_scope("workspace", workspace).map_err(anyhow::Error::msg)?;
    identity::check_scope("project", project).map_err(anyhow::Error::msg)?;
    let binding = Binding {
        server_url: http::normalize_server_url(server_url)?,
        producer: producer.to_owned(),
        actor: actor.to_owned(),
        workspace: workspace.to_owned(),
        project: project.to_owned(),
    };
    let dir = fsguard::prepare_queue_dir(dir)?;
    let mut queue = Queue::open(&dir)?;
    let outcome = queue.bind(&binding, crate::now_ms())?;
    let verb = match outcome {
        BindOutcome::Created => "bound",
        BindOutcome::Recognized => "already bound (identical)",
    };
    Ok(Report::ok(vec![
        format!("queue {verb}: {}", queue.path().display()),
        format!("server:    {}", binding.server_url),
        format!("producer:  {}", binding.producer),
        format!("actor:     {}", binding.actor),
        format!("scope:     {}/{}", binding.workspace, binding.project),
    ]))
}

/// `enqueue`: validate a whole file, then persist it all-or-nothing.
pub fn enqueue(dir: &Path, file: &Path) -> Result<Report> {
    let dir = fsguard::prepare_queue_dir(dir)?;
    let mut queue = Queue::open(&dir)?;
    let binding = queue.binding()?;
    let events = read_input(file, &binding)?;
    let report = queue.enqueue(&events, crate::now_ms())?;
    let stats = queue.stats(crate::now_ms())?;
    Ok(Report::ok(vec![
        format!(
            "enqueued {} event(s), recognized {} duplicate(s)",
            report.accepted, report.recognized
        ),
        format!(
            "pending now: {} event(s) across {} session(s), {} byte(s)",
            stats.pending_items, stats.pending_sessions, stats.pending_bytes
        ),
    ]))
}

/// Read and validate an input file without ever quoting its content back.
fn read_input(file: &Path, binding: &Binding) -> Result<Vec<ValidEvent>> {
    let meta =
        std::fs::metadata(file).with_context(|| format!("read --file {}", file.display()))?;
    if meta.len() > MAX_INPUT_BYTES {
        bail!(
            "--file {} is {} bytes, over the {MAX_INPUT_BYTES}-byte input limit",
            file.display(),
            meta.len()
        );
    }
    let mut raw = Vec::new();
    std::fs::File::open(file)
        .with_context(|| format!("open --file {}", file.display()))?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut raw)
        .with_context(|| format!("read --file {}", file.display()))?;
    // The metadata check above is an early out; this is the one that counts,
    // because the file can grow between `stat` and `read`.
    if raw.len() as u64 > MAX_INPUT_BYTES {
        bail!(
            "--file {} is over the {MAX_INPUT_BYTES}-byte input limit",
            file.display()
        );
    }
    let input: Vec<InputEvent> = serde_json::from_slice(&raw).map_err(|e| {
        // Category plus position only. `Display` on a serde error can quote the
        // offending value ("invalid type: string \"...\""), and an event body is
        // exactly the thing that must not reach a terminal or a log.
        anyhow::anyhow!(
            "--file {} is not a valid event array ({} error at line {}, column {}): expected \
             [{{\"event_id\",\"agent\",\"event\",\"body\"}}, ...]",
            file.display(),
            match e.classify() {
                serde_json::error::Category::Io => "io",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "schema",
                serde_json::error::Category::Eof => "truncated input",
            },
            e.line(),
            e.column()
        )
    })?;
    if input.is_empty() {
        bail!("--file {} contains no events", file.display());
    }
    if input.len() > MAX_INPUT_ITEMS {
        bail!(
            "--file {} carries {} events, over the {MAX_INPUT_ITEMS}-event input limit",
            file.display(),
            input.len()
        );
    }
    input
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            identity::validate(index, event, &binding.producer, &binding.actor)
                .map_err(anyhow::Error::msg)
        })
        .collect()
}

/// `status`: payload-free counters for the queue, as one stable JSON object.
///
/// Machine-readable on purpose: an operator script (and the end-to-end smoke
/// test) reads `pending_items` from it. Every field here is a count, a name the
/// operator chose, or a path. No event body, no cwd, no token.
pub fn status(dir: &Path) -> Result<Report> {
    let dir = fsguard::prepare_queue_dir(dir)?;
    let queue = Queue::open(&dir)?;
    let binding = queue.binding()?;
    let stats = queue.stats(crate::now_ms())?;
    let document = serde_json::json!({
        "queue": queue.path().display().to_string(),
        "server_url": binding.server_url,
        "producer": binding.producer,
        "actor": binding.actor,
        "workspace": binding.workspace,
        "project": binding.project,
        "pending_items": stats.pending_items,
        "pending_bytes": stats.pending_bytes,
        "pending_sessions": stats.pending_sessions,
        "attempted_items": stats.attempted_items,
        "expired_items": stats.expired_items,
        "oldest_pending_age_ms": stats.oldest_pending_age_ms,
        "max_attempts": stats.max_attempts,
        "receipts": stats.receipts,
        "known_sessions": stats.known_sessions,
        "limits": {
            "pending_items": queue::MAX_PENDING_ITEMS,
            "pending_bytes": queue::MAX_PENDING_BYTES,
            "receipts": queue::MAX_RECEIPTS,
            "known_sessions": queue::MAX_SESSION_PINS,
            "body_bytes": crate::identity::MAX_BODY_BYTES,
            "batch_items": MAX_BATCH_ITEMS,
            "batch_bytes": MAX_BATCH_BYTES,
        },
        "retry_window_ms": crate::identity::RETRY_WINDOW_MS,
        "ack_means": "delivered or dropped by server policy, not that an observation was written",
    });
    Ok(Report::ok(vec![serde_json::to_string_pretty(&document)?]))
}

/// `flush`: deliver pending events, oldest first, one head per session per batch.
pub fn flush(dir: &Path, options: &FlushOptions) -> Result<Report> {
    let dir = fsguard::prepare_queue_dir(dir)?;
    let lock_path = dir.join("flush.lock");
    fsguard::check_queue_file(&lock_path)?;
    if !lock_path.exists() {
        fsguard::create_private_file(&lock_path)?;
        fsguard::check_queue_file(&lock_path)?;
    }
    let lock = std::fs::OpenOptions::new()
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    // Serialize flushes across processes. The lock file itself is never removed:
    // releasing the lock is dropping the handle.
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        bail!(
            "another flush already holds {}; run one flush at a time",
            lock_path.display()
        );
    }
    let result = flush_locked(&dir, options);
    let _ = fs2::FileExt::unlock(&lock);
    result
}

fn flush_locked(dir: &Path, options: &FlushOptions) -> Result<Report> {
    let mut queue = Queue::open(dir)?;
    let binding = queue.binding()?;
    queue.prune(crate::now_ms())?;
    let token = Token::from_env()?;
    let authenticated = token.is_some();
    let sender = Sender::new(binding.server_url.clone(), token, CONNECT_TIMEOUT)?;

    let started = Instant::now();
    let mut deferred: HashSet<(String, String)> = HashSet::new();
    let mut delivered = 0usize;
    let mut batches = 0usize;
    let mut blocked_sessions = 0usize;
    let mut failure: Option<String> = None;
    let mut backed_off = false;

    for _ in 0..options.max_batches {
        // Cheap pre-check, so an exhausted budget does not buy another round of
        // selection and stamping. The binding one is taken just before the send.
        if TOTAL_BUDGET.checked_sub(started.elapsed()).is_none() {
            break;
        }
        let now = crate::now_ms();
        let base = binding.server_url.clone();
        let for_cost = binding.clone();
        let batch: Batch =
            queue.select_batch(MAX_BATCH_ITEMS, MAX_BATCH_BYTES, now, &deferred, |item| {
                item_cost(&base, &for_cost, item)
            })?;
        blocked_sessions = batch.blocked_sessions;
        if batch.items.is_empty() {
            break;
        }
        batches += 1;
        let keys: Vec<String> = batch.items.iter().map(|i| i.ingest_key.clone()).collect();
        // Durable, and before the request: a lost ack must not restart the
        // 30-day clock on the next run.
        queue.stamp_attempt(&keys, now)?;
        let items: Vec<BatchItem> = batch
            .items
            .iter()
            .map(|item| {
                Ok(BatchItem {
                    url: http::item_url(&binding.server_url, &binding, item),
                    body: serde_json::from_str(&item.body_json)
                        .context("stored body is no longer valid JSON")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // Selection checked the retry window once; a retry inside this batch can
        // still cross it. Carry the batch's tightest expiry into the sender so
        // every attempt is re-checked against it.
        //
        // Both clocks are read here, after stamping and JSON building: measuring
        // from a `now` captured before that preparation would hand the batch
        // however long the preparation took as extra allowance.
        let send_now = crate::now_ms();
        let ttl_left = batch
            .items
            .iter()
            .map(|item| ttl_remaining(item.first_attempt_ms.unwrap_or(now), send_now))
            .min()
            .unwrap_or(Duration::ZERO);
        let Some(remaining) = TOTAL_BUDGET.checked_sub(started.elapsed()) else {
            break;
        };
        let delivery = match send_with_retries(&sender, &items, remaining, ttl_left) {
            Ok(delivery) => delivery,
            Err(e) => {
                // Transport failure: everything stays pending, by definition.
                for item in &batch.items {
                    queue.record_failure(&item.ingest_key, "transport")?;
                }
                failure = Some(format!("{e} (all {} item(s) kept)", batch.items.len()));
                break;
            }
        };

        // An ack is only meaningful on 200 and 429. Every other status keeps the
        // whole batch, whatever its body claims.
        if !matches!(delivery.status, 200 | 429) {
            let detail = match delivery.status {
                401 | 403 => {
                    if authenticated {
                        "server rejected the bearer token (AI_MEMORY_AUTH_TOKEN)"
                    } else {
                        "server requires authentication; set AI_MEMORY_AUTH_TOKEN"
                    }
                }
                413 => {
                    "the server refused the batch as too large. The relay's own batch bounds \
                     are fixed; record smaller event bodies at the producer, or raise the \
                     server's body limit"
                }
                _ => "unexpected status",
            };
            for item in &batch.items {
                queue.record_failure(&item.ingest_key, &format!("http {}", delivery.status))?;
            }
            failure = Some(format!(
                "HTTP {} from /hook/batch: {detail}; all {} item(s) kept pending",
                delivery.status,
                batch.items.len()
            ));
            break;
        }

        let parsed: BatchAck = match serde_json::from_slice(&delivery.body) {
            Ok(parsed) => parsed,
            Err(_) => {
                for item in &batch.items {
                    queue.record_failure(&item.ingest_key, "malformed ack")?;
                }
                failure = Some(format!(
                    "HTTP {} from /hook/batch with an unreadable ack ({} byte(s)); \
                     all {} item(s) kept pending",
                    delivery.status,
                    delivery.body.len(),
                    batch.items.len()
                ));
                break;
            }
        };
        let accepted = match ack::validate(batch.items.len(), &parsed) {
            Ok(accepted) => accepted,
            Err(rejected) => {
                for item in &batch.items {
                    queue.record_failure(&item.ingest_key, "inconsistent ack")?;
                }
                failure = Some(format!(
                    "HTTP {} from /hook/batch with an inconsistent ack ({rejected}); \
                     all {} item(s) kept pending",
                    delivery.status,
                    batch.items.len()
                ));
                break;
            }
        };

        // Only now, after the whole response was validated, does anything leave
        // the queue.
        let confirmed: Vec<String> = accepted
            .iter()
            .filter_map(|idx| batch.items.get(*idx))
            .map(|item| item.ingest_key.clone())
            .collect();
        delivered += queue.confirm(&confirmed, crate::now_ms())?;

        if let Some(failed_index) = parsed.failed_index
            && let Some(item) = batch.items.get(failed_index)
        {
            queue.record_failure(&item.ingest_key, "server reported failed_index")?;
        }
        // The server stops at failed_index. Keep later, untried sessions
        // eligible; otherwise one failed head could block them on every flush.
        // Defer the failed head and earlier rate-limited items for this flush.
        for (position, item) in batch.items.iter().enumerate() {
            if accepted.contains(&position) {
                continue;
            }
            let untried = parsed.failed_index.is_some_and(|failed| position > failed);
            if !untried {
                deferred.insert(item.session());
            }
        }

        if delivery.status == 429 {
            backed_off = true;
            break;
        }
    }

    let stats = queue.stats(crate::now_ms())?;
    let mut summary = vec![format!(
        "acknowledged {delivered} event(s) in {batches} batch(es); {} still pending across {} session(s)",
        stats.pending_items, stats.pending_sessions
    )];
    if backed_off {
        summary.push(
            "server answered 429: the acknowledged items were applied and the flush stopped. \
             Back off before the next run"
                .to_owned(),
        );
    }
    if !deferred.is_empty() {
        summary.push(format!(
            "{} session(s) deferred to a later flush (a failed or rate-limited head); \
             their later events were never sent ahead of it",
            deferred.len()
        ));
    }
    if blocked_sessions > 0 {
        summary.push(format!(
            "{blocked_sessions} session(s) held: their oldest event passed the 30-day retry \
             window. The relay will not resend those automatically; they are retained for you"
        ));
    }
    summary.push(
        "acknowledged means delivered or dropped by server policy, not that an observation \
         was written"
            .to_owned(),
    );
    Ok(Report {
        summary,
        pending: stats.pending_items > 0,
        failure,
    })
}

/// How long an event may still be auto-resent, measured from its first durable
/// attempt. Zero means the 30-day window is spent.
///
/// Accuracy depends on a stable host clock: the relay carries no time source of
/// its own, so a clock that jumps stretches or shortens this measurement.
pub fn ttl_remaining(first_attempt_ms: i64, now_ms: i64) -> Duration {
    let spent = now_ms.saturating_sub(first_attempt_ms);
    let left = identity::RETRY_WINDOW_MS.saturating_sub(spent);
    Duration::from_millis(u64::try_from(left).unwrap_or(0))
}

/// Retry a batch a finite number of times under one shared deadline.
///
/// Two clocks bound it. `remaining` is the flush budget left when the batch
/// started; `ttl_left` is how long the batch's oldest item may still be resent.
/// Every request *and* every backoff sleep is drawn from the tighter of the two
/// and recomputed, so three attempts can neither overrun the budget nor put an
/// event on the wire after its retry window closed mid-flush.
fn send_with_retries(
    sender: &Sender,
    items: &[BatchItem],
    remaining: Duration,
    ttl_left: Duration,
) -> Result<http::Delivery> {
    let start = Instant::now();
    let deadline = start + remaining.min(ttl_left);
    let ttl_deadline = start + ttl_left;
    let mut attempt = 0usize;
    loop {
        if Instant::now() >= ttl_deadline {
            bail!(
                "hook batch not sent: the batch's oldest event reached the 30-day retry window \
                 mid-flush; it is retained, not resent"
            );
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("hook batch transport failure: flush budget exhausted");
        }
        match sender.post_batch(items, batch_timeout(items.len(), left)) {
            Ok(delivery) => return Ok(delivery),
            Err(e) => {
                if attempt >= TRANSPORT_RETRIES {
                    return Err(e);
                }
                let backoff = Duration::from_millis(250)
                    .saturating_mul(1u32 << attempt.min(8))
                    .min(MAX_BACKOFF)
                    .min(deadline.saturating_duration_since(Instant::now()));
                if backoff.is_zero() {
                    return Err(e);
                }
                attempt += 1;
                std::thread::sleep(backoff);
            }
        }
    }
}

/// Wire cost of one item, used for the batch byte budget.
pub fn item_cost(base: &str, binding: &Binding, item: &PendingItem) -> usize {
    http::item_url(base, binding, item).len() + item.body_json.len() + ITEM_FRAMING_BYTES
}

/// Per-event timeout scaled by item count, capped twice: by an absolute maximum
/// and by whatever is left of the flush budget. Mirrors core's spool drain.
pub fn batch_timeout(items: usize, remaining: Duration) -> Duration {
    let items = u32::try_from(items.max(1)).unwrap_or(u32::MAX);
    PER_EVENT_TIMEOUT
        .checked_mul(items)
        .unwrap_or(Duration::MAX)
        .min(MAX_BATCH_TIMEOUT)
        .min(remaining)
        .max(Duration::from_millis(1))
}
