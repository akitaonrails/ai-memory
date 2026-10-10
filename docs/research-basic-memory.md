# basic-memory - Research Report

> **Follow-up:** the September 2026 landscape refresh (how this project
> evolved, what the field looks like now, and what we concluded) is in
> [`research-2026-landscape.md`](research-2026-landscape.md).


> Source project: `basicmachines-co/basic-memory` (Python, MCP-native, markdown-on-disk).
> Studied as inspiration *and* as the manual-write-note model we explicitly
> diverge from.

## 1. Purpose & Scope

Basic Memory is a **local-first, MCP-native, Markdown-based personal knowledge graph**. Its tagline is "Your AI never forgets again": notes live as plain Markdown files on disk, both humans (in Obsidian, VS Code) and LLMs (over MCP) read and write them, and a SQLite/Postgres index keeps a knowledge graph in sync.

**The model is explicitly *manual*.** The user (or, more often, the agent in response to the user) calls `write_note`, `edit_note`, `move_note`, etc. There is **no implicit capture** of conversation content. The "How it works" example in `README.md:317-356` is literally *"Ask the LLM to capture it: 'Make a note on coffee brewing methods.'"* The README even prescribes user prompts ("Create a note about our project architecture decisions") as the activation step. Nothing in the codebase auto-saves conversation turns. The closest thing to automation is the `continue_conversation` *prompt* (`src/basic_memory/mcp/prompts/continue_conversation.py:18-90`), which only *retrieves*: it asks the model to search recent activity and load context; it never writes.

## 2. Storage Model

**Both Markdown and SQL, with the files being source-of-truth.** Each note is one Markdown file with YAML frontmatter:

```yaml
---
title: Coffee Brewing Methods
type: note
permalink: coffee-brewing-methods
tags: [coffee, brewing]
---
```

The grammar is documented in `docs/NOTE-FORMAT.md` and parsed at `src/basic_memory/markdown/entity_parser.py:1-27` using `markdown-it` plus custom `observation_plugin` and `relation_plugin`. Three semantic primitives:

- **Entity** (one per file): `src/basic_memory/models/knowledge.py:28-149`. Holds `title`, `note_type`, `permalink`, `file_path`, `checksum`, `mtime`, `size`, `entity_metadata` (JSON for custom frontmatter), `external_id` (stable UUID).
- **Observation**: `- [category] text #tag (context)` lines, indexed in the `observation` table (`knowledge.py:220-263`).
- **Relation**: `- relation_type [[Other Entity]]` lines, indexed in `relation` (`knowledge.py:265-311`). Bare `[[X]]` becomes `links_to`. Relations can be *unresolved* (`to_id` NULL until the target exists), then auto-resolved on sync.

**Search** is dual-stack and selected by `database_backend` config (`src/basic_memory/config.py:222-226`):

- **SQLite**: FTS5 virtual table `search_index` with custom tokenizer `'unicode61 tokenchars 0x2F'` so `/` is searchable (`src/basic_memory/models/search.py:62-94`). Semantic vectors via `sqlite-vec` virtual table `search_vector_embeddings` (`search.py:146-153`).
- **Postgres**: real `search_index` table with `tsvector` GIN + `pgvector` (`search.py:17-58`) and `pg_trgm` for fuzzy link resolution (migration `f8a9b2c3d4e5`).

Hybrid search defaults: vector candidates `semantic_vector_k=100`, similarity threshold `0.55`, model `bge-small-en-v1.5` via FastEmbed (`config.py:233-313`). Search type is `hybrid` when semantic is on, else `text` (`config.py:313-318`).

There is also a `NoteContent` table (`knowledge.py:152-217`) that **materializes the markdown body in the DB** with a `file_write_status` state machine (`pending|writing|synced|failed|external_change_detected`) and `db_version`/`file_version` for conflict resolution between AI writes and human file edits.

## 3. MCP Tools Exposed

All tools are registered via the `@mcp.tool` decorator and exported from `src/basic_memory/mcp/tools/__init__.py:9-65`. Every tool is annotated with MCP `readOnlyHint`/`destructiveHint`/`idempotentHint`/`openWorldHint` so agents can pick safely. Every single tool requires **explicit invocation**; none are triggered by the server itself.

