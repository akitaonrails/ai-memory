# Typed relation edges

Pages can declare *typed* edges to other pages — not just "these are
related" but *how*:

```yaml
---
title: Fixed the linker OOM
relations:
  fixes: ["gotchas/linker-oom.md"]
  contradicts: ["decisions/0007-static-linking.md"]
---
```

## When to bother — and when not to

Plain `[[wikilinks]]` remain the default and are enough for "these
pages are related". Typed edges earn their keep in one specific loop —
**contradictions you want lint to chase**:

1. A gotcha page exists: `gotchas/linker-oom.md` ("the linker OOMs on
   machines under 32 GB").
2. Months later a session fixes it; the consolidator (or you) writes
   `notes/linker-fix.md` with `relations: { fixes:
   ["gotchas/linker-oom.md"] }` — the gotcha's page history now shows
   what resolved it.
3. Later still, new evidence disagrees with a stored decision; the new
   page declares `contradicts:` — and `memory_lint` reports the pair as
   a `contradiction` finding until someone reconciles them. No LLM
   involved: the declaration IS the signal.

If you are not using lint and don't need fix/cause chains, skip the
frontmatter entirely; nothing else changes.

## The vocabulary (closed)

| relation | meaning |
|---|---|
| `causes` | this page describes a cause of the target |
| `fixes` | this page fixes the problem the target describes |
| `contradicts` | this page disagrees with the target |

The set is deliberately closed: a free-text relation column turns into
an unqueryable folksonomy. Keys outside the vocabulary are skipped at
the write boundary (with a warning) — a typo cannot mint a new edge
kind. Targets use the same grammar as wikilinks: `path`,
`project:path`, or `workspace/project:path`; extension-less targets
gain `.md`.

## What typed edges do

- **`contradicts` feeds lint.** A declared contradiction is the
  highest-signal zero-LLM finding possible — someone (or the
  consolidator) explicitly said two pages disagree. `memory_lint`
  reports each edge as a `contradiction` finding until the pages are
  reconciled, including the case where the target no longer resolves
  (a stale declaration).
- **They participate in the retrieval graph** as ordinary edges. No
  relation-specific ranking weight is applied — the LongMemEval
  harness showed no basis for one yet; the data is stored so a future
  change can be measured rather than guessed.
- **Backlinks stay clean.** A typed edge and a plain `[[wikilink]]`
  to the same target coexist as distinct rows (`links.link_type`), but
  page-link listings deduplicate.

## Who writes them

- **You**, in any page's frontmatter (the wiki files are plain
  markdown — edit and `reindex`, or let the watcher pick it up).
- **The consolidator**, sparingly: both single-page consolidation and
  `memory_consolidate` with `multi_page=true` can preserve a relation
  when the session's evidence states it plainly (a fix
  landed for a documented gotcha; new evidence contradicts a stored
  decision). The output is JSON-schema constrained and filtered to the
  vocabulary again at the write boundary. Empty relation kinds are
  omitted from frontmatter; older outputs without `relations` still work.

Storage detail: edges ride the existing `links.link_type` column
(default `references`), so this needed no schema migration and old
stores need no rewrite.
