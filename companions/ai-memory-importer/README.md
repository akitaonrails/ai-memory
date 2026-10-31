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

### agentmemory

Two routes read what agentmemory exports. Both write through the MCP
`memory_write_page` tool with explicit `workspace` and `project`, every write
conditional: `create_only` for a page's first version and `expected_page_id`
for each later one. They refuse a server older than ai-memory 3.0, which would
ignore those arguments. The client performs the MCP initialization handshake,
so these routes work with both the default stateless transport and
`serve --http-stateful`. Pages land under `--dest-prefix` (default
`imported/agentmemory`), never under ai-memory's own `sessions/` or any other
reserved family.

`agentmemory-vault --dir <vault>` reads the Obsidian vault agentmemory's
Obsidian export writes: `memories/`, `lessons/`, `crystals/` and `sessions/`,
each page with YAML frontmatter and `[[id]]` wikilinks. The generated `MOC.md`
index is skipped. A wikilink to another imported page is rewritten to its new
path; a link to anything else becomes plain text. `--workspace` and
`--project` name the one destination.

`agentmemory-export --file <export.json> --mapping <mapping.json>` reads the
`ExportData` JSON file agentmemory's export command writes. It takes a file
only: the importer never contacts an agentmemory server and never handles its
credentials. The mapping file is required, because agentmemory names a
project by its directory basename and two repositories can share one:

```json
{
  "projects": {
    "api": {"workspace": "acme", "project": "acme-api"},
    "web": {"workspace": "acme", "project": "acme-web"}
  },
  "cwds": {
    "/home/me/other/api": {"workspace": "other", "project": "other-api"}
  },
  "unscoped": {"workspace": "acme", "project": "acme-misc"}
}
```

`cwds` matches a session's exact working directory and wins over `projects`;
records tied to that session (its summary, the memories that cite it) follow
it. `unscoped` receives records that name no project. A record the mapping
does not cover is listed in the dry run, and `--apply` refuses to run until the
mapping covers it.

| agentmemory record | ai-memory page | `kind` / `tier` |
| --- | --- | --- |
| memory (every version) | `memories/<first-version-id>.md` | `preference`, `decision` (architecture), `workflow`, `pattern`, `bug`, `fact` / `semantic`, or `procedural` for workflow and pattern |
| lesson | `lessons/<id>.md` | `lesson` / `semantic` |
| insight | `insights/<id>.md` | `insight` / `semantic` |
| semantic memory | `semantic/<id>.md` | `fact` / `semantic` |
| procedural memory | `procedural/<id>.md` | `procedure` / `procedural` |
| crystal | `crystals/<id>.md` | `crystal` / `episodic` |
| session and its summary | `sessions/<id>.md` | `session` / `episodic` |

The paths are relative to `--dest-prefix`. A memory's versions are written
oldest first to one path, each conditional on the one before, so ai-memory
keeps its own supersession chain. `forgetAfter` becomes `expires_at`, so an
expired memory is hidden from normal reads as soon as it lands. Concepts and
tags become page tags, after the `agentmemory-import` tag every page carries.
Related records become wikilinks (qualified with `workspace/project:` when the
target landed in another project), or plain ids when they were not imported.
Deleted lessons and insights are skipped.

Compressed observations are dropped by default; the session summaries carry
their value. `--replay-observations` replays each session's observations
through `/hook/batch` as one `external-import` session (`source_event`
`agentmemory-observation`, at most 254 per session), so the server sanitizer
applies a second time. Graph nodes and edges, actions, signals, routines,
sentinels, sketches, checkpoints, facets, access logs and any key a newer
export adds are not imported; the manifest counts each under `dropped`. An
export page whose `pagination.hasMore` is true needs `--allow-partial`.

Claude memory graph and Qdrant imports remain roadmap items; there are no code
stubs for them.

### claude-mem SQLite

