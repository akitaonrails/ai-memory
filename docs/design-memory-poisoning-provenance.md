# Design: source provenance and trust class against memory poisoning

Status: design, partially staged. Tracks #2164. Step 1 (the `page_sources`
table, migration V108) is implemented; "Step 1 as built" below records what
steps 2 to 5 can rely on. The rest of this note covers those steps.

## Problem

On a compile-style wiki, poisoning is not stopped at the write gate. Measured
against a real memory system, a single poisoned tool result survives the memory
write about 98% of the time (arXiv:2611.09721), and the sanitizer's own contract
says it strips secrets and bounds size but cannot make stored prose trusted.
Consolidation then compiles an attacker instruction sitting in a `post-tool-use`
body into a page as faithfully as it compiles an operator's own note.

Today a page carries `path`, `tier`, `supersedes`, and `author_id`, and nothing
about where its body came from. A compiled page cannot be traced to the
observations that produced it, an operator-authored decision and a scraped tool
result are the same kind of object, and the only defence is the "untrusted data,
not instructions" clause in the consolidation prompt and `MEMORY_INSTRUCTIONS`,
which is prose, not structure.

The paper's numbers (about 98% write survival, about 22% retrieval-to-behaviour
conversion) place the useful levers after the write: knowing a page's origin,
keeping tool-origin instructions out of the always-on rule surface, and ranking
tool-origin material below operator material at retrieval. A write-time content
filter is out of scope.

## Steps

