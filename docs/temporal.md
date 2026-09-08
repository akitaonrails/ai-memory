# Temporal validity (bi-temporal-lite)

*2.0 item 4.* The entity index carries an **ingestion-time** validity
window, so "what did we know about X as of June" is answerable from
the store — the one mainstream graph-camp mechanism (Zep/Graphiti)
worth having at our scale.

*2.2, issue #656.* Page versions carry the same window at page grain
(`pages.valid_from` / `valid_to`, V62), and `as_of` fuses the entity
timeline with version-filtered full-text search. Vocabulary is pinned
in `docs/design-page-ingestion-windows.md`: this is **ingestion time**
("the version the store treated as current"), never world-time
validity — "valid-time" stays reserved for the deferred Phase B.

## Honest scope

- **Ingestion time only.** `valid_from` / `superseded_at` (link grain)
  and `valid_from` / `valid_to` (page grain) record when ai-memory
  *learned* and *replaced* a fact, not when it was true in the world. A
  world-time split (Graphiti's `valid_at`/`invalid_at` extracted by an
  LLM) is deliberately out of scope: it requires trusting an LLM to
  date facts, and our zero-LLM default path could never populate it.
- **Two grains, one semantic.** Entity links inherit their page
  version's timeline (V56, repaired by V58); page versions now
  materialize it directly (V62). Typed relation edges (item 3) die with
  their page version, which is already a timeline — no extra columns
  needed there yet.
- **Deletion is deletion.** Purged pages cascade their entity links
  away; the timeline does not survive an explicit purge (that is what
  purge means here — see the purge docs).

## Schema

`entity_page_links` carries two additive columns:

| column | meaning |
|---|---|
| `valid_from` | `created_at` of the page version the link belongs to |
| `superseded_at` | `created_at` of the version that superseded it; `NULL` while the version is latest |

Backfill is a one-shot data migration inside refinery: `valid_from`
from the linked version's `created_at`; `superseded_at` from the
superseding version's `created_at` (found via `pages.supersedes`),
`NULL` for latest versions. New writes populate `valid_from` at
insert, and the supersede path closes the outgoing version's windows
in the same transaction — a link's window can never be open-ended in
a superseded version.

`pages` carries the same window at page grain (V62, issue #656):

| column | meaning |
|---|---|
| `valid_from` | the version's own `created_at` |
| `valid_to` | `created_at` of the superseding version; the retirement instant for successor-less retirements; `NULL` while the version is latest |

For existing successor-less retirements, V62 uses the decay marker first,
then the earliest recorded entity-link close, then `updated_at` as an
approximation when no retirement timestamp survived. Reorg and
move-regenerate historically recorded the close only in entity links.

The end is named `valid_to`, not `superseded_at`, because
`pages.superseded_at` already exists with a different meaning: the V03
decay-tombstone eviction marker, written exactly when `supersedes IS
NULL`. Every retire path closes both grains in the same transaction
(supersede, decay, reorg graveyard, move-regenerate); the V62 backfill
covers existing stores with the same rules. No FTS rebuild is needed:
the `pages_fts_*` triggers already index every version row, superseded
ones included — version-filtered search is a join-predicate change,
not an index change.

## When to reach for it

Ordinary `memory_query` answers "what do we know NOW" and is the right
call 99% of the time. `as_of` is for audits and post-mortems: "what
did the wiki say when that decision was made", "did we already know
about this gotcha before the incident".

## A worked example

A project recorded `postgres` as its database, then migrated:

```jsonc
// June: notes/db.md says "we use postgres"    (entity: postgres)
// August: the page is superseded — "we migrated to sqlite" (entity: sqlite)

memory_query { "query": "postgres" }
// → no hits: current knowledge has moved on

memory_query { "query": "postgres", "as_of": "2026-07-01T00:00:00Z" }
// → the JUNE version of notes/db.md — the page that carried the
//   `postgres` entity when July began, even though it has since been
//   superseded
```

The entity leg needs the entity: it only answers when the query names
a tracked entity the version carried. The FTS leg covers the rest — a
version with no `entities:`/`tags:` frontmatter, or a question phrased
without the exact retired term ("which database were we on during the
outage?"), still resolves through the text that was live at T.

## Query

`memory_query` accepts `as_of` (ISO-8601). When present the query is
a **lookup over historical versions using two streams**:

- the **entity timeline**: links whose window contains the instant
  (`valid_from <= T AND (superseded_at IS NULL OR superseded_at > T)`),
  returning the page versions that carried the matched entities then;
- **version-filtered FTS**: page versions whose page-grain window
  contains the instant
  (`valid_from <= T AND (valid_to IS NULL OR valid_to > T)`),
  answering questions through lexical matches even without entity metadata.

The two streams merge with the default path's RRF (k=60) plus the same
bounded authority adjustment, and `explain=true` reports both in
`streams_active` (`["entity", "fts"]`) with per-stream ranks and RRF
contributions. The time filter selects historical versions, but FTS5
BM25 uses statistics from the current index, including later versions
and other projects. Later writes can therefore change ordering and the
limited result set for the same T. This retrieves historical content;
it does not reproduce the ranking a search would have returned at T.

Still out of `as_of`: vector (embeddings are present-tense artifacts
of the current text; version-scoped vectors are a separate project),
graph neighbours (latest-scoped by construction), and the
raw-observation fallback. Audit reads take no access bump and no
rerank. Omit `as_of` and nothing changes.

One asymmetry to know: the FTS leg hides pages already TTL-expired at
T (the usual `not_expired` predicate evaluated at T); the entity leg
ignores TTL, on the grounds that a page valid at T was still what we
knew at T even if it expired since.

## Where entities come from

`as_of` and the entity retrieval stream both read the entity index, so
they only answer once pages actually carry entities. Entities are the
salient nouns a page is *about*, and they are populated from each page's
frontmatter at write time:

- an explicit `entities:` list, when an LLM consolidator emits one; and
- the page's `tags:` list, which nearly every page carries — tags *are*
  "what this page is about", so they seed the entity index deterministically
  with no LLM pass and no re-consolidation.

Broad tags do not distort ranking: the entity stream weights each match
by inverse page-frequency, so a tag shared by many pages contributes
proportionally little.

Existing stores self-heal. On the first start after upgrading, a
one-shot, idempotent backfill scans latest pages that have no entity
links yet, derives their entities from the same frontmatter, and opens
each link's validity window at the page version's own `created_at` — so
`as_of` works historically, not just from the upgrade forward. A store
whose pages were all written through the current path finds no
candidates and the pass is a no-op. No command is required; a manual
`reindex` is only needed if you deliberately cleared the index.
