# Storage adapters

ai-memory persists two very different kinds of data:

- **Operational data** — sessions, observations, users, handoffs,
  embeddings, audit, schedulers. Always SQLite, never pluggable.
- **Page content** — the memory pages themselves (markdown body,
  frontmatter, links). The *source of truth* for this content is
  pluggable through a content-backend adapter.

The default backend, `sqlite`, is the historical pipeline: markdown
files under `<data_dir>/wiki/` (git-versioned, watched for external
edits) written atomically together with the SQLite index. In **every**
mode — default or adapter — the SQLite index keeps serving search
(FTS5), recency, the link graph, embeddings, and decay; adapters
replace only where the canonical page content lives.

## Selecting a backend

```toml
[storage]
backend = "sqlite"        # default — no section needed

# or, for example:
[storage]
backend = "outl"

[storage.outl]            # adapter-owned section, opaque to the core
workspace_dir = "~/notes"
slug_prefix = "ai-memory"
mode = "primary"          # "primary" | "shadow"
```

`backend` is a free-form name: `"sqlite"` short-circuits to the
built-in pipeline, anything else is looked up in the compiled-in
adapter registry. An unknown name fails at boot listing the available
adapters. Each adapter defines and validates its own
`[storage.<name>]` table — adding an adapter never requires touching
the core config code.

## Writing an adapter

Adapters live inside `ai-memory-store`, one module per adapter under
`crates/ai-memory-store/src/adapters/<name>/`, each gated by a cargo
feature (`adapter-<name>`) that carries its extra dependencies — so
consumers that only need the sqlite default never build them. The
contract lives in `ai-memory-store::content`:

1. **Implement [`ContentBackend`]** — seven content operations
   (`persist_page`, `persist_pages_batch`, `delete_page`,
   `remove_project`, `remove_workspace`, `move_project`, `read_page`)
   plus `fs_root()`. Implementations own their atomicity model: the fs
   backend compensates (file rollback when the index write fails);
   adapters with append-only stores are typically eventually
   consistent and heal the index with a background task. Return
   `fs_root() == None` when your SoT is not a markdown tree on disk —
   the engine then stands down the wiki watcher and git checkpoints.
2. **Implement [`ContentBackendFactory`]** — `name()` (the string
   operators put in `backend = "..."`; `"sqlite"` is reserved) and
   `build(AdapterContext)`. The context gives you the index writer +
   reader, the built-in fs backend (so shadow/mirror modes can wrap
   it), the operator home for `~` expansion, and your raw
   `[storage.<name>]` section as JSON. Return the backend plus any
   background task handles; the engine keeps them alive for the
   lifetime of `serve`.
3. **Register it** — declare the feature + optional deps in
   `crates/ai-memory-store/Cargo.toml`, add the `#[cfg]`-gated module,
   and list the factory in `adapters::registry`
   (`crates/ai-memory-store/src/adapters/mod.rs`).

The reference implementation is
`crates/ai-memory-store/src/adapters/outl` (feature `adapter-outl`,
enabled by the CLI): an embedded
[Outl](https://outl.app) workspace (append-only op-log CRDT) as the
content SoT, with a sha-stamped projection so its reconciler can tell
its own writes from edits the user makes inside the Outl app, and a
`shadow` mode that keeps the fs pipeline primary while mirroring
writes for migration validation.

## What adapters do NOT change

- Search ranking, recency, graph traversal, embeddings, decay — all
  index-side, identical across backends.
- Operational tables.
- The engine's orchestration (sanitizer, admission webhooks, title and
  link derivation, embedding computation) — adapters receive the fully
  orchestrated page.

## Known limitations (non-default backends)

- Git checkpoints / `restore-page` are a feature of the fs backend's
  markdown tree; backends without an fs root currently have no
  point-in-time restore.
- `ai-memory backup` does not yet include adapter-owned stores.
- Auto-improve approvals write through a specialised index transaction
  and are not yet routed through the adapter (the page lands in the
  index; the adapter copy catches up on the next engine write).