`claude-mem --mapping <mapping.json>` reads claude-mem's local SQLite database
(`~/.claude-mem/claude-mem.db`, or `--database <path>`). It imports only the
model-produced observations and session summaries. Raw tool calls and event
payloads are never read. Observation types `sensitive` and `security_alert`
are skipped and counted without copying their bodies into the manifest;
`security_note` is retained as an ai-memory `gotcha`.

The mapping format follows `agentmemory-export`: `projects` maps claude-mem's
project key, `cwds` can override it for schema generations that record
`sdk_sessions.cwd`, and `unscoped` is the optional fallback. A target contains
`workspace` and exactly one of `project` or stable `project_id`; the id form
selects an existing project without creating a same-name twin. Observation
pages land at `imported/claude-mem/<type>/<id>.md`; summaries use
`imported/claude-mem/session-summary/<id>.md`. The closed type set is
`decision`, `gotcha`, `security_note`, `feature`, `bugfix`, `change`,
`refactor`, and `discovery`. Unknown types are skipped and counted without
reading their bodies. `security_note` retains that source path and maps to the
`gotcha` kind; the five change/discovery types map to `fact` while retaining
their source type in the path.

The importer opens the source read-only and uses SQLite's backup API to take a
WAL-consistent private snapshot before inspection. It refuses a symlink or a
source whose identity changes during the snapshot. Live databases are
recognized from `schema_versions` and their actual `PRAGMA table_info`
columns: the legacy text and structured observation families are supported,
as are both the pre-v17 `sdk_session_id` and current `memory_session_id`
relationships. Mixed or incomplete layouts fail closed with a schema
diagnostic. A
source is capped at 2 GiB, each source text field at 1 MiB, and each supported
table at 100,000 rows. SQLite cells are capped at 8 MiB before allocation,
each final page stays within the 64,000-character exact-read ceiling, and all
planned page bodies together are capped at 512 MiB.

```bash
# Plan only; add --show-body for review.
ai-memory-importer claude-mem --mapping mapping.json

# Apply with a durable resume/conflict manifest.
ai-memory-importer claude-mem --mapping mapping.json \
  --apply --manifest-out claude-mem-import.json
```

### Local documents

`document --file <path>` extracts one explicitly selected local document into
one attributed page under `imported/documents/`. Supported inputs are UTF-8
text and Markdown, text-layer PDF, DOCX, PPTX (including speaker notes), XLSX,
WebVTT, and SRT. CSV and TSV are imported as text so their original delimiters
remain visible. Legacy XLS, XLSB, and ODS files must first be exported as XLSX
or delimiter-separated text.

The page records the source file's basename, format, SHA-256 digest, and
extractor version, followed by sanitized text. Its path includes the full
source digest, and its last line carries a deterministic provenance marker.
A rerun therefore performs no duplicate write. If another page already owns
that path, the import refuses to replace it. The rerun compares the whole
stored page, read through `memory_read_page`, which returns at most 64,000
characters of a body; a page longer than that cannot be compared, so its
rerun fails with that reason instead of being reported as present.

This is text extraction, not a media pipeline. Image-only PDFs need OCR before
import. Images, raw audio/video, and proprietary media transcription are not
supported; provide a completed VTT or SRT transcript instead. The original
opaque file is never uploaded or stored in ai-memory. DOCX text marked hidden
by formatting may still be extracted; deleted revisions and field instructions
are excluded. Review `--show-body` before every live import of a document whose
visibility rules matter.

DOCX and PPTX extraction preserves explicit run-level line breaks, including
DOCX carriage returns and formatted PPTX breaks, and DOCX tabs. Paired XML
start/end tags and self-closing tags produce the same separators.
Paragraph tab-stop definitions are formatting and do not insert text.

VTT/SRT files accept LF, CRLF, or CR line endings and any number of blank lines
between cues. WebVTT comments are omitted; identifiers such as `NOTE123` keep
their cue text. WebVTT character references such as `&amp;` and `&#233;` are
decoded once before sanitization. Escaped tags remain literal text, and SRT
character references retain their original spelling.

## Safety contract

