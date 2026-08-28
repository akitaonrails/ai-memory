# ai-memory-importer

Standalone optional companion for importing external memory corpora into a
running ai-memory server. This crate is deliberately isolated from the root
workspace: its `Cargo.toml` has its own `[workspace]`, uses only crates.io
dependencies, and is not included in root `cargo test --workspace`.

## Supported sources

### OMC wiki directory

The original importer supports oh-my-claudecode / OMC flat Markdown wiki
directories. It reads only top-level `*.md` files, skips `index.md` and
`session-log-*` by default, and writes deterministic destination paths under
`omc/<slug>.md`.

### Generic external conversations

`external-conversation` accepts one deliberately small interchange format:

```json
{
  "project": "my-project",
  "source": "chatgpt",
  "session_id": "exported-conversation-id",
  "messages": [
    { "role": "user", "content": "What evidence supports this claim?" },
    { "role": "assistant", "content": "The source supports only part of it." }
  ]
}
```

Roles are `system`, `user`, or `assistant`. Product-specific ChatGPT, Claude
Desktop, Markdown, or other export adapters intentionally stay outside this
repository; they only need to emit the generic envelope.

The importer replays the conversation through ai-memory's public hook pipeline:
one `session-start`, the ordered messages, and one `session-end`. User messages
use the canonical `user-prompt` event. Assistant/system messages use the
validated `ai-memory-importer` extension vocabulary so their role and body stay
available to later consolidation. The whole sequence uses `/hook/batch`, whose
inline processing preserves order and does not return until the session page
and other SessionEnd effects have landed.

Imported sessions use the dedicated `agent=external-import` wire identity,
never a live `codex` or `claude-code` identity. Core's tolerant unknown-agent
boundary stores it in the closed `other` bucket, while the SessionStart
observation names `external:<source>` and assistant/system observations retain
extension provenance. A reader can therefore distinguish an explicit import
and its source without adding product-specific agent kinds to core.

Claude memory graph and Qdrant imports remain roadmap items; there are no code
stubs for them.

## Safety contract

- Default mode is dry-run; live mode requires `--apply`.
- Live mode requires explicit `--workspace`, `--project`, and
  `--manifest-out <path>`.
- Live writes use only `POST /admin/write-page`; the importer never opens
  ai-memory SQLite or wiki files directly and never deletes pages.
- The destination workspace/project must already exist unless
  `--create-destination` is passed.
- Existing destination pages abort the import unless `--overwrite` is passed.
  The importer also re-checks each page immediately before writing.
  This is best-effort protection: a concurrent writer could still race between
  the check and `/admin/write-page`, so avoid running competing import/write jobs
  into the same destination.
- It stops on the first live-write error and updates the manifest with completed
  writes and the failed checkpoint.
- Path handling fails closed: absolute paths, `..`, unsafe destination paths,
  and reserved/internal destination prefixes are rejected. Duplicate generated
  destination paths abort planning.
- Dry-run output does not print full page bodies unless `--show-body` is passed.
- Only endpoint-supported metadata is mapped: `title`, `kind`, `tier`, `tags`,
  `pinned`, and `body`. Unknown frontmatter is ignored.
- Auth comes only from `AI_MEMORY_AUTH_TOKEN`; there is intentionally no CLI
  token argument.
- External conversations are fully parsed, schema-checked, bounded, and
  sanitized before any manifest or HTTP request is created. Unknown fields and
  roles fail closed. Limits are 2 MiB per file, 128 messages, and 1 MiB total
  message content. Oversized individual messages are UTF-8-safely truncated to
  the same durable hook caps: 16 KiB for user prompts and 2,000 bytes for
  extension messages. The dry-run and manifest report the truncation count.
- Conversation bodies receive client-side credential redaction before replay
  and then cross ai-memory's normal server sanitizer as a second boundary.
- Stable session IDs derive from `(workspace, project, source, session_id)`.
  Each replay event has a stable `ingest_key`; a changed transcript gets a new
  terminal generation key, so rerunning an interrupted import is safe and
  appending to an export re-runs SessionEnd after the new messages.
- A live conversation import is one ordered hook batch. If the server accepts
  only a prefix, the manifest is marked failed with its accepted count; rerun
  the same source to resume via the stable keys.
- There is no inbox or watch-folder mode. Import is an explicit one-shot
  operation.

## Usage

Dry-run with a summary:

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project
```

Dry-run with a manifest:

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --manifest-out /tmp/omc-import-manifest.json
```

Live import:

```bash
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  omc-wiki --dir /path/to/omc/wiki --workspace default --project my-project \
  --apply --manifest-out /tmp/omc-import-manifest.json
```

Options:

- `--server-url URL`: ai-memory server URL; defaults to
  `http://127.0.0.1:49374`, or `AI_MEMORY_SERVER_URL` when set. A URL path is
  treated as the base path.
- `--create-destination`: allow `/admin/write-page` to auto-create the
  workspace/project after the read preflight fails.
- `--overwrite`: replace existing destination pages.
- `--include-session-logs`: include `session-log-*` pages.
- `--show-body`: print full page bodies during dry-run.
- `--pinned`: pin all imported pages.

## External conversation usage

Dry-run (the default):

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  external-conversation --file /path/to/conversation.json \
  --workspace default
```

Show the sanitized event bodies in the dry-run:

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  external-conversation --file /path/to/conversation.json \
  --workspace default --show-body
```

Live replay:

```bash
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  external-conversation --file /path/to/conversation.json \
  --workspace default --apply \
  --manifest-out /tmp/conversation-import-manifest.json
```

The envelope's `project` and the CLI's `--workspace` are both explicit. The
destination must already exist unless `--create-destination` is passed. Source
files are read once; adapters should write their completed generic JSON export
before invoking the importer.

## Validation

Run these from the repository root:

```bash
cargo fmt --check --manifest-path companions/ai-memory-importer/Cargo.toml
cargo test --manifest-path companions/ai-memory-importer/Cargo.toml
cargo clippy --manifest-path companions/ai-memory-importer/Cargo.toml --all-targets -- -D warnings
```

Root hygiene checks remain separate:

```bash
cargo fmt --check
git diff --check
```

## Roadmap

- Claude Code memory graph export import.
- Qdrant collection import with user-supplied schema mapping.
- Optional deterministic normalization passes after OMC import is stable.
