# Day-to-day usage

This page covers what happens after ai-memory is installed: handoffs,
compaction recovery, proactive memory queries, the web UI, and the
managed routing snippet + Agent Skills package.

## Capture exclusions

To keep recognized file-tool events under private paths out of ai-memory before
they are spooled or sent, configure `[capture] ignore_paths` in the nearest
`.ai-memory.toml`. The canonical grammar, limitations, support matrix, refresh
requirements, and safe local `--check-capture` command are in
[the marker-file reference](marker-file.md#capture-exclusions). Shell commands
are matched by their path arguments too (`cat docs/adr/*.md`), lexically. This
is not a general prompt/output DLP filter.

## Cross-agent handoff

You normally do not create handoffs by hand. With lifecycle hooks
installed, session-end capture writes the handoff and the next
session-start hook fetches it. Manual handoffs are project-wide and take
precedence over automatic SessionEnd handoffs. Among automatic handoffs that
match the receiving directory by path boundary, the newest is delivered;
creating a new automatic handoff expires prior open automatic handoffs from
that exact directory, and acceptance expires older matching automatic
handoffs without disturbing manual handoffs or pending work from sibling
directories.

```text
$ claude
> Working on the auth refactor. JWT rotation is broken; trying session cookies.
[work for an hour]
> /exit

$ codex   # in the same directory, later
[SessionStart hook fetches the handoff; Codex sees it before your prompt.]
> Picking up: you were investigating session cookies as an alternative...
```

If an agent has MCP but no lifecycle hook surface, ask it to call
`memory_handoff_begin` before quitting. The next hooked agent can still
consume that handoff automatically. Grok shows it as `PostToolUse`
`additionalContext` after the first tool. `install-hooks --agent grok --apply`
and `install-skills --scope global --agent grok` write
`$GROK_HOME/rules/ai-memory-handoff.md` so the first turn calls
`memory_handoff_list` (read-only; PostToolUse then claims the row). The rule
spells out Grok's invocation — `use_tool` with the qualified catalog key
`ai-memory__memory_handoff_list` — because Grok reaches MCP tools only through
that meta-tool, and puts that call ahead of any file read, correcting
`workspace`/`project` from `./.ai-memory.toml` only when the list is empty or a scope error.
Until that
tool runs, or if the rule is absent and the session never calls a tool, call
`memory_handoff_list` then `memory_handoff_accept` with the listed
`handoff_id`. Zero should list then accept on resume. Listing does not claim
the row.

`memory_handoff_accept` says why it returned no handoff. Its `status` is
`claimed` when the call took one, `consumed_by_hook` when the calling
session's own SessionStart already did (the handoff is in that session's
context), and `none_pending` when nothing is left to claim. Only a client that
forwards its session id on MCP calls can be told `consumed_by_hook`: Claude
Code through `install-mcp --session-aware`, or OpenCode 2. Any other client
gets `none_pending` after the hook consumed the handoff, so its agent still
checks its context for the delivered block first.

On a server that distinguishes operators, handoffs belong to their creator by
default: the next session for that operator sees their own plus deliberately
shared rows, never a teammate's. Use `shared: true` on
`memory_handoff_begin` only when the baton is intended for anyone in the
project. Root-authorized recovery can pass `any_owner: true` to
`memory_handoff_accept` or `memory_handoff_cancel`; normal callers cannot use
that switch.

Handoffs are next-session transfer, not a live message bus between agents that
are still running. In particular, Antigravity fetches a handoff only once per
conversation: on `SessionStart`, or on builds without it, on the first turn's
`PreInvocation` with invocation zero. Its later turns and model calls never
fetch one, so a handoff created later in that conversation stays open instead
of being consumed by its creator's next model call.

If an agent creates a handoff by mistake, cancel it immediately with
`memory_handoff_cancel` and the `handoff_id` returned by
`memory_handoff_begin`. Cancelling marks the handoff expired, so the next
session-start hook will not consume stale context.

### Offering instead of claiming at session start

By default, `SessionStart` claims a pending handoff automatically, as
described above. With several concurrent lines of work in the same
project, an unrelated session (a different task, a different harness, or a
non-interactive launch) can consume a baton meant for a specific follow-up
session. Set `claim_on_session_start = false` under `[handoff]` in
`config.toml` to stop that:

```toml
[handoff]
claim_on_session_start = false
```

`SessionStart` then leaves the handoff open and renders a non-consuming
notice instead, naming the exact `handoff_id`, the agent that left it, and
its age. It never includes the stored summary, open questions, or next steps (same
security bar as the inbox notice: that content is written by whatever agent
or operator ended the prior session, and a notice injected into the on-start
context cannot be deliberately skipped the way leaving `memory_handoff_accept`
uncalled can). Pick it up explicitly:

```
> memory_handoff_accept handoff_id=<the id from the notice>
```

This is server-wide: every operator on the server gets the same behavior.
Restart the server after changing `config.toml`; configuration is loaded once
at startup. The default (`true`) is unchanged for every existing install.

### Opting out of automatic handoff creation at session end

By default, every unmanaged session end creates an open handoff for the next session. In a single-operator setup where consecutive sessions are part of the same continuous workflow or where batons are undesirable, automatic creation can be disabled while preserving the session summary page and consolidation:

```toml
[handoff]
create_on_session_end = false
```

