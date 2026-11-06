# Data Handling

This page is for reviewers who need one place to answer "what data does
ai-memory collect, where does it live, and what (if anything) leaves the
machine", usually for a security, legal, or data-protection review ahead of
internal approval. It restates facts from [`SECURITY.md`](SECURITY.md), cited
by section below, alongside the operational docs it names; if the two ever
disagree, `SECURITY.md` is authoritative.

## Default: local, no telemetry, one model download

ai-memory does not phone home. There is no analytics, crash reporting, or
usage telemetry built into the binary, and no background update check.
Everything it stores (wiki pages and the SQLite database) lives under a
single, operator-controlled data directory on the local filesystem
(`SECURITY.md`, "Local data confidentiality"). No stored data leaves the
host unless one of the opt-in paths below is explicitly enabled. The
**embedding provider** is the one most likely to matter to a reviewer,
since it decides whether stored content reaches a cloud API at all.

The out-of-the-box default is local embeddings: the config template leaves
`embedding_provider` unset, which selects in-process `local` embeddings
(all-MiniLM-L6-v2, 384-dim) with no API key and no cloud calls. That
default carries one network behavior: on the first start, when the model
files are not already present, the server downloads three files (~87 MB
total, sha256-pinned in source) from `huggingface.co` in the background,
and hybrid search enables on the next restart. Nothing stored is sent; the
transfer is a download, so your host's address and the timing of the
request are visible to Hugging Face and its CDN, and no content goes with
them. To avoid the download entirely, set `embedding_provider = "none"`,
or pre-seed the files offline as
[`docs/airgapped-install.md`](docs/airgapped-install.md) and
[`docs/local-embeddings.md`](docs/local-embeddings.md) describe.

There is no encryption at rest in v1; ai-memory relies on OS filesystem
permissions (owner-only directories/files on Unix, ACLs on Windows;
`SECURITY.md`, same section).

## The opt-in paths that send data externally

Each is off by default and requires a deliberate config change or command
to turn on.

| path | what leaves the host |
|---|---|
| `embedding_provider = openai\|voyage\|google\|openai-compat\|copilot` | the text of every stored page |
| `llm_provider` (and `llm_fallbacks`), off by default | session observations and page text on consolidation, lint, bootstrap, dream, profile and auto-improve runs |
| `capture_assistant` (double opt-in) | the assistant's final-turn text |
| `AI_MEMORY_RERANKER=llm` | each live query + bounded page titles/snippets |
| `[[admission_webhooks]]` | every write's page path, frontmatter and body, POSTed to the URL you configured |

1. **Embedding provider.** This is the broadest path by far, and the one
   most relevant to a reviewer's usual question ("does our source
   material reach a cloud provider?"). `OpenAiEmbedder`, `VoyageEmbedder`,
   `GoogleEmbedder`, and `CopilotEmbedder` each call their provider's
   embedding endpoint for every page that needs a vector. This isn't limited
   to new writes: switching a running install from `local` to a cloud
   provider triggers a backfill of the **existing corpus** (`POST
   /admin/embed`, `ai-memory embed --force`), so changing this setting
   later can retroactively send content that was previously local-only.
   Leave `embedding_provider` on `local` (the default) to keep this off;
   see [`docs/local-embeddings.md`](docs/local-embeddings.md).
   `openai-compat` points at a self-hosted engine, so the destination is
   your own server.
2. **LLM provider.** Setting `llm_provider` (or `llm_fallbacks`) turns on
   the model-assisted pipeline: consolidation, lint, bootstrap, dream, the
   cross-project profile digest, and the auto-improvement loop each send
   session observations and page text to the configured provider and
   receive generated pages and proposals back. The `codex`, `copilot`, and
   `cursor` provider options spawn the vendor's own CLI, which talks to
   that vendor's cloud under your existing login. With no provider
   configured, every one of these features either runs rule-based locally
   or stays off; see `docs/ARCHITECTURE.md` ("Zero-LLM default path").
   Turning on `llm_request_metadata_headers` additionally sends, as
   `openai-compat` request headers, the workload name and, when an
   operation has exactly one, the workspace name, project name and
   username it acts on: identifiers, never page content
   ([`docs/llm-providers.md`](docs/llm-providers.md)).
