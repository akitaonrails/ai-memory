//! Single-writer actor owning the embedded Outl [`Workspace`].
//!
//! `outl-actions` mutations take `&mut Workspace` and are synchronous,
//! so — mirroring the SQLite writer actor in `ai-memory-store` — one
//! dedicated OS thread owns the workspace (and its locks) for the
//! process lifetime, and async callers talk to it over a channel.
//!
//! The thread opens the workspace itself via [`outl_ws::open`], which
//! implements the multi-process contract: shared workspace lock plus a
//! per-actor write lock, falling back to an ephemeral actor when the
//! Outl app (TUI/desktop/MCP) already holds the config actor.

use std::path::PathBuf;
use std::sync::mpsc;

use outl_actions::BlockTreeSpec;
use outl_core::id::NodeId;
use outl_core::property::PropValue;
use outl_ws::WsCtx;
use tokio::sync::oneshot;

/// Property key carrying the sha256 of the projected page content, so
/// the reconciler can tell "our own projection" from an external edit.
pub const PROJECTED_SHA_PROP: &str = "ai-memory-sha";

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub(crate) enum Cmd {
    UpsertPage {
        slug: String,
        title: String,
        tier: String,
        specs: Vec<BlockTreeSpec>,
        projected_sha: String,
        reply: Reply<()>,
    },
    DeletePage {
        slug: String,
        reply: Reply<bool>,
    },
    /// Trash every ai-memory page whose slug starts with `prefix`.
    DeleteByPrefix {
        prefix: String,
        reply: Reply<usize>,
    },
    /// Slugs of every page under `prefix` (ai-memory-owned pages).
    ListOwned {
        prefix: String,
        reply: Reply<Vec<String>>,
    },
    /// Rendered body + stored projected-sha of the page at `slug`.
    /// `None` when the page vanished between list and read.
    ReadOwned {
        slug: String,
        reply: Reply<Option<OwnedContent>>,
    },
    /// Re-stamp the projected-sha prop after the reconciler absorbed an
    /// external edit into the index.
    MarkReconciled {
        slug: String,
        projected_sha: String,
        reply: Reply<()>,
    },
    /// Re-open the workspace when other actors' ops files changed on
    /// disk, so the materialized tree sees their writes. Replies `true`
    /// when a refresh happened.
    RefreshExternal {
        reply: Reply<bool>,
    },
}

/// Content of an ai-memory-owned page as read back from the workspace.
pub struct OwnedContent {
    /// Depth-first block texts joined into a document body.
    pub body: String,
    /// Title (from the `title::` prop, falling back to slug).
    pub title: String,
    /// `ai-memory-sha` prop value at read time, if any.
    pub stored_sha: Option<String>,
}

/// Cloneable handle to the workspace actor thread.
#[derive(Clone)]
pub struct OutlHandle {
    tx: mpsc::Sender<Cmd>,
}

/// Facts about the opened workspace, reported once at boot.
pub struct OpenInfo {
    /// Actor id this process writes under.
    pub actor: String,
    /// Whether the actor is ephemeral (another outl process holds the
    /// config actor — e.g. the TUI is open).
    pub ephemeral_actor: bool,
}

impl OutlHandle {
    /// Spawn the actor thread and open the workspace at `dir`.
    /// Fails (with the underlying open error) when `dir` is not an
    /// initialised Outl workspace.
    pub fn open(dir: PathBuf) -> Result<(Self, OpenInfo), String> {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let (boot_tx, boot_rx) = mpsc::channel::<Result<OpenInfo, String>>();

        std::thread::Builder::new()
            .name("outl-content-writer".into())
            .spawn(move || {
                let mut ctx = match outl_ws::open(&dir) {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        let _ = boot_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let _ = boot_tx.send(Ok(OpenInfo {
                    actor: ctx.actor.0.to_string(),
                    ephemeral_actor: ctx.ephemeral_actor,
                }));
                let mut external = snapshot_external_ops(&ctx);
                while let Ok(cmd) = rx.recv() {
                    // Refresh is handled in the loop because it consumes
                    // and replaces the whole context (the materialized
                    // tree only sees other actors' ops at boot — there is
                    // no incremental cursor API in outl-core yet).
                    if let Cmd::RefreshExternal { reply } = cmd {
                        let now = snapshot_external_ops(&ctx);
                        if now == external {
                            let _ = reply.send(Ok(false));
                            continue;
                        }
                        let root = ctx.root.clone();
                        // Drop releases the workspace + actor locks; the
                        // re-open re-acquires them (possibly landing on an
                        // ephemeral actor if another process grabbed ours
                        // in the window — contract-conformant).
                        drop(ctx);
                        ctx = match outl_ws::open(&root) {
                            Ok(new_ctx) => new_ctx,
                            Err(e) => {
                                tracing::error!(error = %e, "outl workspace re-open failed; writer thread exiting");
                                let _ = reply.send(Err(format!("re-open failed: {e}")));
                                return;
                            }
                        };
                        external = snapshot_external_ops(&ctx);
                        let _ = reply.send(Ok(true));
                        continue;
                    }
                    handle(&mut ctx, cmd);
                }
            })
            .map_err(|e| format!("spawning outl writer thread: {e}"))?;

        let info = boot_rx
            .recv()
            .map_err(|_| "outl writer thread died during boot".to_string())??;
        Ok((Self { tx }, info))
    }

    async fn call<T>(&self, build: impl FnOnce(Reply<T>) -> Cmd) -> Result<T, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(build(tx))
            .map_err(|_| "outl writer thread is gone".to_string())?;
        rx.await
            .map_err(|_| "outl writer thread dropped the reply".to_string())?
    }

