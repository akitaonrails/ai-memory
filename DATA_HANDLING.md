# Data Handling

This page exists for reviewers who need a single place to answer "what data
does ai-memory collect, where does it live, and what (if anything) leaves the
machine" — typically a security, legal, or data-protection review ahead of
internal approval. It restates facts that already live in
[`SECURITY.md`](SECURITY.md), cited by section below, rather than making new
claims; if the two ever disagree, `SECURITY.md` is authoritative.

## Default: fully local, no telemetry

ai-memory does not phone home. There is no analytics, crash reporting, or
usage telemetry built into the binary. Everything it stores — wiki pages and
the SQLite database — lives under a single, operator-controlled data
directory on the local filesystem (`SECURITY.md`, "Local data
confidentiality"). No data leaves the host unless one of the three opt-in
paths below is explicitly enabled — the **embedding provider** being the one
most likely to matter to a reviewer, since it's the default-on question of
whether stored content reaches a cloud API at all.

The out-of-the-box default is local-only: `config.default.toml` configures
in-process local embeddings (all-MiniLM-L6-v2, 384-dim) with no API key and
no data egress. See [`docs/local-embeddings.md`](docs/local-embeddings.md).

There is no encryption at rest in v1; ai-memory relies on OS filesystem
permissions (owner-only directories/files on Unix, ACLs on Windows —
`SECURITY.md`, same section).

## The three opt-in paths that send data externally

All three are off by default (local embeddings need no key or network call)
and each requires a deliberate config change to turn on.

| path | what leaves the host |
|---|---|
| `embedding_provider = openai\|voyage\|google\|openai-compat` | the text of every stored page |
| `capture_assistant` (double opt-in) | the assistant's final-turn text |
| `AI_MEMORY_RERANKER=llm` | each live query + bounded page titles/snippets |

1. **Embedding provider.** This is the broadest path by far, and the one
   most likely to answer a reviewer's actual question ("does our source
   material reach a cloud provider?"). `OpenAiEmbedder`, `VoyageEmbedder`,
   and `GoogleEmbedder` each call their provider's embedding endpoint for
   every page that needs a vector. This isn't limited to new writes:
   switching a running install from `local` to a cloud provider triggers a
   backfill of the **existing corpus** (`POST /admin/embed`, `ai-memory embed
   --force`), so changing this setting later can retroactively send content
   that was previously local-only. Leave `embedding_provider` on `local` (the
   default) to keep this off; see
   [`docs/local-embeddings.md`](docs/local-embeddings.md).
2. **Assistant/Stop capture** (`SECURITY.md`, "Assistant/Stop capture is
   opt-in and sanitized"). Persisting the coding assistant's final-turn text
   requires a double opt-in: `capture_assistant` on the server *and*
   `install-hooks --capture-assistant` on the client. Once enabled, captured
   text rides in the session's automatic handoff to the next session and
   flows into consolidation/reviewer prompts and — only if you have
   separately configured a cloud LLM provider — is sent to that provider. The
   flag is global to the install; there is no per-project exclusion once it's
   on.
3. **LLM-based search reranking** (`SECURITY.md`, "Search reranking is an
   outbound-data opt-in"). Setting `AI_MEMORY_RERANKER=llm` sends each live
   query plus bounded page titles/snippets to the configured LLM provider.
   Leave this unset, or point it at a local provider, to keep queries and
   recalled snippets on-host.

Paths 2 and 3 pass through a sanitizer that strips obvious credentials before
anything is transmitted; the sanitizer is documented as best-effort, not a
guarantee — see the "Stored-content prompt injection" and reranking notes in
`SECURITY.md` for the exact caveats. The embedding path (1) sends full page
text to the provider's embedding endpoint and is not sanitized the same way,
so it is the path to leave on `local` when stored content must not leave the
server.

## Is this "personal data" under GDPR?

ai-memory stores developer-authored artifacts — wiki pages, session
handoffs, entity extractions — derived from coding-agent sessions. It has no
purpose-built handling for personal data (no user-profile fields, no
consent-tracking, no data-subject-request tooling) because it isn't designed
to process personal data as such. In practice this means:

- If your team's session content incidentally contains personal data (e.g. a
  commit message with a name, or free text a developer typed), that data is
  stored and retained exactly like any other memory content, under your own
  organization's control on infrastructure you run.
- ai-memory does not transmit that content anywhere on its own; the two
  opt-in paths above are the only export mechanisms, and both are under your
  control.
- Treat this the same way you'd treat any local database your team
  operates — your organization is the data controller and decides retention,
  access, and disclosure. This document does not constitute legal advice;
  confirm classification with your own data-protection function.

## Processor relationship

Self-hosted with no opt-in features enabled: there is no third party in the
data path, so ai-memory does not create a data-processor relationship under
GDPR Art. 28. If you configure a cloud LLM or embedding provider (for core
functionality, capture, or reranking), **that provider** becomes your
processor for the data you send it, under whatever agreement you have with
them directly — ai-memory is a conduit, not a party to that relationship.

## Deletion / retention

There is no built-in retention-expiry policy; data persists until removed.
Deletion is filesystem-level: removing the data directory removes everything
in it. For scoped deletion, a per-project purge operation exists and is
isolation-safe — it cannot delete files or rows belonging to a different
`(workspace_id, project_id)` (`SECURITY.md`, "Per-project isolation").

## Network exposure (if you run the server non-loopback)

Binding the server beyond `127.0.0.1` is a separate, opt-in operational
choice unrelated to the data-export paths above — it controls who can reach
the API, not what ai-memory sends externally. See `SECURITY.md`'s "Network
exposure when binding to non-loopback addresses" section and
[`docs/https-via-proxy.md`](docs/https-via-proxy.md) before doing this.

## Related documents

- [`SECURITY.md`](SECURITY.md) — threat model, scope, vulnerability reporting.
- [`docs/sso.md`](docs/sso.md) — enterprise identity/SSO integration.
- [`docs/airgapped-install.md`](docs/airgapped-install.md) — offline/air-gapped
  installation.