- Default mode is dry-run; live mode requires `--apply`.
- Live mode requires explicit `--workspace`, `--project`, and
  `--manifest-out <path>`.
- The OMC wiki route writes only through `POST /admin/write-page`. No route
  opens ai-memory SQLite or wiki files directly or deletes pages.
- The destination workspace/project must already exist unless
  `--create-destination` is passed.
- Existing OMC destination pages abort the import unless `--overwrite` is
  passed. The importer also re-checks each page immediately before writing.
  This is best-effort protection: a concurrent writer could still race between
  the check and `/admin/write-page`, so avoid running competing import/write jobs
  into the same destination.
- The OMC route stops on the first live-write error and updates the manifest
  with completed writes and the failed checkpoint.
- Path handling fails closed: absolute paths, `..`, unsafe destination paths,
  and reserved/internal destination prefixes are rejected. Duplicate generated
  destination paths abort planning.
- Dry-run output does not print full page bodies unless `--show-body` is passed.
- The OMC route maps only endpoint-supported metadata: `title`, `kind`, `tier`,
  `tags`, `pinned`, and `body`. Unknown frontmatter is ignored.
- Manifests use randomized private temporary files in the destination directory,
  sync their contents, and atomically replace the previous manifest.
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
  The [shared private privacy helpers](../ai-memory-client) scrub nested values
  before event hashes, previews, manifests and delivery. Sensitive native source
  identities are refused before deriving an imported session ID or writing a
  manifest. Valid native session strings are preserved exactly. UTF-8 byte caps
  keep complete redaction markers, and numeric token metrics use an exact
  11-name allowlist. Body provenance remains untrusted and cannot supply actor
  or destination authority.
- Stable session IDs derive from `(workspace, project, source, session_id)`.
  Each replay event has a stable `ingest_key`; a changed transcript gets a new
  terminal generation key, so rerunning an interrupted import is safe and
  appending to an export re-runs SessionEnd after the new messages.
- A live conversation import is one ordered hook batch. If the server accepts
  only a prefix, the manifest is marked failed with its accepted count; rerun
  the same source to resume via the stable keys.
- There is no inbox or watch-folder mode. Import is an explicit one-shot
  operation.

The agentmemory routes keep the same dry-run default, `--apply` plus
`--manifest-out`, auth and path rules, with these differences:

- Writes use MCP `memory_write_page` only, each conditional, so the server
  refuses a page that appears or changes after the version read. A resume
  accepts an existing page only when its complete body matches a planned
  version; a different body stops the run and names the page.
- `--dest-prefix` must be relative and portable. `..`, absolute paths, a
  Windows drive, and a first segment that is reserved (`sessions`,
  `session-logs`, `procedures`, `decisions`, `gotchas`, `profile`) or starts
  with `_` or `.` are refused. Every planned path is checked to stay under the
  prefix, and two records that map to one path abort the plan.
- Vault folders and entries that are symlinks are refused; hidden entries and
  non-Markdown files are skipped and counted. Pages are capped at 1 MiB each
  and 100,000 per vault; an export file at 256 MiB.
- Every page and replayed observation is redacted client-side before the plan,
  the manifest or any request exists. Record ids that look like credentials
  are refused.
- Each record has a stable ingest key, a UUIDv5 of its agentmemory id, written
  on the page's last line. A rerun compares the entire sanitized planned body,
  including that line, with the page the server already holds before skipping
  it. `memory_read_page` returns at most 64,000 characters of a body, so a
  longer page cannot be compared and stops the run with that reason. Editing the body while keeping its attribution footer therefore cannot
  authorize a later imported version to replace the edit. Server-side
  sanitation that changes the body also stops automatic resumption. This
  comparison covers the body, not frontmatter-only edits; a later imported
  version still applies its planned metadata. An identical interrupted import
  resumes without duplicates and a finished one writes nothing. The manifest
  records each version as `written`, `present` (already there) or `failed`.
- Without `--create-destination`, every destination must already exist; the
  check is a read, which never creates a scope.