    /// Raw SoT write: replace the page at `slug` with `specs`, stamping
    /// `projected_sha`. Exposed for migration tooling and tests; the
    /// normal path is [`super::OutlContentBackend::persist_page`].
    pub async fn upsert_page(
        &self,
        slug: String,
        title: String,
        tier: String,
        specs: Vec<BlockTreeSpec>,
        projected_sha: String,
    ) -> Result<(), String> {
        self.call(|reply| Cmd::UpsertPage {
            slug,
            title,
            tier,
            specs,
            projected_sha,
            reply,
        })
        .await
    }

    pub(crate) async fn delete_page(&self, slug: String) -> Result<bool, String> {
        self.call(|reply| Cmd::DeletePage { slug, reply }).await
    }

    pub(crate) async fn delete_by_prefix(&self, prefix: String) -> Result<usize, String> {
        self.call(|reply| Cmd::DeleteByPrefix { prefix, reply })
            .await
    }

    /// Slugs of every ai-memory-owned page under `prefix`.
    pub async fn list_owned(&self, prefix: String) -> Result<Vec<String>, String> {
        self.call(|reply| Cmd::ListOwned { prefix, reply }).await
    }

    /// Read one owned page back (rendered body + stored sha).
    pub async fn read_owned(&self, slug: String) -> Result<Option<OwnedContent>, String> {
        self.call(|reply| Cmd::ReadOwned { slug, reply }).await
    }

    /// Pick up writes other actors (the Outl app, a peer CLI) appended
    /// to the op log since our last look. Returns `true` when the
    /// workspace was re-opened.
    pub async fn refresh_external(&self) -> Result<bool, String> {
        self.call(|reply| Cmd::RefreshExternal { reply }).await
    }

    /// Update the projected-sha marker after reconciling an external edit.
    pub async fn mark_reconciled(&self, slug: String, projected_sha: String) -> Result<(), String> {
        self.call(|reply| Cmd::MarkReconciled {
            slug,
            projected_sha,
            reply,
        })
        .await
    }
}

fn handle(ctx: &mut WsCtx, cmd: Cmd) {
    match cmd {
        Cmd::UpsertPage {
            slug,
            title,
            tier,
            specs,
            projected_sha,
            reply,
        } => {
            let _ = reply.send(upsert_page(
                ctx,
                &slug,
                &title,
                &tier,
                &specs,
                &projected_sha,
            ));
        }
        Cmd::DeletePage { slug, reply } => {
            let _ = reply.send(delete_page(ctx, &slug));
        }
        Cmd::DeleteByPrefix { prefix, reply } => {
            let _ = reply.send(delete_by_prefix(ctx, &prefix));
        }
        Cmd::ListOwned { prefix, reply } => {
            let slugs = outl_actions::list_pages(&ctx.workspace)
                .into_iter()
                .map(|m| m.slug)
                .filter(|s| s.starts_with(&prefix))
                .collect();
            let _ = reply.send(Ok(slugs));
        }
        Cmd::ReadOwned { slug, reply } => {
            let _ = reply.send(Ok(read_owned(ctx, &slug)));
        }
        Cmd::MarkReconciled {
            slug,
            projected_sha,
            reply,
        } => {
            let result = match outl_actions::find_by_slug(&ctx.workspace, &slug) {
                Some(id) => set_text_prop(ctx, id, PROJECTED_SHA_PROP, &projected_sha),
                None => Ok(()),
            };
            let _ = reply.send(result);
        }
        // Intercepted by the thread loop (needs to replace the context).
        Cmd::RefreshExternal { reply } => {
            let _ = reply.send(Err("RefreshExternal must be handled by the loop".into()));
        }
    }
}

/// Sizes of every ops file owned by OTHER actors. A change means
/// another process (Outl app, peer CLI) wrote ops we have not seen.
fn snapshot_external_ops(ctx: &WsCtx) -> std::collections::BTreeMap<String, u64> {
    let own = format!("ops-{}.jsonl", ctx.actor.0);
    let mut out = std::collections::BTreeMap::new();
    let Ok(dir) = std::fs::read_dir(&ctx.paths.ops) else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("ops-") || !name.ends_with(".jsonl") || name == own {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.insert(name, len);
    }
    out
}