When set to `false`, `SessionEnd` writes `sessions/<id>.md` and queues consolidation as usual, but skips creating an open handoff row (#1043). OpenCode's per-turn checkpoint, which stands in for its session end, likewise refreshes the summary page without creating or refreshing a baton. Explicit batons created with `memory_handoff_begin` and managed workstream runs remain unaffected. Like `claim_on_session_start`, this setting is server-wide.

## Compaction recovery

When Claude Code or Codex compact their working context, the
`PreCompact` hook fires and ai-memory writes a fresh
`sessions/<id>.md` page summarising the session so far. The hook
response returns after that heuristic page; when a provider is
configured the LLM rewrite is queued and applied by the same durable
worker SessionEnd uses. With a provider, the checkpoint write also goes
through `consolidate` admission, so a policy that refuses consolidation
leaves no page and queues nothing. A rewrite still queued when the
session ends runs once over the whole session even with
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END` off, replacing the rule-based
summary; with the opt-in on, the SessionEnd job supersedes it. Without
the opt-in, a session that ends while that rewrite is still running
keeps the rewrite as the latest page, so it can leave out the end of the
session; the SessionEnd summary stays reachable as the superseded
version. After
compaction, the agent can recover the summary via `memory_recent` even
though its raw chat history was compacted away.

Generated session pages carry `session_id` and `agent` in frontmatter. The
`agent` value is the originating harness stored on the session (for example,
`claude-code` or `codex`), not the client or operator that later requested a
consolidation. Checkpoints and superseding versions therefore keep the same
origin. Manual `memory_write_page` and `ai-memory write-page` calls do not
infer an agent.

### Reload context after a compaction

A compaction keeps a model-written summary of the conversation, and the
summary can drop or soften what you asked. Start a prompt with
`reload context` to put the stretch it dropped back in front of the agent:

```
reload context after last compaction, then finish the screenshots for guide 12
```

The hooks see the phrase at the start of the prompt (any case; a sentence
that only mentions it does not count) and inject a **compaction recap** built
from what ai-memory captured for this session between its latest compaction
and the one before it (or the session start):

- your prompts in that stretch, verbatim, oldest first; harness markup and
  task notifications are left out. When they do not all fit, the newest stay
  whole, the next older one is cut to the room left, and older ones are
  dropped with a note;
- the agent's last reply before the compaction. Replies are captured only
  with the assistant-capture double opt-in (`install-hooks --capture-assistant`
  plus `capture_assistant` on the server); without it the recap says so;
- the project's open handoffs, listed and never claimed, this session's own
  first.

The rest of the prompt runs as usual with the recap in context. Serving the
recap writes nothing: no handoff is claimed and no page or extra observation
is added (the prompt itself is captured like any other).

Fetching the recap holds the prompt until it arrives. On a healthy local
server that takes milliseconds. When the server is slow or unreachable, the
hook first spends up to 3 seconds sending queued events and then waits up to
3 seconds for the recap, so the prompt can take about 6 seconds to reach the
agent, which then runs it without a recap. Prompts that do not start with
`reload context` never wait for this.

| Harness | When the agent sees the recap |
|---|---|
| Claude Code | With that prompt (`UserPromptSubmit` `additionalContext`, up to 8,000 chars) |
| Codex | With that prompt (developer context, up to 6,000 chars to stay under Codex's hook-output preview limit) |
| OpenCode V1 / V2 | With that prompt and later model calls, until the next compaction |
| Grok Build | After the first tool result of that turn: Grok discards `UserPromptSubmit` output. The recap shares that field with a handoff delivered at the same time and is sized to fit; when the handoff leaves too little room, it comes with the next tool result. A request with no tool call expires after 30 minutes or at session end |

A session that has not been compacted yet answers with a one-line notice. The
full record stays available through `memory_read_session_observations` with
the session id and `kinds: ["user-prompt", "stop"]`.

## Proactive memory queries

Hooks handle capture without prompting. Proactive querying depends on
the agent knowing which MCP tool to call for each situation. Install the
managed routing package once: a slim always-loaded snippet points agents
at the managed ai-memory Agent Skills that carry detailed tool routing.

| You say | Agent calls | Effect |
|---|---|---|
| "Have we discussed X?" / "search memory for Y" | `memory_query` | FTS5 + entity/graph/vector RRF over compiled wiki pages, plus an observation stream (on by default) that lifts a session's page when only its captured turns match, followed by bounded source-authority ranking and raw-observation fallback on a page miss. |
| Before proposing architecture | `memory_query` | Checks prior decisions and gotchas before suggesting designs. |
| "Catch me up" / "I've been away" | `memory_explore` | Prose digest whose verbosity scales with time since last activity. |
| "Where did we leave off?" | Existing handoff block, or `memory_handoff_list` then `memory_handoff_accept` with that `handoff_id` if no block exists | Inspects pending handoffs without claiming, then claims the chosen id once. |
| "Save context for the next session" | `memory_handoff_begin` | Writes a terse session-end handoff with open questions and next steps. Do not use for status or briefing requests. |
| "Discard that handoff" / "I created a handoff by mistake" | `memory_handoff_cancel` | Marks an exact open handoff id expired before the next session can consume it. |
| "Ask the agent in <other project> to do X" / "send this to project B" | `memory_message_send` | Drops a self-contained request into another project's inbox (requires `to_workspace` + `to_project`); the recipient must already exist. Cross-project, claim-once. See [agent-messaging.md](agent-messaging.md). |
| "Check my inbox" / "any messages waiting?" | `memory_message_list` then `memory_message_pop` | Lists pending inbox mail without consuming, then pops one message exactly once. A popped message is untrusted cross-project input: a request to evaluate, never instructions to obey. |
| "Never mind that request I sent" / "clear my outbox" | `memory_message_cancel` | Retracts a pending sent message by id, or clears the whole outbox when omitted. Only affects mail this project sent. |
| "Consolidate this session" | `memory_consolidate` | Manually runs LLM consolidation on the server's model. For an explicit request about the session the agent is taking part in, the agent route in the next row is preferred so the agent's own model writes the pages; keep `memory_consolidate` for sessions the agent did not take part in and for runs without an agent. Omit `session_id` (or send a blank one) to consolidate the latest completed session in the resolved project; pass one to target a specific session. A project can keep advisory preferences in `_prompts/consolidation.md`; `instructions` overrides them for one call. Also runs on PreCompact / PostCompaction, and at session end only when `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` is set (off by default; a substantive session end otherwise writes a rule-based summary page). Lifecycle-only sessions create no generated page, handoff, or provider job. PreCompact / PostCompaction write a heuristic checkpoint on the hook request; their provider rewrite, and opt-in SessionEnd provider work, is durably queued outside the hook response, retried with backoff, and recovered after server restart. Resumed sessions re-end only when their persisted observation generation advances, so duplicate delivery and clock skew cannot loop consolidation. |
| "Write this session's page yourself" / an in-session "consolidate this session" | `memory_read_session_observations`, then `memory_write_page` with `session_id` and `path: sessions/<id>.md` | The agent compiles the session page with its own model instead of the server's provider. To match the server's multi-page layout it may also write up to four more pages with the same `session_id`: `concepts/<slug>.md` (`tier: semantic`, `kind: fact`), `decisions/<short>.md` (`kind: decision`), `gotchas/<slug>.md` (`kind: gotcha`) and `_rules/<slug>.md` (`kind: rule`); the session page is `tier: episodic`, `kind: fact`. The session must belong to the project the page is written to; the page records it as evidence, gets the same session-page frontmatter (plus `consolidated_by: agent`) and duplicate-title suffix, and the session's queued SessionEnd job is marked completed. The write uses the `write_page` admission op, so a webhook that filters on `consolidate` does not see it. A pinned page is never overwritten this way. SessionEnd keeps the page: neither the rule-based summary nor the opt-in SessionEnd worker replaces it, even when the session went on after the write, and a PreCompact or PostCompaction checkpoint leaves it alone too. |
| "What did we learn from this session?" / "what memory should we add?" | `memory_auto_improve` | Without a session ID, reviews the newest completed session with no persisted auto-improvement run, advancing past preflight skips on repeated calls; pass an ID for a targeted rerun. The server also runs scheduled auto-improvement for new completed sessions when an LLM is configured. `[auto_improve.scheduler] enabled = false` disables automatic review; `[auto_improve] require_approval = true` leaves scheduled and manual proposals in pending-writes for review. |
| "Remember this permanently" / "add an annotation" | `memory_write_page` | Writes durable wiki knowledge; not a single-use handoff. |
| "Append this action to the Log section" / "replace only Open issues" | `memory_read_page`, then `memory_patch_page` | Patches an existing Markdown page without re-emitting its full body or frontmatter. Pass the required `page_id`; section replacement also passes the section's returned `sha256`. The batch writes all edits or none. |
| "Remember this until Friday" / "expire this after the migration" | `memory_write_page` with `expires_at` | Writes a time-bounded page. Use RFC3339 or `YYYY-MM-DD` (end of day UTC); normal retrieval hides it after expiry and the next forget sweep deletes it; a `memory_read_page` by exact path still returns it, marked `expired: true`. TTL outranks `pinned`. |
| "Search expired notes for X" | `memory_query` with `include_expired: true` | Opts an explicit project, sibling-scope, or global search into expired historical pages; ordinary searches exclude them. |
| "Search only Rust release runbooks" | `memory_query` with `path_prefix: "procedures/"`, `tag: "rust"`, and optionally `kind` / `tier` | Narrows every page retrieval stream before ranking. Filters also constrain global, time-travel, profile, and `pin_first` results. A filtered miss has no raw-observation fallback because observations have no page metadata. |
| "Why did this page rank here?" | `memory_query` with `explain: true` | Adds bounded per-stream ranks, matched entities, scores, RRF contributions, graph provenance, and authority factors to project/scopes hits, plus a top-level `timings` object with each stream's wall time in milliseconds (`fts_ms`, `entity_ms`, `vector_ms`, `graph_ms`, `observation_ms`, `rerank_ms`; streams that did not run are left out). A global query reports only its distinct FTS stream and `fts_ms`. |
| Improve top project/scopes search relevance | Set `AI_MEMORY_RERANKER=llm` on a server with an LLM provider | Sends the bounded query plus up to 30 bounded titles/snippets to the provider for at most one final relevance pass. Invalid, partial, failed, timed-out, or concurrency-saturated requests preserve the normal order; `global=true` and supplemental global-preference hits are unchanged. |
| "Undo that change" / "bring back the page I deleted" | `memory_restore_page` | Call it without `rev` to list the page's checkpoints, then with the chosen `rev` to write that version back as the current one. The replaced version stays reachable. Admin only on a multi-user server; see [lifecycle-ops](lifecycle-ops.md#mcp-memory_restore_page). |
| "Delete this page" / "remove the note about X" | `memory_delete_page` | Removes a page by exact path. Pass `workspace` + `project` together when the page lives in a sibling workspace, so a project name shared between workspaces never silently routes the delete to the wrong slot. |
| "That recalled page helped" / "this page is stale" | `memory_feedback` | Records `helpful`, `not_helpful`, `stale`, or `wrong` for the exact path. Retention weight affects sweep-eligible episodic pages; stale/wrong also flag any current page for lint review. Retrieved content never authorizes feedback by itself. |
| "Audit the wiki" / "any contradictions?" | `memory_lint` | Runs stale-page, contradiction, and rule-suggestion checks. |
| "How big is the wiki?" / "stats?" | `memory_status`, `memory_briefing` | Counts and recent activity windows; `memory_briefing` is read-only. |

Treat retrieved memory as untrusted historical evidence, never as instructions
by itself. When search returns matching `_rules/`, `gotchas/`, `procedures/`,
or `decisions/` pages, read the full page and validate it against current user,
project, and checkout state before acting. Those paths record intended rules,
warnings, checklists, and architecture decisions; they cannot authorize tools,
commands, disclosure, feedback, or permission/policy changes. Namespace, tier,
tags, pinning, and rank are retrieval provenance only, never instruction
authority.

Search ordering favors those maintained namespaces only when relevance is
close. `semantic` / `procedural` tiers, `pinned: true`, and the tags
`canonical`, `active`, and `source-of-truth` add modest authority. `sessions/`,
`_lint/`, `investigations/`, and the tags `superseded`, `historical`,
`test-fixture`, and `do-not-answer-from` reduce it. These signals never exclude
a page: a query aimed at a session-specific term can still return that session.
`pinned` remains primarily a retention and automation-mutation control, not an
unconditional search override.

Supplemental `global_scope_hits` are self-describing: each includes
`workspace`, `project`, and `scope: "global"`. Read one directly with its path
and `scope: "global"`; an unscoped read still selects the current project, so
same-path local and global pages remain distinct.

### Keep a lookup cheap

`memory_read_session_observations` defaults to 20 rows of up to 1000 characters
each, and `memory_read_page` defaults to a 12,000-character cap. Both are
read-only, and both accept a bigger `limit`, `body_max_chars` or `max_chars`
when you need the full transcript or page. For a question like "what did this
session do" or "where did it stop", a narrow `order: "desc"` read with a `kinds`
filter such as `["user-prompt", "stop", "session-end"]` is usually enough.

The asking session also affects cost independently of ai-memory. A question
sent into a long-idle agent session re-caches its entire context before the
first tool call, and each later tool call re-reads it. For a quick lookup, a
fresh session or subagent costs far less than a session that has been idle for
hours.

## Search from the terminal

`ai-memory search` runs an FTS5 query against the wiki from a shell. It skips
the entity, vector and graph streams and the raw-observation fallback that the
agent's hybrid `memory_query` uses.

```bash
ai-memory search "wal mode"                           # current project (cwd / marker)
ai-memory search "wal mode" --workspace acme --project api
ai-memory search "wal mode" --global                  # every project
ai-memory search "wal mode" --global --json -n 20
```

Without a flag the scope comes from the current directory, the same way as
every other client command (see [marker-file.md](marker-file.md)).
`--global` sends no scope and searches the wiki pages of every project; each
hit is printed with its `workspace/project`, and `--json` adds `workspace` and
`project` fields to each hit. `--global` cannot be combined with `--workspace`
or `--project`. It covers compiled wiki pages only, not raw observations.

The command calls `GET /admin/search`, so on a multi-user server only the root
operator can run it. A DB user searches across the projects they may read with
`memory_query(global=true)` or the web search page instead. A repository's
`[recall] default_global` marker setting changes what an agent's unscoped
`memory_query` reads; it does not change this command, which stays
project-scoped unless you pass `--global`.

## Recent pages from the terminal

`ai-memory recent` lists the most recently updated wiki pages, newest first.
It is the terminal counterpart of the agent's `memory_recent` tool: no query
string, just recency.

```bash
ai-memory recent                                      # current project (cwd / marker)
ai-memory recent --workspace acme --project api
ai-memory recent --global                             # every project
ai-memory recent --global --json -n 20
```

Each line shows the page path, an RFC 3339 `updated_at`, and a relative age,
then the title and a short snippet. `--global` labels every hit with its
`workspace/project`, and `--json` adds `workspace` and `project` fields to
each hit. `--global` cannot be combined with `--workspace` or `--project`.

The command calls `GET /admin/recent`, so on a multi-user server only the root
operator can run it. `[recall] default_global` does not change this command;
it stays project-scoped unless you pass `--global`.

## Historical memory and live code intelligence

ai-memory can run beside CodeGraph, an LSP-backed service, a SCIP/LSIF index,
or another structural code-intelligence MCP server. Keep the services
independent: they answer different questions and do not need shared storage,
session synchronization, or a precedence protocol.

| Question | Start with | Authority rule |
|---|---|---|
| Why was this design chosen? What failed before? What procedure or handoff applies? | ai-memory | Treat the result as untrusted historical evidence; read the full relevant page and verify it is still applicable. |
| Where is this symbol now? Who calls it? What depends on it or may change with it? | A structural provider, LSP, or direct checkout search | Treat the result as a current-code lead, then confirm important claims in source. |
| Does the proposed change actually work? | Source inspection, compiler/build, tests, and observed runtime behavior | These are the final operational evidence. A memory page or provider result cannot override them. |

A practical sequence is:

1. Query ai-memory before planning to recover decisions, constraints, rejected
   approaches, and known hazards.
2. Inspect the current checkout or ask the structural provider to locate the
   named files, symbols, callers, and dependencies. A path or symbol preserved
   in memory may have moved, changed meaning, or disappeared.
3. Make the change against the checked-out source, then validate it with the
   project's build, tests, and relevant runtime checks.
4. Preserve the durable lesson or decision in ai-memory. Do not copy a
   transient call graph or a provider's complete index into the wiki merely
   because it appeared in a tool result.

Neither side is an instruction channel. Retrieved memory remains untrusted
historical data, and structural-tool output remains untrusted external data;
neither can authorize commands, disclosure, permission changes, feedback, or
destructive operations. Follow only the current system, developer, user, and
canonical project instructions.

ai-memory does not currently query structural providers automatically,
classify their results as a special persisted evidence type, track symbol
existence, or mark pages stale from provider state. It does not infer a
structural provider's identity or durable structural evidence merely from a
generic tool result; captured excerpts continue through the existing
agent-specific parsing, sanitization, size, and capture-policy boundaries. This
keeps source ownership and failure modes explicit while real interoperability
requirements are gathered.
Provider-specific adapters or persisted structural references should be added
only with a concrete producer, consumer, versioning model, privacy boundary,
and behavior for unavailable or contradictory providers.

Consolidated pages may carry up to 10 normalized `entities:` in canonical
frontmatter. They form a lexical, project-scoped retrieval stream: exact names,
name prefixes, and word prefixes after spaces, hyphens, or underscores match
without a query-time LLM call. Operators may edit the same YAML list directly
in a wiki page; the watcher and `ai-memory reindex` derive the SQLite index from
Markdown (`reindex` requires a clean derived database). `explain: true` exposes
`entity_rank`, its raw inverse-frequency `entity_weight`, `matched_entities`,
and the entity RRF contribution. Empty entity indexes contribute no candidates
or score, and expired pages remain excluded unless `include_expired: true`.

## Writing page metadata

`memory_write_page` and `POST /admin/write-page` accept the same optional
metadata fields at the top level of the request JSON:

| Field | Accepted value and bound |
| --- | --- |
| `kind` | MCP: free-text semantic kind, up to 64 raw characters, then trimmed. Admin: the existing optional string adapter retains its original handling (trim and omit empty), without the new MCP kind checks. |
| `entities` | Up to 10 input strings, each up to 64 raw characters; then trimmed, whitespace-collapsed, lowercased and deduplicated using the existing entity normalizer. |
| `abstract` | L0 summary, up to 1,024 raw characters, then trimmed. |
| `relations` | Object with only `causes`, `fixes`, or `contradicts` keys and arrays of page targets, up to 32 input targets total and 1,024 raw characters per target. |

Character and list limits apply to raw input, before trimming, normalization
or deduplication. For example, an entity with 60 characters followed by six
spaces is refused even though normalization alone would produce 60 characters.
The admin's existing top-level `kind` keeps its legacy behavior, including
strings longer than 64 characters; the importer continues using that adapter.

Relation targets use the existing wikilink grammar: `notes/page`,
`project:notes/page.md`, or `workspace/project:notes/page.md`. Extensionless
paths resolve with `.md`; invalid or non-portable page targets are refused.
Scope components must already be trimmed: `other:notes/x` is valid, while
`other :notes/x` is refused instead of creating an unresolved edge.
Relations describe links; they do not grant access to their destinations.
Malformed shapes, invalid entities and exceeded bounds in the new metadata
fields fail before a write scope is created. Those fields and MCP `kind`
reject non-whitespace control characters. Relation targets also
refuse whitespace control characters. All metadata still passes through the
wiki sanitizer and admission chain, and attribution comes from authentication.

For example, these fields can be added to either surface's existing request:

```json
{
  "kind": "decision",
  "entities": ["SQLite", "Writer actor"],
  "abstract": "One writer owns every SQLite mutation.",
  "relations": {"fixes": ["gotchas/concurrent-writes.md"]}
}
```

Both surfaces replace the whole page, including its editable metadata.
Omitted fields do not inherit the previous version; empty entity lists and
relation objects clear those fields. Empty `kind` or `abstract` strings omit
those keys. Existing requests without the new fields remain valid, including
the admin's existing top-level `kind`. Arbitrary frontmatter, scope and author
fields are never copied from metadata. Without `expected_page_id` or
`create_only` the write is an unconditional replacement. Only
`memory_write_page` accepts those preconditions; `POST /admin/write-page` has
neither. See
[Change a page only if nobody else has](programmatic-memory.md#change-a-page-only-if-nobody-else-has)
for conditional writes.

### Patching a section without replacing the page

`memory_read_page` returns at most 100 parsed Markdown headings in `sections`,
each with its `anchor`, `level`, and exact `sha256`; `sections_truncated` says
whether more exist. Headings inside fenced code are not sections. Read the page
immediately before patching, then call `memory_patch_page` with that required
`page_id`:

```json
{
  "workspace": "customers",
  "project": "aran",
  "path": "STATUS.md",
  "expected_page_id": "019...",
  "edits": [
    {
      "op": "append",
      "target": "section",
      "anchor": "Recent actions",
      "content": "| 2026-10-26 | SMTP fixed |"
    },
    {
      "op": "replace_section",
      "anchor": "Open issues",
      "section_sha256": "9f...",
      "content": "| None |"
    }
  ]
}
```

Use `target: "end_of_page"` with no anchor to append at EOF. `add_section`
requires an ATX level 2-6 heading such as `## Decisions` and rejects a heading
that already exists after whitespace/case normalization. Duplicate headings,
stale page or section tokens, overlapping targets, failed admission, or a
concurrent writer reject the whole batch. Existing frontmatter is preserved;
the final body still passes through the normal sanitizer, admission,
attribution, versioning, index and git-checkpoint path.

## Install the routing snippet and Agent Skills

From an agent, say:

```text
Install ai-memory routing into this project.
```

The agent calls `memory_install_self_routing` and receives the slim
`markered_block`, marker strings, rules-file hints, managed skill payloads,
skill target hints, and overwrite guidance. It then uses its normal file-edit
tool to preserve unrelated user content, replace or append the
`<!-- ai-memory:start -->` / `<!-- ai-memory:end -->` block only when the
marker delimiters appear alone on their own lines, and write each managed skill
below the selected skill root. Skill files are ai-memory-managed only when they
contain the managed marker, so unmanaged same-name skills should not be
overwritten unless the human explicitly forces replacement.

The installed snippet and all project-scoped managed skills use the same
client-aware scope rule. A session-aware bridge that forwards the real hook
session id may omit scope for the current repository. A static MCP client must
send exact `workspace` and `project` values on every project-scoped call, even
when lifecycle hooks are installed, because hooks alone do not identify the MCP
request. Declare both names in the nearest [`.ai-memory.toml`](marker-file.md),
or obtain them from the operator/server configuration; never infer them from a
directory name or rely on the server's last active project. Global query and
global preference modes still omit project scope arguments.

From a terminal:

```bash
ai-memory install-instructions
ai-memory install-instructions --target AGENTS.md
ai-memory install-instructions --print
ai-memory install-instructions --no-skills
```

`install-instructions` installs or updates managed skills by default. Use
`--no-skills` only when you intentionally want a snippet-only refresh.
The CLI replaces only the markered ai-memory block, preserves unrelated content,
and writes a timestamped backup before changing an existing instruction file.
`install-instructions --print` previews the instruction snippet only; use
`install-skills --print` to preview skill payloads. Skill flags mirror
`install-skills` with an `--skills-` prefix:
`--skills-scope project|global`, `--skills-agent claude-code|agents|devin|grok|both`,
`--skills-target-dir <dir>`, and `--skills-force`.

Auto-detect extends `CLAUDE.md` when it exists, `AGENTS.md` when it
exists, both when both exist, or creates `CLAUDE.md` when neither exists. Use
`--target AGENTS.md` for non-Claude-only projects. The skill target follows the
instruction target unless you override it: `CLAUDE.md` implies
`.claude/skills`, `AGENTS.md` implies `.agents/skills`, and both files imply
both skill roots. For Grok Build CLI, select `--skills-agent grok` so skills
install under its `.grok/skills` root; for GitHub Copilot CLI, select
`--skills-agent copilot-cli` so skills install under `.github/skills` (project)
or `$COPILOT_HOME/skills` (global, default `~/.copilot/skills`); for Hermes
Agent, `--skills-agent hermes`
installs under `.hermes/skills` (project) or `~/.hermes/skills` (global).

When a project keeps `AGENTS.md` as its canonical instruction file, give it a
`CLAUDE.md` whose first line is a bare `@AGENTS.md` import. Claude Code 2.1.277
and later reads `AGENTS.md` only when no `CLAUDE.md`, `.claude/CLAUDE.md`, or
`CLAUDE.local.md` exists in the working directory or any directory above it.
Without that import, sessions that read only `CLAUDE.md` (older Claude Code, the
AGENTS.md plugin disabled, the `claude-md` Project instructions setting, or a
developer's own `CLAUDE.local.md`) start without a block installed with
`--target AGENTS.md`, or any other project rule in that file. A prose "read
AGENTS.md" pointer does not load the file; it asks the agent to open it, which
leaves adherence to whether the agent does. See
[Claude Code memory](https://code.claude.com/docs/en/memory#agents-md).
`install-instructions` prints a note when it writes only `AGENTS.md` while a
`CLAUDE.local.md`, `.claude/CLAUDE.md` or `CLAUDE.md` in the directory or above
it would make Claude Code read that file instead, unless one of them imports
the `AGENTS.md` being written. Claude Code's user-level
`~/.claude/CLAUDE.md` does not count, including when `$HOME` reaches that
directory through a symlink. The import is resolved from the file that contains
it (`@AGENTS.md` in `.claude/CLAUDE.md` points at `.claude/AGENTS.md`; that file
needs `@../AGENTS.md`), `~/` expands from the home directory, and an `@path`
inside a code span or fenced block does not count. Imports are followed through
other imported files up to four hops, as Claude Code follows them.

To refresh only the managed Agent Skills:

```bash
ai-memory install-skills
ai-memory install-skills --scope global --agent agents
ai-memory install-skills --scope global --agent devin
ai-memory install-skills --scope global --agent grok
ai-memory install-skills --scope global --agent copilot-cli
ai-memory install-skills --scope global --agent hermes
ai-memory install-skills --scope global --agent antigravity
ai-memory install-skills --agent both --print
ai-memory install-skills --target-dir .custom/skills --force
```

For Devin, project-local skills are installed under `.devin/skills`. Global
Devin installs use `%APPDATA%\devin\skills` on Windows and `~/.devin/skills`
on non-Windows systems. For Grok Build CLI, project-local skills go under
`.grok/skills` and global under `$GROK_HOME/skills` (default
`~/.grok/skills`). A global Grok skill install also writes the first-turn
handoff rule `$GROK_HOME/rules/ai-memory-handoff.md` (default
`~/.grok/rules/ai-memory-handoff.md`); project-scope Grok skills and
`--target-dir` do not. For GitHub Copilot CLI, project-local skills go under the
repository's `.github/skills` and global under `$COPILOT_HOME/skills` (default
`~/.copilot/skills`); Copilot CLI already loads `AGENTS.md`, so it shares the
`AGENTS.md` instruction target. Google Antigravity reads workspace skills from
`.agents/skills` and has two global roots, `~/.gemini/config/skills` for the
App and IDE and `~/.gemini/antigravity-cli/skills` for the CLI; a global
`--agent antigravity` install writes both, and `uninstall` sweeps both.

Project-local skill roots are `.claude/skills` for Claude-compatible installs,
`.agents/skills` for cross-client installs, `.devin/skills` for Devin, and
`.grok/skills` for Grok, and `.github/skills` for GitHub Copilot CLI. Global
Claude/Agents roots are `~/.claude/skills` and `~/.agents/skills`; global Devin
roots are platform-specific as described above; global Grok is
`$GROK_HOME/skills` (default `~/.grok/skills`); global Copilot CLI is
`$COPILOT_HOME/skills` (default `~/.copilot/skills`). `uninstall` sweeps both
Copilot CLI roots and removes only files carrying the ai-memory managed marker,
so team-authored skills in `.github/skills` are never touched.
`--target-dir` points at an explicit skill root and bypasses scope/agent
inference. `--print` previews target paths and `SKILL.md` contents. `--force`
allows replacement of unmanaged same-name skills; without it, user-authored
skills are preserved. Uninstall removes ai-memory-managed skills from the
default project/global roots after marker validation; custom `--target-dir`
roots are a manual cleanup path.

This is prompt packaging only. ai-memory does not run a runtime skill router,
does not store durable memory in `SKILL.md`, and does not turn the
auto-improvement loop into a skill-authoring system. Durable knowledge still
lives in the wiki.

## Bootstrap an existing project

If you install ai-memory into a project that already has months of
history, the wiki starts empty. `ai-memory bootstrap` seeds it from the
existing repo history and docs.

```bash
export AI_MEMORY_SERVER_URL="http://localhost:49374"
ai-memory bootstrap --dry-run
ai-memory bootstrap
```

The bootstrap collector reads `git log`, the root README, `docs/`,
project rule files, and Rust module docs, then POSTs the selected
sources to the running server. It requires an LLM provider on the
server. See [Installation cookbook - bootstrap mid-project](install.md#bootstrap-mid-project)
for flags, token budgets, and source priority.

## Migrate from another memory tool

When replacing an existing memory system, treat the old data as untrusted
historical input until you curate it. Do not pipe raw transcripts or old memory
stores directly into ai-memory.

Migration checklist:

1. Export the old memory or history before changing hooks.
2. Keep the raw export as an archive, not as current project truth.
3. Scrub secrets, tokens, credentials, API keys, and raw logs that should not
   become durable memory.
4. Curate the useful material into reviewed Markdown pages under a temporary
   docs directory or directly into `concepts/`, `decisions/`, `gotchas/`,
   `procedures/`, `notes/`, or `_rules/`.
5. If this checkout might be ambiguous, add `.ai-memory.toml` to pin the intended
   workspace/project before importing or installing hooks.
6. Start `ai-memory serve` locally and confirm `ai-memory status` can reach the
   server before touching existing client configs.
7. Import curated material first; avoid importing the full legacy raw history.
8. Verify expected pages with full hybrid `memory_query`; use
   `ai-memory search` only when a terminal FTS5 lookup is sufficient.
9. Configure MCP and lifecycle hooks for one client at a time.
10. Only after ai-memory capture and retrieval work, disable the old memory
    hooks, plugins, or MCP servers.
11. Search each client config for stale references to the old tool and remove
    stale `Authorization` headers or env vars if bearer auth changed.
12. Restart each agent CLI after changing hooks, plugins, or MCP config.

Client cleanup hints:

- Claude Code: check plugins, hooks, old SessionStart injection, and MCP servers.
- Codex: check MCP config plus session/user-prompt/tool/compaction/stop hooks.
- Command Code: check `~/.commandcode/mcp.json` and the four stable lifecycle
  events in `~/.commandcode/settings.json`.
- Devin CLI: check `.devin/config.json`, `.devin/hooks.v1.json`, and
  `.devin/skills` for stale MCP, hook, or routing-skill entries.
- Gemini CLI and Antigravity CLI: check `settings.json` or equivalent hook/MCP
  config files.
- Kimi Code: check `~/.kimi-code/mcp.json` and the `[[hooks]]` entries in
  `~/.kimi-code/config.toml` (both under `$KIMI_CODE_HOME` when set) for stale
  MCP or hook entries.
- Kiro CLI: check the `hooks` objects inside `~/.kiro/agents/*.json` (v2),
  `~/.kiro/hooks/ai-memory.json` (v3), and `~/.kiro/settings/mcp.json` (all
  under `$KIRO_HOME` when set) for stale ai-memory entries.
- OpenCode, OpenClaw, and OMP: check MCP config and plugin/extension directories;
  move old memory plugins to a disabled/quarantine directory before deleting.
  OpenCode uses `ai-memory.ts` with direct `mcp` on V1 and
  `ai-memory-opencode2.ts` with `mcp.servers` on V2; the generic installer
  detects the installed major and removes only an incompatible sibling proven
  to be generated by ai-memory.
- VS Code Copilot, Claude Desktop, and Zed: these are MCP-only, so confirm
  whether the old tool was providing capture hooks elsewhere. Zed's MCP
  entries live under `context_servers` in its user `settings.json`.

If you want a visible startup reminder during the transition, keep it small. A
rules-file note such as "Active memory: ai-memory; legacy export is historical
reference only; use memory_query for retrieval" is safer than dumping large
legacy context into every session.

If you use the ChatGPT/Codex OAuth provider, sign in once before starting the
server with `AI_MEMORY_LLM_PROVIDER=openai-oauth`:

```bash
ai-memory auth login openai-oauth
ai-memory auth status
```

The login command stores only provider credentials in `<data_dir>/auth.json`.
It is separate from `AI_MEMORY_AUTH_TOKEN`, the machine-root Bearer used by
MCP, hooks, handoffs, workstreams, and machine calls to dual-auth APIs.

For GitHub Copilot, use the matching provider login before starting the server
with `AI_MEMORY_LLM_PROVIDER=copilot`:

```bash
ai-memory auth login copilot
ai-memory auth status
```

Copilot auth stores a GitHub user token, then the provider exchanges it for a
short-lived Copilot API token before each LLM call.

Device login (`openai-oauth`, `copilot` and `oidc-device`) waits up to
`--timeout-secs` seconds for authorization after the device code is issued
(default: 600). The limit covers device-authorization polling requests,
response reads and pauses between polls, including a provider's `slow_down`
backoff. Reaching it discards the polling result without changing stored
credentials. Discovery, the initial device-code request and OpenAI's exchange
of a code authorized within the deadline keep their own HTTP request timeout.

## Browse the wiki in a browser

Start the server with `--enable-web` and open
`http://<host>:49374/web`.

```bash
ai-memory serve --transport http --bind 127.0.0.1:49374 --enable-web
```

On macOS the [menu bar app](macos.md#scenario-d-menu-bar-app) already starts
the LaunchAgent with `--enable-web`; **Open Web UI** opens that same URL.

Docker compose users can add the flag to the service command:

```yaml
command: ["serve", "--transport", "http", "--bind", "0.0.0.0:49374", "--enable-web"]
```

The web UI is read-only: project list, per-project page tree,
breadcrumbs, rendered markdown, metadata, search, a link graph, and an
operator view of managed workstreams.
The header **Graph** link opens `/web/graph` (cross-project wikilinks);
each project page links to `/web/w/<ws>/<proj>/graph` (pages in that
project and the resolved links between them). Both are a server-rendered
SVG plus a table, with no JavaScript. On a project, page,
namespace, history or session view the header search box searches that project;
elsewhere it searches every project with FTS5, like
`memory_query(global=true)`, and the search page's "Search in" selector picks
a project, and its "Kind" and "Tier" selects (`kind=` and `tier=`) keep only
the pages of that kind or tier. A page shows an outline of its headings, an `expired` badge once its `expires_at` TTL has passed, the pages that cite it and
the pages it cites; a folder's namespace listing sorts by recent update or by
title and shows 25 pages at a time. The tree, the recent-activity list and a
namespace listing leave out a page whose `expires_at` TTL has passed, as search
does; its own URL still opens it. The search on a project page ranks with
the same hybrid ranker as `memory_query` for that project, and its "Explain
ranking" box shows each hit's stream ranks, RRF contributions, authority and rerank score, plus the per-stream timings.
Root also gets `/web/workstreams`: a bounded, attention-first overview of
managed workstreams across projects. It derives `active`, `interrupted`,
`completed`, and `empty` only from run leases, exit state, and imported
event counts; it does not interpret transcript prose or expose checkout paths,
repository fingerprints, lease owners, or native session ids. This operator
aggregate does not change `ai-memory resume`, whose discovery remains scoped
to the current checkout. Root also gets `/web/health`: hook counters (429s included), the writer queue,
the local spool and recovery journal, the consolidation queue, embedder and
provider state, and the failing checks with the command that fixes each. Each page links
to its history (`/web/w/<ws>/<proj>/h/<path>`): every stored version, newest
first, with the author where the write was attributed, and a unified diff
between any two of them (by default the latest against the version it
replaced). A version another write replaced stays listed; an expired one does
not, and a page whose TTL has passed has no history. The project page
lists recent sessions, and each opens a timeline
(`/web/w/<ws>/<proj>/s/<session-id>`) that pages through the session's
captured observations in order, with first/prev/next/last links, a jump field
and, when JavaScript runs, a slider and `j`/`k` keys to step one observation
at a time. Observations are shown as plain text. In rendered
pages, `[[wiki links]]` become clickable links to the target page.
`[[path]]`, `[[path|label]]`, `[[project:path]]`,
`[[workspace/project:path]]`, and `[[_global:path]]` are all supported
(resolved against the current page's project unless the target carries
its own scope; `[[_global:path]]` always names the reserved `_global`
project in the default workspace).
`[[…]]` stays literal inside fenced code (` ``` ` and `~~~` close
only by their own glyph), inline `` `…` `` code, and 4-space-indented
code; external schemes inside the brackets (`http://`, `https://`,
`mailto:`, `data:`, `javascript:`, `vbscript:`, `tel:`, `file:`)
stay literal too.

With no bearer token configured on loopback (an install from before 3.0 that
never ran `ai-memory auth enable`), the built-in wiki remains anonymous. A
fresh install has a bearer token and no human accounts, so a browser gets a
Basic sign-in prompt: any username, and the contents of
`<data_dir>/auth-tokens/default` as the password. The browser resends those
credentials itself, so no cookie holds the token. That GET-only browser path
stops immediately after
a human password or completed bootstrap exists. For human-authenticated
administration, create the first root account ([first-time
root](install.md#first-time-root-greenfield)) and serve the compiled admin SPA
with `--web-ui-dir`: it signs in through `/auth/login` and uses an HttpOnly web
session plus CSRF protection. Browser-stored Bearers remain unsupported. The
inbox composer can use an explicitly entered write token for a single message
when the legacy sign-in is
read-only; it clears the token after the request and never persists it.
Outside this inbox action, `AI_MEMORY_AUTH_TOKEN` and `aim_` API keys remain
machine credentials sent as `Authorization: Bearer <token>` by MCP, hook, handoff,
and workstream clients.

To host the web UI under a URL subpath behind a reverse proxy, the
`--base-path` / `--web-slug` flags do the work. See
[`docs/frontend-api.md`](frontend-api.md#6-custom-ui-hosting-and-base-paths)
for the flag semantics and
[`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath)
for the proxy-side walk-through.

![Project list homepage with four projects shown as cards with page counts and last activity.](web-projects-home.png)

![Project view with folder tree, kind badges, and recent activity.](web-project-view.png)

## Inspect the raw wiki

The wiki is plain markdown plus git history.

```bash
docker exec ai-memory ls /data/wiki/sessions/
docker exec ai-memory cat /data/wiki/sessions/<uuid>.md

# Open in Obsidian or any markdown viewer:
docker cp ai-memory:/data/wiki ./my-ai-memory-wiki

# Time-travel:
docker exec ai-memory git -C /data/wiki log --oneline
```

Watcher reindexing shares the per-page mutex used by writes and batches on the
same Wiki handle and its clones. It waits before reading the file, holds that
mutex through the SQLite upsert, and releases both page and global mutation
guards before embedding. Different paths can proceed concurrently, and the
watcher never rewrites the page. External editors and independently constructed
Wiki handles do not take these locks; external edits can still race with a
server write or reindex.

## Move a session to another project

A session captured under the wrong project (a `cd` into a scratch
directory, a subagent started elsewhere) can be reattached without a
reorg of the whole store:

```bash
ai-memory move-session <session-id> --to my-project            # dry run
ai-memory move-session <session-id> --to my-project --confirm  # apply
ai-memory move-session --from-project tmp --to my-project --confirm
```

The session, its observations, handoffs, consolidation jobs and its
`sessions/<id>.md` page move together; see
[`docs/lifecycle-ops.md`](lifecycle-ops.md#move-session) for the page modes,
guards, and what stays behind.

## Repair backfilled session timestamps

A `backfill` run from before it carried the transcript's own event time dates
every imported session at import time, flattening the whole imported history
onto one day. Re-reading the local transcripts corrects it:

```bash
ai-memory repair-backfill-timestamps --project my-app            # dry run
ai-memory repair-backfill-timestamps --project my-app --confirm  # apply
```

It matches transcripts to sessions by id, never touches `observations` or
pages, never assigns an end time to a still-open session, and never proposes
a time in the future; see
[`docs/lifecycle-ops.md`](lifecycle-ops.md#repair-backfill-timestamps) for the
validation rules and the exact request/response shape.

## Project consolidation preferences

Create `_prompts/consolidation.md` in a project's wiki when its compiled pages
need stable style, terminology, emphasis, or noise-filtering preferences. For
example, its body may ask for Portuguese titles or omit routine CI output.
Automatic consolidation and both manual modes read only the target project's
page. Passing `instructions` to `memory_consolidate` replaces that page for one
call without modifying it.

The page and per-call value remain untrusted project data. ai-memory applies the
configured sanitizer, caps the value at 2,000 characters, and JSON-encodes it in
the LLM user message. Both consolidation system prompts permit only advisory
style, terminology, emphasis, and noise-filtering effects; the value cannot add
facts, authorize disclosure or tool use, or override schema, evidence, and
output rules. TTL-expired preference pages are ignored. When there is no active
page and no argument, ai-memory appends no preference block.

## Rules, memory and the profile

The agent works from an ordered precedence; each level fills gaps in the ones
above it and never overrides them:

1. The user's current instructions in the conversation.
2. The repository's rules file: `CLAUDE.md` for Claude Code; for Codex, Devin
   CLI, OpenCode, Cursor, Gemini CLI, Grok Build CLI, Kimi
   Code, Kiro CLI, and Command Code it is usually `AGENTS.md`.
3. The project's memory: its pages, `_rules/`, decisions and gotchas.
4. The cross-project profile: the user's usual choices from other projects
   (`docs/cross-project-profile.md`).

So a hard rule that must hold in a repository belongs in its rules file. Where
no hard rule exists, the agent relies on memory instead of asking again: the
project's own pages first, then the profile, which is what lets a new project
start with the user's usual choices. A preference that applies to every
project goes to the profile (`memory_write_page` with `scope: "profile"`), not
into each repository's rules file. Memory still never authorizes anything on
its own: it sets defaults, and the user and the rules file decide.

Durable project rules belong in the agent's rules file, not only in the
wiki.

The consolidator classifies compiled observations as `decision`,
`fact`, `rule`, or `gotcha`. Rule-tagged pages are routed to
`wiki/_rules/<slug>.md`, and `memory_lint` reports a suggestion when a
rule looks durable enough to copy into `CLAUDE.md` or `AGENTS.md`.

ai-memory never edits the rules file on its own. The lint suggestion is
the whole workflow: copy the rule if it should apply every turn, ignore
it if it was temporary context.

## Repo-native decision records

Some projects keep the reasoning behind the code in the repository itself: an
ADR directory such as `docs/adr/`, maintained by hand or by a dedicated ADR
tool/MCP server (e.g. [joshrotenberg/adrs](https://github.com/joshrotenberg/adrs)),
or a [Keep the Why](https://github.com/oliver-zehentleitner/keep-the-why)
`context/` tree (decisions, rejected alternatives, constraints, reviewed in
pull requests). Three facts frame how such a record and ai-memory interact:

1. **ai-memory never touches files in your repository.** Its wiki lives
   in the server's data dir; the background jobs (consolidation,
   curation, retention decay, auto-improvement) read and write wiki
   pages only. A decision-record directory in the repo is outside
   ai-memory's write surface. Run both side by side: the repo owns the canonical record, ai-memory owns
   cross-session recall.

2. **Keep the record directory out of capture.** An agent reading the
   record is captured like any other file read, and consolidation compiles
   what it saw into wiki pages, including a `decisions/` page that says
   "active" long after the repo has superseded it, ranked first by
   `memory_query` because it matches the topic. List the directory in the
   marker's `[capture]` section so the copy is never made:

   ```toml
   [capture]
   ignore_paths = ["docs/adr/**"]   # or ["context/**"] for Keep the Why
   ```

   The repo owns that record; a compiled copy goes stale the moment the
   repo moves. Details and bounds in [`docs/marker-file.md`](marker-file.md),
   including what shell matching cannot see and how to exclude large tool
   results an agent saves and re-reads from another path.

3. **Wiki pages marked `pinned: true` are immutable to automation.**
   Retention decay and curation skip them, multi-page consolidation
   skips any update whose path names one (with a warning in the server
   log; `_slots/` keep their own state/invariant regime), and the
   auto-improvement apply path hard-refuses to rewrite them (the proposal
   is recorded as a conflict with the reason). Unpinning is the explicit
   opt-out.

For a project without a repo-side record, decisions go *in* the wiki, and
the managed durable-pages Agent Skill teaches agents the recipe:
`decisions/<slug>.md`, ADR structure (Status / Context / Decision /
Consequences, including rejected alternatives), `pinned: true`, and
supersede-by-new-page instead of editing history. Ask an agent to "record this as an architectural
decision" and the skill does the rest. The structured shape also
retrieves noticeably better through `memory_query` than free-form
prose.

## Investigate token usage in the dashboard

Filter the complete authorized ledger before pagination by UTC period, exact
model, harness and session. From is inclusive; Until is exclusive. Turn
and tool filters connect a response to reported native context. "Turn & tools"
separates a requested tool, an input result and a same-turn association, and
shared context does not prove that a token cost belongs to one tool. Update
native clients as well as the server to collect these fields. Old rows with no
source metadata stay unknown. The
[feature guide](features.md#token-accounting-and-investigation) explains
timestamp fallbacks, filtered totals and capture prerequisites.

Open a project under `/web` and select the Token usage tab. Updated native Claude Code and
Codex hooks send measured response usage on `post-tool-use`, `stop`,
`pre-compact` and `session-end`. The server stores usage as it arrives, including while a
session is open. The panel and session totals refresh every ten seconds. To
enable capture, update the native client on each agent machine as well as the
server.

The metric cards are navigation controls. Workspace cards sort the project
list by the selected count, and Open sessions also selects projects that have
an unfinished session. Project cards open the matching Agents, Sessions,
Knowledge, or Token usage tab. Captured events sorts sessions by event count,
and Open sessions filters the table to unfinished sessions. On an agent page,
token and call cards open the project's response ledger already filtered to
that harness.

Responses are deduplicated by their native identity, so a partial update raises
the stored counters instead of adding the same response again. Fresh input
excludes cache reads. Search, signal filtering and recent ordering query all
captured responses before fifty-row pages are selected. Page links keep those
filters, and search results and totals stay separate. Responses link to their
session and show model, input, output, cache and total tokens. The panel flags
contexts of at least 100,000 tokens, outputs of at least 20,000 tokens, and
responses above three times the project's captured-response average once five
responses exist. These are hints for investigation, not confirmed anomalies or
currency costs.

Live extraction reads forward in bounded 4 MiB batches and reports at most 256
responses per event. A cursor under the client's data directory advances only
after the local enqueue is durable. Later hooks continue any backlog, including
records outside the previous byte window, and repeated responses stay
deduplicated. A large backlog needs several events, and a session with no
further hooks does not drain its transcript on its own. Incomplete lines wait
for completion, and lines over 4 MiB are skipped in bounded chunks. Truncation
or changed checkpoint content restarts the scan. Update the native binary on
each agent machine, because updating only the server does not change transcript
capture. Reports appear after the agent writes usage. Missing or deleted
records and unsupported formats stay unknown.

The ledger keeps counters, response and model identifiers, and bounded native
turn, response-time and tool references when reported. It excludes prompts, tool
arguments and outputs. Model and tool labels are sanitized before their length
limit is applied, and private or malformed context identifiers are refused.
Project authorization and session-owner filters apply before ranking or
aggregation. Session-end totals remain as a fallback for history.

The header's Light / Dark / System control stores the preference in the browser.
System follows operating-system changes, and a manual choice overrides it.

### Explore agents and sessions

The workspace sidebar links to projects, the inbox, workstreams, the knowledge
graph, learning review and server health on desktop and mobile.
Project cards show the recorded harnesses and can be filtered by name, open
sessions or queued messages. An open session has no recorded end event; the agent may no longer be online.

Project tabs show one section at a time. Overview summarizes capture and pending
messages, Agents groups harnesses, Token usage investigates responses, Sessions
lists recent sessions, and Knowledge browses saved pages. The selected tab is
kept in the URL (`?tab=agents`, for example), so reload, shared links and
browser back and forward keep the same section. Section bookmarks such as
`#agents` still open the matching tab. The tab bar supports Left and Right,
Home and End, and wraps on narrow screens. Inbox and Link graph open separate
views.

In Agents, agent cards add up reported usage across sessions and show how many
sessions reported it. "Explore sessions" pages through one harness's sessions,
and a session opens to its response ledger and captured event timeline. Missing
usage stays unknown. Legacy session totals can exist without individual
response records, so coverage and captured-call counts are shown separately.

The response table can be searched by model, agent or response, filtered by
investigation signal, and sorted by token usage or capture time. These controls
query all captured responses in the current scope before fifty-row pages are
selected. Project and session usage refresh every ten seconds while visible, and
the filter controls and keyboard focus stay in place.

### Coordinate work from the browser

The Inbox lists the messages a selected project received and sent, fifty per
page, without claiming them. Received mail opens on pending messages, and "All
messages" or "Collected" show history. Page links keep the selected state.
"Waiting for agents" shows real queued counts and mailbox links for each
workspace and project you can access. By default the page opens a project with
queued messages, and the heading names the current mailbox. The sidebar
refreshes pending counts every ten seconds on visible pages, and its project
links open "Awaiting collection". Collected mail leaves the count. The count is a
queue indicator, not human read or unread tracking. "Message state" filters the
history before pagination. Reading through a notification does not claim mail.
Expanding a row shows its plain text. New message picks a source project and a
recipient harness from the agents that already have sessions in visible
projects. Sending needs write access to both projects and goes through the
configured sanitizer and message admission policy. Browser sessions use the
existing CSRF checks. A queued message has an Edit button. The message leaves the
queue while you change its subject or text. Saving or discarding returns it to
its original place. A message an agent already collected
cannot be edited.

A queued request does not launch an agent. The addressed harness collects it the
next time it checks its inbox, and "Collected" records that claim, not that the
task succeeded. Static MCP clients pass their harness in the `agent` argument of
`memory_message_pop`. See [agent messaging](agent-messaging.md).

The navigation and drilldowns follow the overview-to-detail approach in
[Grafana](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/best-practices/)
and the session grouping documented by
[Langfuse](https://langfuse.com/docs/observability/features/sessions). They are
interaction references. The built-in interface is still Cargo-built HTML and CSS
with small vanilla JavaScript enhancements.