The claude-mem route submits bounded atomic page batches. A failed precondition
or rejected page changes none of that batch; successful batches create one
wiki checkpoint rather than one per page. It therefore requires a server that
advertises `memory_write_pages_batch`; the older agentmemory routes retain
their ai-memory 3.0 compatibility. The claude-mem manifest contains source
ids, destinations, counts, and hashes, but never imported bodies or skipped
sensitive/security bodies. Each imported page carries a deterministic
source-row key, while resume authority still requires an exact full-body
match; retaining the footer on an edited page does not permit overwrite.

The document route adds these boundaries:

- It accepts one explicit regular file on Unix, refuses a final-component
  symlink, opens with `O_NOFOLLOW`, and verifies the opened inode. The source is
  capped at 32 MiB. Document import currently fails closed on native Windows;
  use WSL2 until equivalent handle identity checks are implemented.
- Office ZIP containers are preflighted before extraction: at most 4,096
  entries, 16 MiB per entry, and 64 MiB claimed expanded size. XLSX extraction
  streams at most 64 sheets and 100,000 actual cells without materializing the
  worksheet's coordinate rectangle. PDF extraction is capped at 2,000 pages
  and 16 MiB of decompressed stream data per load/extraction operation.
- Working extracted text is capped at 2 MiB. The stored body is UTF-8-safely
  truncated to 1 MiB with an explicit marker, then passed through the shared
  client-side credential sanitizer before a manifest or request is created.
- Live writes use MCP `memory_write_page` with `create_only`. The server must
  support conditional writes, and a conflicting page counts as the same import
  only when its complete body equals the current sanitized plan. A provenance
  marker alone is never trusted. No document content grants authority or
  changes the destination scope.

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

## Document usage

Dry-run one document (the default):

```bash
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  document --file /path/to/report.pdf \
  --workspace default --project my-project
```

Review the sanitized extracted page body with `--show-body` before applying it,
especially for DOCX files that may contain text hidden by formatting:

```bash
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  document --file /path/to/report.pdf \
  --workspace default --project my-project \
  --apply --manifest-out /tmp/document-import-manifest.json
```

Use `--title` to override the displayed title and `--dest-prefix` to select a
different non-reserved imported-page family. The destination must already
exist unless `--create-destination` is passed.

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

## agentmemory usage

```bash
# Obsidian vault: dry run, then apply.
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  agentmemory-vault --dir ~/.agentmemory/vault --workspace default --project my-project
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  agentmemory-vault --dir ~/.agentmemory/vault --workspace default --project my-project \
  --apply --manifest-out /tmp/agentmemory-vault-manifest.json

# ExportData JSON: dry run (lists unmapped projects and dropped counts), then apply.
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  agentmemory-export --file agentmemory-export.json --mapping mapping.json
AI_MEMORY_AUTH_TOKEN=... \
cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
  agentmemory-export --file agentmemory-export.json --mapping mapping.json \
  --apply --manifest-out /tmp/agentmemory-export-manifest.json
```

Options shared by both: `--dest-prefix`, `--server-url`, `--create-destination`
and `--show-body`. `agentmemory-export` adds `--replay-observations` and
`--allow-partial`.

## Validation

Run these from the repository root:

```bash
cargo fmt --check --manifest-path companions/ai-memory-importer/Cargo.toml
cargo test --manifest-path companions/ai-memory-importer/Cargo.toml
cargo clippy --manifest-path companions/ai-memory-importer/Cargo.toml --all-targets -- -D warnings
```

The agentmemory end-to-end tests start a real `ai-memory serve` in a
temporary data directory when `AI_MEMORY_IMPORTER_E2E_BIN` names the binary
(CI sets it); without it they report a skip:

```bash
cargo build --bin ai-memory
AI_MEMORY_IMPORTER_E2E_BIN="$PWD/target/debug/ai-memory" \
  cargo test --manifest-path companions/ai-memory-importer/Cargo.toml e2e
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