1. `page_sources(page_id, observation_id)`, the provenance plumbing, in its own
   PR (see #2164). A page can name the observations it was compiled from, written
   in the same transaction as the page upsert. The rest of this note assumes it.
2. A trust class on observations and pages.
3. Rule-emission gating, so an instruction first seen in a tool result is not
   emitted as a `_rules/` page.
4. `memory_query` trust ranking: the class is returned, and tool-origin pages
   rank below operator-authored ones by default.
5. Adversarial tests and a `docs/security-boundaries.md` row.

## Step 1 as built

`page_sources(workspace_id, project_id, page_id, observation_id, created_at)`
is keyed by page version and written in the page-upsert transaction, from the
session citation a write already carries (`PageEvidence::observations`).

- **Consolidation** lists the observations whose text reached the prompt:
  every rendered block, plus every verbatim-residue group, minus anything the
  final clip cut off (`ProjectedObservations::source_ids`). On a session longer
  than the projection's selection cap this is fewer than the session holds.
  Every page of a batch lists the same set, since one prompt produced them.
- **`memory_write_page` with a `session_id`** lists every observation the
  session has in the target project when the write commits. The tool names a
  session, not observations, so this is the narrowest honest set.
- **Re-derived per compiled version.** A new version starts from the sources of
  its own compile and the superseded version keeps its rows. A write that
  reproduces the current version byte for byte creates no new version, so its
  sources are added to that version's: the same text has then been compiled
  from both sets, and dropping the earlier one would let a later compile hide a
  tool result behind identical text.
- **A version written without a session citation has no rows.** That covers a
  hand edit, `memory_patch_page`, a lint or compaction rewrite, a reindex from
  disk and a copy+purge `move-project`. Zero rows means "not recorded". Step 2
  has to decide what class such a version takes when the version it replaces
  had tool-result sources; the chain is reachable through `pages.supersedes`.
- **The store filters.** A row is kept only for an observation of the cited
  session inside the page's own workspace and project.
- **`observation_id` is not a foreign key.** Observation retention prunes rows,
  and the id outlives the observation. A class that must survive pruning has to
  be stored at compile time, not looked up from the observation later.
- **Not covered:** the zero-LLM rule-based `sessions/<id>.md` page the hook
  router writes at SessionEnd and at compaction checkpoints. It is built from
  the session's observations and cites no session today, so it records no
  sources. Pages compiled before V108 have none either; nothing is backfilled,
  because the observations an old prompt selected were not kept.

Read path: `ReaderPool::page_compiled_sources`, by `(workspace_id, project_id,
path)`, under the session-owner filter.

## Step 2: trust class

Three classes, ordered most to least trusted:

- `operator_authored`: the operator's own words. A `user-prompt` observation,
  and a `memory_write_page` body the operator wrote.
- `model_generated`: text the assistant produced (captured assistant turns,
  `stop` summaries). Trusted less than the operator and more than a tool result.
  The model can be wrong or steered, but it is not the direct injection vector.
- `tool_result`: a `post-tool-use` body. External content the harness pasted in,
  the poisoning vector the paper measures.

The class is assigned at hook ingress from the `ObservationKind` the router
already computes, with the `capture_assistant` path deciding whether assistant
turns are stored at all:

| `ObservationKind`                              | Trust class        |
|------------------------------------------------|--------------------|
| `user-prompt`                                  | `operator_authored`|
| `post-tool-use`                                | `tool_result`      |
| `stop`, captured assistant content             | `model_generated`  |
| `session-start`, `pre-tool-use`, `notification`, metadata | `model_generated` (context, never a rule source) |

A `memory_write_page` carries `operator_authored` when an operator wrote it and
no `session_id` ties it to compiled observations. A write that does carry a
`session_id` inherits the least-trusted class of its sources, like a consolidated
page below.

The class is a new column on observations, set at ingress and never rewritten, so
the sanitizer boundary stays the only path from untrusted text into the store
(invariant #6). A page derives its class from `page_sources`: the least-trusted
class among its sources. One `tool_result` source makes the page `tool_result`.
The derivation is recomputed whenever the page is recompiled, so it follows the
supersession chain instead of freezing at first write.

## Step 3: rule-emission gating

A `_rules/` page surfaces on every turn, so it is the highest-value target for a
compiled injection. The rule: an instruction whose supporting evidence is drawn
only from `tool_result` observations is not written as `kind = rule`. The
consolidation and auto-improve emit paths already carry the evidence
observations for a proposed page; before writing a `_rules/` path they check the
trust class of that evidence. A proposed rule with at least one
`operator_authored` or `model_generated` source is allowed; one backed solely by
tool-result origin is downgraded to a lower tier (a `gotchas/` note) or rejected,
with the reason recorded. The same instruction restated in a `user-prompt` lifts
the block, the intended escape hatch for a real rule an operator confirms.

## Step 4: `memory_query` trust ranking

`memory_query` returns the trust class on each hit, so a caller and the prompt
surface can see it. Ranking demotes `tool_result`-origin pages below
`operator_authored` ones by default. The demotion applies inside the RRF fusion,
not as a hard filter, so a strongly-matching tool-origin page is still reachable,
just under an operator page of equal match. The default is on; an explicit
opt-out keeps the old ranking for a caller that wants it.

This changes retrieval ranking, so it is benchmark-affecting. Steps 2 to 4 do
not land until the `ai-memory-eval poison` baseline exists to measure them
(marker transfer between sessions, rule misclassification, path containment), and
the ranking change is diffed against the LongMemEval slices like any other
retrieval change before release. The demotion weight is tuned against the poison
baseline.

## Step 5: tests and the boundary inventory

This adds a trust dimension, so `docs/security-boundaries.md` gets a new row and
adversarial tests that bite:

- a poisoned `post-tool-use` observation compiled into a page is marked
  `tool_result`, not `operator_authored`;
- an instruction first seen only in a tool result does not become a `_rules/`
  page;
- a tool-origin page ranks below an equally-matching operator page in
  `memory_query`;
- a legitimate control (an operator-authored rule, an operator-corroborated
  instruction) is unaffected.

Each test must fail with the guard removed and pass with it, at integration
level, per the security-boundary test rule.

## Dependencies and sequencing

- Step 1 (`page_sources`) is independent and lands first; it needs no baseline.
- Steps 2 and 3 depend on step 1 and the trust-class column.
- Step 4 depends on steps 2 and 3 and waits on the `ai-memory-eval poison`
  baseline.
- Step 5 lands with each of steps 2 to 4 that it covers.

## Open questions

- Settled by step 1: `page_sources` is re-derived per compiled version, not
  append-only across the supersession chain. See "Step 1 as built".
- What class a version takes when it was written without a session citation
  over a version that had tool-result sources (see "Step 1 as built").
- Whether the rule-based session page should cite its session, which would
  also add a `page_evidence` row on zero-LLM installs.
- Whether `model_generated` should also be demoted at retrieval, or only
  `tool_result`. The paper's vector is the tool result; demoting model-generated
  text as well is a larger behaviour change, left out of the default until the
  baseline says it helps.