| Tool | Hint | File |
|---|---|---|
| `write_note` | destructive, not idempotent | `tools/write_note.py:21` |
| `edit_note` (append/prepend/find_replace/replace_section/insert_*) | not destructive | `tools/edit_note.py:225` |
| `read_note`, `view_note`, `read_content` | read-only | `tools/read_note.py:64`, `view_note.py:13`, `read_content.py:157` |
| `delete_note` | destructive | `tools/delete_note.py:184` |
| `move_note` | not destructive (updates links) | `tools/move_note.py:346` |
| `search_notes` (advanced: tags, status, metadata_filters, after_date, search_type=text/vector/hybrid) | read-only | `tools/search.py:564` |
| `recent_activity` | read-only | `tools/recent_activity.py:28` |
| `build_context` (resolves `memory://` URIs, walks relation graph N hops, `depth=1..3`) | read-only | `tools/build_context.py:114` |
| `list_directory`, `canvas` (Obsidian canvas), `list_workspaces` | mixed | various |
| `list_memory_projects`, `create_memory_project`, `delete_project` | mixed | `tools/project_management.py` |
| `schema_infer`, `schema_validate`, `schema_diff` (Picoschema over frontmatter) | read-only | `tools/schema.py:206-440` |
| ChatGPT-compat `search` / `fetch` | read-only | `tools/chatgpt_tools.py:107-171` |
| `cloud_info`, `release_notes` | read-only | |

Two MCP **prompts** ship: `continue_conversation` and `recent_activity` (both retrieve-only). There is also a `view_note` UI artifact path.

## 4. Memory Lifecycle

**There is no lifecycle.** Grep'd the entire `src/` tree for `decay|aging|consolidat|summariz|forget|expire|ttl|prune|archive_old` - zero matches outside of unrelated OAuth-token expiry and SQLAlchemy `expire_on_commit`. Notes are **append-only forever** unless a human or agent explicitly calls `delete_note`, `move_note`, or `edit_note`. There is no auto-summarization, no auto-merge of duplicates, no recency weighting in search ranking (only an `after_date` filter), no "cold storage" tier. `recent_activity` (`tools/recent_activity.py:28`) just queries by `created_at`/`updated_at`; it doesn't shape memory.

The only background process is the `WatchService` (`src/basic_memory/sync/watch_service.py:81-145`) + `SyncService` (`src/basic_memory/sync/sync_service.py:153-188`), which reconcile file changes with the DB after a `sync_delay` of 1000 ms (`config.py:338`). That's housekeeping, not lifecycle.

## 5. Cross-Project / Cross-Agent

Projects are a first-class concept. Config holds a `Dict[str, ProjectEntry]` (`config.py:184-195`), each with a `path`, a `mode` (`LOCAL` or `CLOUD`, for per-project routing), optional `workspace_id`, and bisync state. A `default_project` is auto-set to the first project (`config.py:705-711`).

**Project resolution is a unified three-tier chain** (`docs/ARCHITECTURE.md:298-324`): explicit `project` argument → default → single-project fallback. Every MCP tool accepts `project` and `project_id` (UUID) parameters. `get_project_client(project, ...)` (`mcp/project_context.py`) routes to local ASGI or cloud HTTP per-project, so you can mix.

For agent handoffs, there is no special handshake; basic-memory treats all MCP clients identically. The "handoff" story is: agent A writes notes to project `foo`, agent B (any other MCP client pointed at the same project) reads them. The Markdown files on disk are the lingua franca. There is no session/agent identifier, no per-agent scratchpad. `created_by`/`last_updated_by` columns exist (`knowledge.py:99-102`) but are populated only in cloud (user_profile_id), null for local/CLI.

## 6. Backup & Portability