3. **Assistant/Stop capture** (`SECURITY.md`, "Assistant/Stop capture is
   opt-in and sanitized"). Persisting the coding assistant's final-turn
   text requires a double opt-in: `capture_assistant` on the server *and*
   `install-hooks --capture-assistant` on the client. Once enabled,
   captured text rides in the session's automatic handoff to the next
   session and flows into consolidation/reviewer prompts, so it reaches a
   cloud provider only through path 2. The flag is global to the install;
   there is no per-project exclusion once it's on.
4. **LLM-based search reranking** (`SECURITY.md`, "Search reranking is an
   outbound-data opt-in"). Setting `AI_MEMORY_RERANKER=llm` sends each live
   query plus bounded page titles/snippets to the configured LLM provider.
   Leave this unset, or point it at a local provider, to keep queries and
   recalled snippets on-host.
5. **Admission webhooks.** Every wiki write (create, update, delete, and
   the handoff lifecycle events) runs the configured webhook chain: each
   `[[admission_webhooks]]` entry receives the page's path, frontmatter,
   and full body as a JSON POST, and may mutate or reject the write. The
   request carries only an `X-Memory-Op` header naming the operation; no
   credentials are attached. Deletes send the path with no body. The URL
   is one you configured, so this sends content to infrastructure you
   chose, over whatever TLS it has
   ([`docs/admission-webhooks.md`](docs/admission-webhooks.md)).

Paths 3 and 4 pass through a sanitizer that strips obvious credentials
before anything is transmitted. The sanitizer is documented as
best-effort, not a guarantee; see the "Stored-content prompt injection"
and reranking notes in `SECURITY.md` for the exact caveats. The embedding
path (1) sends full page text to the provider's embedding endpoint and is
not sanitized the same way, and neither are the LLM provider (2) and
admission webhook (5) payloads, so they are the paths to leave off when
stored content must not leave the server.

Two more network behaviors sit outside the table because they carry no
stored content:

- `ai-memory upgrade`, when you run it, queries `api.github.com` and
  downloads from `github.com`. Nothing checks for updates in the
  background.
- `auth login oidc-device` (and the `openai-oauth`, `anthropic-oauth`,
  `copilot` logins) talks to the issuer or vendor token endpoints during
  login and token refresh. These calls carry credentials, never memory
  content.

Finally, a topology choice rather than a config key: with a remote
`server_url`, the lifecycle hooks and the external relay ship every
captured event to the host you point them at
(`docs/external-lifecycle.md`). The data handling is everything on this
page, but the host, and whoever administers it, are now outside your
machine.

## Is this "personal data" under GDPR?

ai-memory stores developer-authored artifacts (wiki pages, session
handoffs, entity extractions) derived from coding-agent sessions. It has no
purpose-built handling for personal data (no user-profile fields, no
consent-tracking, no data-subject-request tooling) because it isn't designed
to process personal data as such. In practice this means:

- If your team's session content incidentally contains personal data (e.g. a
  commit message with a name, or free text a developer typed), that data is
  stored and retained exactly like any other memory content, under your own
  organization's control on infrastructure you run.
- ai-memory does not transmit that content anywhere on its own; the opt-in
  paths above are the only export mechanisms, and all are under your
  control.
- Treat this the same way you'd treat any local database your team
  operates: your organization is the data controller and decides retention,
  access, and disclosure. This document does not constitute legal advice;
  confirm classification with your own data-protection function.

## Processor relationship

Self-hosted with no opt-in features enabled: there is no third party in the
data path, so ai-memory does not create a data-processor relationship under
GDPR Art. 28. If you configure a cloud LLM or embedding provider (for core
functionality, capture, or reranking), **that provider** becomes your
processor for the data you send it, under whatever agreement you have with
them directly. ai-memory passes the data along and is not a party to that
relationship.

## Deletion / retention

Most data persists until removed, but two mechanisms expire some of it without
operator action, both on by default:

- **Decay eviction of cold episodic pages.** The daily forget sweep
  (`[maintenance] forget_sweep_interval_secs`, default 86400) tombstones
  episodic pages whose relevance score falls below `[decay] cold_threshold`
  (default 0.20, with an ~80-day survival floor) and hard-deletes those
  tombstones after `[decay] hard_delete_after_days` (default 180). Semantic,
  procedural and pinned pages are exempt, and raw observations are kept
  (`observation_retention_days = 0`).
- **TTL pages.** A page written with `expires_at` is hidden after its expiry
  and hard-deleted by the next forget sweep; a TTL outranks `pinned`.

Everything else persists until removed. Deletion is filesystem-level: removing
the data directory removes everything in it. For scoped deletion, a per-project
purge operation exists and is isolation-safe: it cannot delete files or rows
belonging to a different `(workspace_id, project_id)` (`SECURITY.md`,
"Per-project isolation").

## Network exposure (if you run the server non-loopback)

Binding the server beyond `127.0.0.1` is a separate, opt-in operational
choice unrelated to the data-export paths above. It controls who can reach
the API, not what ai-memory sends externally. See `SECURITY.md`'s "Network
exposure when binding to non-loopback addresses" section and
[`docs/https-via-proxy.md`](docs/https-via-proxy.md) before doing this.

## Related documents

- [`SECURITY.md`](SECURITY.md): threat model, scope, vulnerability reporting.
- [`docs/sso.md`](docs/sso.md): enterprise identity/SSO integration.
- [`docs/airgapped-install.md`](docs/airgapped-install.md): offline/air-gapped
  installation.