/// Run `f` with the workspace in batch mode: every op it applies is
/// buffered and flushed with ONE fsync at the end (outl-core's manual
/// `enter_batch`/`end_batch` pair — `begin_batch`'s RAII guard borrows
/// the workspace and would conflict with the `&mut WsCtx` the action
/// helpers take). `f` must not panic past `enter_batch`: our closures
/// only build ops and return `Result`s.
fn batched<T>(
    ctx: &mut WsCtx,
    f: impl FnOnce(&mut WsCtx) -> Result<T, String>,
) -> Result<T, String> {
    ctx.workspace.enter_batch();
    let result = f(ctx);
    let flush = ctx
        .workspace
        .end_batch()
        .map_err(|e| format!("batch flush: {e}"));
    // An action error wins (the flush of the applied prefix still ran);
    // a clean run with a failed flush is a persistence error.
    result.and_then(|v| flush.map(|()| v))
}

fn set_text_prop(ctx: &mut WsCtx, node: NodeId, key: &str, value: &str) -> Result<(), String> {
    outl_actions::set_property(
        &mut ctx.workspace,
        &ctx.hlc,
        node,
        key,
        Some(PropValue::Text(value.to_string())),
    )
    .map_err(|e| format!("set {key}: {e}"))
}

fn upsert_page(
    ctx: &mut WsCtx,
    slug: &str,
    title: &str,
    tier: &str,
    specs: &[BlockTreeSpec],
    projected_sha: &str,
) -> Result<(), String> {
    // One batch = one fsync for the whole composite mutation (page
    // create + clear + forest + props) instead of one per action.
    let id = batched(ctx, |ctx| {
        let id = outl_actions::open_or_create_page(
            &mut ctx.workspace,
            &ctx.hlc,
            slug,
            title,
            outl_actions::PageKind::Page,
        )
        .map_err(|e| format!("open_or_create `{slug}`: {e}"))?;

        // Full-content replace: trash the previous projection's blocks,
        // then append the new forest. Op-log churn is proportional to page
        // size, matching the fs backend's whole-file rewrite semantics.
        let children: Vec<NodeId> = outl_actions::children_of(&ctx.workspace, id)
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        for child in children {
            outl_actions::delete(&mut ctx.workspace, &ctx.hlc, child)
                .map_err(|e| format!("clear `{slug}`: {e}"))?;
        }
        outl_actions::append_forest(&mut ctx.workspace, &ctx.hlc, id, specs)
            .map_err(|e| format!("append content `{slug}`: {e}"))?;

        set_text_prop(ctx, id, "title", title)?;
        set_text_prop(ctx, id, "tier", tier)?;
        set_text_prop(ctx, id, PROJECTED_SHA_PROP, projected_sha)?;
        Ok(id)
    })?;

    // Keep the on-disk .md projection fresh so external editors and
    // the user see the page immediately. Projection failures don't
    // fail the write — the op log is the source of truth and `outl
    // doctor` / reconcile can rebuild projections.
    if let Err(e) = outl_actions::apply_page_md_with_sidecar(&ctx.workspace, &ctx.root, id) {
        tracing::warn!(slug, error = %e, "outl: page written but .md projection failed");
    }
    Ok(())
}

fn delete_page(ctx: &mut WsCtx, slug: &str) -> Result<bool, String> {
    if outl_actions::find_by_slug(&ctx.workspace, slug).is_none() {
        return Ok(false);
    }
    let meta = outl_actions::delete_page(&mut ctx.workspace, &ctx.hlc, slug)
        .map_err(|e| format!("delete `{slug}`: {e}"))?;
    if let Err(e) = outl_actions::remove_page_projection(&ctx.root, &meta) {
        tracing::warn!(slug, error = %e, "outl: page trashed but .md projection removal failed");
    }
    Ok(true)
}

fn delete_by_prefix(ctx: &mut WsCtx, prefix: &str) -> Result<usize, String> {
    let slugs: Vec<String> = outl_actions::list_pages(&ctx.workspace)
        .into_iter()
        .map(|m| m.slug)
        .filter(|s| s.starts_with(prefix))
        .collect();
    // One flush for the whole prefix sweep instead of one per page.
    batched(ctx, |ctx| {
        let mut removed = 0;
        for slug in slugs {
            if delete_page(ctx, &slug)? {
                removed += 1;
            }
        }
        Ok(removed)
    })
}

fn read_owned(ctx: &mut WsCtx, slug: &str) -> Option<OwnedContent> {
    let id = outl_actions::find_by_slug(&ctx.workspace, slug)?;
    let mut texts: Vec<String> = Vec::new();
    collect_texts(ctx, id, &mut texts);
    let title = outl_actions::read_text_prop(&ctx.workspace, id, "title")
        .unwrap_or_else(|| slug.to_string());
    let stored_sha = outl_actions::read_text_prop(&ctx.workspace, id, PROJECTED_SHA_PROP);
    Some(OwnedContent {
        body: super::project::texts_to_body(&texts),
        title,
        stored_sha,
    })
}

fn collect_texts(ctx: &WsCtx, parent: NodeId, out: &mut Vec<String>) {
    for (child, _) in outl_actions::children_of(&ctx.workspace, parent) {
        if let Some(text) = ctx.workspace.block_text(child) {
            out.push(text);
        }
        collect_texts(ctx, child, out);
    }
}