- **Default location**: `~/basic-memory` for notes (per `BASIC_MEMORY_HOME` env, README:197), `~/.basic-memory/` for SQLite DB + config (`config.py:24, 67-80`). Config in `~/.basic-memory/config.json`, chmod 0600.
- **Portability**: Excellent for files (they're just Markdown, so `git clone`, `rsync`, Syncthing, and rclone all work). The DB is a derived index; `bm sync` rebuilds it from the files.
- **Schema migrations**: 22 Alembic migrations in `src/basic_memory/alembic/versions/`, auto-run on startup via `get_or_create_db` (`services/initialization.py:23-38`). Migrations cover both SQLite and Postgres.
- **Importers** for Claude conversations, ChatGPT exports, and `memory.json` (the original MCP "memory" server format) live in `src/basic_memory/importers/`.
- **Cloud sync** uses `rclone bisync` (`config.py:136-138, 164-167`), not a custom protocol, which is another portable choice.

## 7. Strengths Worth Borrowing

1. **Files are source of truth, DB is derived index.** Survives any DB corruption, plays nice with git, version control, and grep. Bidirectional human/AI editing via file-watcher + checksum.
2. **MCP behavior annotations on every tool** (`readOnlyHint`, `destructiveHint`, etc., `tools/write_note.py:23`). Agents can plan multi-step actions without trial-and-error.
3. **Aggressive `AliasChoices` aliasing** of parameter names (`write_note.py:31` accepts `directory|folder|dir|path`; `search.py:574` accepts `query|q|search|text`). LLMs use whatever name their training reaches for; the tool absorbs the variance.
4. **`memory://` URI scheme + `build_context` graph walker** (`tools/build_context.py:114-247`) turns wiki-links into navigable context. Cleaner than dumping the whole graph.
5. **Unresolved relations as first-class state** (`knowledge.py:282`): `to_id` is nullable and resolves later when target appears. Forward references just work.
6. **Per-project routing** with mixed local/cloud modes per project, not per-server.
7. **Composition root + typed-client pattern** (`docs/ARCHITECTURE.md:14-256`) keeps MCP/CLI/API entrypoints clean and testable.
8. **The `NoteContent` table** with `file_write_status` state machine (`knowledge.py:155-217`) handles the race between agent writes and on-disk human edits. Worth replicating if you keep a DB cache.

## 8. Weaknesses / Friction (Avoid)

1. **The manual `write_note` ceremony is the headline friction.** Users must explicitly tell the model "make a note about this," and the model must decide *title*, *directory*, *tags*, *note_type*, and the semantic observation/relation grammar, all on every call. The `write_note` signature has 11 parameters (`write_note.py:25-45`). Skip this entirely: capture should be ambient (post-turn summarization, automatic salience scoring, etc.) instead of a tool the model must remember to call.
2. **Append-only forever.** No decay, no consolidation, no automatic deduplication. Long-running graphs accumulate cruft. Recent v0.20 added a guard so `write_note` *errors* on conflict instead of silently upserting (`write_note.py:240-262`), which protects data but pushes the burden back onto the agent to manage identity. For agent long-term memory, decay/merge/consolidation is essential and absent here.
3. **The semantic grammar is human-authored convention.** `- [category] text #tag (context)` and `- relation_type [[Target]]` are intuitive for humans in Obsidian but require the LLM to *generate* this format correctly every time. Drift is normal. A Rust server can store edges natively and let the LLM emit prose.
4. **Note identity is brittle.** `permalink` is derived from title/path; renames create work (`update_permalinks_on_move` defaults to `False`, `config.py:349-352`). The 11-column `entity` table + separate `note_content` is a lot of machinery to keep in sync with files.
5. **No agent/session model.** No notion of "who wrote this," "which session," or "what was the user's intent." `created_by` exists only for cloud auth (`knowledge.py:99-102`). For multi-agent handoffs, you'd want provenance baked in.
6. **Search ranking is keyword-or-vector with a fixed `min_similarity=0.55`** (`config.py:307-312`). No recency/importance reranking, no usage feedback, no per-query learning.
7. **The Markdown source-of-truth tradeoff.** Filesystem latency, checksum recomputation, FTS5 rebuilds, and circuit-breaker retry tracking (`sync_service.py:179-281`) are a lot of infrastructure just to keep a DB consistent with files. For agent memory where the files exist only because LLMs wrote them, this is overhead with no payoff. Keep the DB as primary and only export Markdown on demand.
8. **Tool surface is wide (~25 tools).** Agents have to pick among `write_note`/`edit_note`/`move_note`/`delete_note`/`read_note`/`view_note`/`read_content`/`search`/`search_notes`/`build_context`/`recent_activity`/`list_directory`/`list_memory_projects`/`canvas`/`schema_*`. Even with hints, that's a lot of context burned describing tools. Aim for a smaller, more orthogonal set.

**Bottom line**: borrow the *graph + observations + wiki-link* primitives, the *memory:// URI* navigation, the *typed-client + composition-root* layering, the *MCP annotations*, and the *file-as-portable-export* idea, but invert the capture model (ambient, not invoked), add a lifecycle (decay/consolidation/summarization), keep the DB primary, add agent/session provenance, and ship a much narrower MCP tool surface.
