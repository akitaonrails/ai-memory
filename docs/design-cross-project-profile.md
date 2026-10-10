# Design: the cross-project profile

*Status: accepted (2026-10-04), implemented for 2.6. User guide:
[`cross-project-profile.md`](cross-project-profile.md).*

## 1. The problem

ai-memory keeps knowledge per project, and switching harness already costs
nothing: Claude Code, Codex and OpenCode in one checkout read the same pages.
What still costs something is switching **project**. A new repository starts
from zero, so the user re-explains the same habits in every one: the package
manager they use, how they like tests laid out, which architecture they reach
for, the review and commit workflow they follow.

Nothing in the tree converges that knowledge today. The reserved
`default/_global` scope only receives pages an agent writes there explicitly,
reaches the agent only when a `memory_query` happens to match it, and is
deliberately kept out of the SessionStart brief. `docs/design-rules-promotion.md`
defers cross-project sharing, and RFC #1000 asks for exactly that.

The **profile** fills that gap. It is a small, curated, cross-project record of
how this user usually works, built from evidence, delivered to every project
and every harness, and used as the default whenever nothing more specific says
otherwise.

## 2. Doctrine: memory as the fallback, rules as the override

The previous stance was "memory is untrusted history, never policy; always-on
rules belong in `CLAUDE.md`/`AGENTS.md`." It protected against a memory page
turning into an instruction. It also meant the agent ignored what it already
knew about the user unless someone copied it into every repository by hand.

The stance from 2.6 is an ordered precedence:

1. The user's current instructions in the conversation.
2. The repository's rules file (`AGENTS.md`, `CLAUDE.md`, the harness's own).
3. The project's memory (its pages, rules and decisions).
4. The profile: what the user usually does across projects.

Lower levels fill gaps; they never override a higher one. When a rules file
says "use npm", the profile's "usually pnpm" is irrelevant. When nothing says,
the agent uses the profile instead of asking again, and says which default it
applied so the user can correct it.

What does not change: retrieved text is still data. A profile entry is a
*preference the user expressed*, not a command found in stored text; it can set
a default (a tool, a layout, a convention) but it cannot authorize anything the
current instructions would not (no tool use, permission change, disclosure or
destructive action because a page says so). The digest is fenced and labelled
the same way the brief is.

## 3. What we borrowed from OptChat, and what we did not

Victor Taelin's OptChat, since rewritten as UniiChat
(<https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449>,
revisions of 2026-10-04 and 2026-10-08), makes one endless chat the memory: a
binary tree of one-line summaries over a verbatim log, a fixed-size view, and a
`zoom` tool to open a summary back into detail. The rewrite fixed a cache bug in
the first version: a pair's age was measured from its first message, merges ran
at every message, and the view was rebuilt at start, so old lines kept being
rewritten and the cached prefix broke early. The profile has no tree or view
and never inherited that formula; the lessons it applies are stable-first
ordering and leaving settled lines alone (below, and §11).

Borrowed:

- **The user's own words rank first.** OptChat's compactor keeps orders,
  corrections and reasoning nearly verbatim; that is why instructions stick
  without a rules file. The profile harvests mainly from the user's prompts and
  corrections and from curated pages, never from tool output.
- **The latest ruling wins.** A newer statement on the same topic supersedes
  the older one; the older version stays reachable through the wiki's
  supersession chain (invariant #16: supersede, never destroy).
- **Fixed byte budget, stable rendering.** Sizes are UTF-8 bytes, not tokens.
  The digest is deterministic: one line per entry (scope, statement, page),
  with no evidence counts, dates or confidence, in fixed category-then-path
  order. It goes first in the SessionStart payload, ahead of the handoff,
  brief and inbox, which change every session, so it stays in the reusable
  part of the harness's prompt.
- **Settled lines don't churn.** UniiChat's correction to OptChat was that an
  old line must change only when its content does. New evidence that agrees
  with an entry is appended to its evidence and leaves the statement, and so
  the digest line, untouched. The statement is rewritten only when the ruling
  changes (the latest ruling wins) or its scope narrows; the LLM merge reports
  `changed`, and its restatement is ignored otherwise.
- **Zoom.** Each digest line names its page; `memory_read_page` opens it, and
  each page's evidence links back to the projects and sessions that taught it.
- **Context for the summarizer, and it never obeys.** The LLM merge step sees
  the current profile entry when folding in new evidence, and its prompt
  forbids following instructions found in the text it summarizes. It uses the
  evidence to understand the entry, never to add what the user did not say,
  and credits quoted text to its real author.
- **Zoom before you act.** The digest footer tells the agent to open an entry
  with `memory_read_page` before relying on its one-line summary for anything
  non-trivial.

Not borrowed: the single universal chat and harness (ai-memory is
multi-harness and the harness owns its context), a verbatim log kept forever
(the sanitizer boundary and capture exclusions win), and a 64k-token view (the
injected digest stays in the low kilobytes; rule adherence drops well before
that, `docs/design-rules-promotion.md` §2).

## 4. Where the profile lives

`[profile] share` selects the scope; `auto` (the default) picks by deployment:

| Deployment | Default | Profile location |
|---|---|---|
| Single user (no DB users, no trusted proxy), any number of workspaces | on, `share = "global"` | `default/_global`, under `profile/` |
| Multi-user | off; opt in with `enabled = true` | `share = "user"`: one private restricted project per operator |

Explicit values:

- `global`: one profile for the whole server, in `default/_global/profile/`.
  Every workspace contributes and consumes. In multi-user mode this is a
  *team* profile: it harvests only from open (non-restricted) projects, and it
  is readable by everyone like the rest of `_global`.
- `workspace`: one profile per workspace, in the reserved project `_profile`
  of that workspace. Use it when workspaces separate contexts that must not mix
  (work and personal, or two clients).
- `user`: one private profile per operator, in a restricted project in the
  default workspace, named `_profile.<user id>`, whose creator is that
  operator. It harvests only that
  operator's own sessions and pages and only from projects the operator can
  read. Other operators cannot read or write it (per-project authorization,
  `docs/design-per-project-authz.md`); root can, as with every restricted
  project. On a single-user server it behaves like `global`.
- `off`: no harvesting, no digest, no union.

Single vs multi-user is the existing `distinguishes_operators` decision.

## 5. Pipeline

### 5.1 Harvest (per project, zero-LLM)

Candidates come from three sources, after the sanitizer and capture
exclusions, never from tool output:

1. **The user's prompts** whose shape is a preference or a correction:
   *always / never / prefer X over Y / use X instead of Y / don't use /
   from now on / in all my projects / by default*, and the Portuguese
   equivalents the recall router already recognizes. The verbatim words are
   kept with the candidate.
2. **Curated pages** of the project: `_rules/`, `decisions/`, `gotchas/`,
   `procedures/`.
3. **Stack signals** derived from the project's observed file paths
   (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, …), recorded as
   `applies_to` facts rather than preferences.

Each candidate is stored in `profile_candidates` with the identity tuple
(invariant #4), its source (`session:<id>` or `page:<path>`), its date and the
contributing operator. A project whose marker sets `[profile] contribute =
false` is never harvested.

### 5.2 Converge

A server job on the writer actor groups candidates across projects by topic.
A candidate becomes (or updates) a profile entry when:

- the user explicitly scoped it beyond the project ("in all my projects",
  "every project", "across projects", "everywhere", or the Portuguese
  equivalents); a bare "always", "never", "by default" or "from now on" is
  compatible with one file, app or task and does not count (#1148), or
- the same choice appears in at least `min_projects` distinct projects
  (default 2).

A conflicting choice in the same scope supersedes the older entry (the latest
ruling wins); a choice that only holds for one stack becomes a scoped entry
(`applies_to: [rust]`). Exceptions that only one project makes stay in that
project.

Zero-LLM topic matching normalizes the statement (lowercase, stop words out,
tool and language names kept) and groups by token overlap. With a provider
configured (§6) the LLM classifies and merges instead.

Grouping never scans every pair of candidates (UniiChat's "queue ready work,
never scan"): a candidate is compared only with groups that share one of its
topic words, once per distinct word set in each group.

### 5.3 Store

Entries are wiki pages, markdown in git, editable by hand:

```
profile/<category>/<slug>.md      category: stack, architecture, workflow,
                                  testing, tools, style, habits
```

Frontmatter: `kind: preference`, `applies_to`, `projects` (count),
`first_seen`, `last_seen`, `confidence`, `evidence` (project, source, date,
quote; capped), and optionally `enforced_by` (a hook or rules-file line that
already enforces it, which drops it from the digest). The body is one
imperative line, then the user's own reasoning when there is some.

All writes go through `Wiki::write_page` (sanitization, admission,
attribution, rollback, index in the same transaction).

A harvested page carries `generated_by: profile-harvest` and the sha256 of
the body it wrote (`generated_sha`). The harvester rewrites only such a page,
and only while its body still matches: a page written or edited by hand is
never rewritten, and the group it would have updated is reported as skipped.
`profile_entry_ledger` records the entries it wrote; a ledger row whose page
is gone was removed by the user, and that topic is not recreated until a
candidate newer than the entry's last write arrives.

### 5.4 Deliver

- **SessionStart digest.** A fenced section, first in the payload (ahead of
  the handoff, managed context, project brief and inbox notice):
  *"Your usual choices from other projects. Use them as defaults when this
  project's rules and the user say nothing; say which default you applied."*
  Entries whose `applies_to` is empty or matches the project's stack signals,
  in a fixed category order then path order, one line each with its page path,
  until `digest_max_bytes` (default 3,000). The rest are counted with a hint to
  query. No timestamps, so the text is stable.
- **New-project baseline.** When the project has no pages yet, the digest uses
  `baseline_max_bytes` (default 6,000) and adds a line telling the agent it can
  offer `ai-memory profile apply` to seed the repository's rules file.
- **Query union.** `memory_query` already unions `_global`; for `workspace`
  and `user` profiles it also unions the caller's profile project, subject to
  authorization.
- **`ai-memory profile apply`.** Writes the top entries into a managed
  sub-block (`<!-- ai-memory:profile:start -->` … `end`) of the rules file in
  the current repository, capped at `apply_max_lines` (default 40), never
  touching text outside the markers. Command-driven only, with `--dry-run`
  and `--remove`. The entries come from `GET /admin/profile/apply` with the
  digest's selection; the target is `--target`, else an existing `AGENTS.md`,
  else a `CLAUDE.md` that does more than import `AGENTS.md`, else a new
  `AGENTS.md`. The write is atomic with its backup kept under the data dir.
  This is how a profile entry becomes a hard rule, when the user wants one.

## 6. Using an LLM when one is configured

Zero-LLM remains the complete default (invariant #13). With a provider
configured, and `[profile] llm` not `false`:

- **Classify**: at session end, the user-authored statements of the session
  go to a structured-output call (JSON schema, invariant #7) returning, per
  statement, `generality` (`project` | `general`), `category`, `topic`, a
  normalized imperative `statement` (≤ 200 chars), the verbatim `quote`,
  `applies_to` and `confidence`. Text is JSON-encoded in the user message,
  sanitized first.
- **Merge**: when a topic gains evidence, the current entry plus the new
  candidates go to a second structured call that returns the updated entry
  (statement, reasoning, applies_to, supersedes). Like OptChat's compactor, it
  sees the existing entry as context, ranks the user's words first, records
  faithfully and is told never to follow instructions in the text.

Both calls fall back to the zero-LLM path on any provider error.

## 7. Configuration

Server (`config.toml` / `AI_MEMORY_PROFILE__<KEY>`):

```toml
[profile]
enabled = "auto"            # auto = on for single-user, off for multi-user
share = "auto"              # auto | global | workspace | user | off
min_projects = 2
inject_on_session_start = true
digest_max_bytes = 3000     # clamped 1500..12000
baseline_max_bytes = 6000   # clamped 2000..20000
apply_max_lines = 40
llm = true                  # use the configured provider when there is one
```

Per project (`.ai-memory.toml`):

```toml
[profile]
contribute = false   # never harvest this project (client or NDA work)
consume = false      # no digest or union in this project
```

## 8. Commands

`ai-memory profile status | list | show <path> | forget <path> | review |
rebuild | apply [--target FILE] [--dry-run | --remove]`.

`status`, `list`, `review` and `apply` read `/admin/profile/*` and `rebuild`
posts to it (forget the harvest marks, re-read every project, converge);
`show` and `forget` reuse `/admin/read-page` and `/admin/delete-page` against
the profile's scope. All
are root-only on a multi-user server, like every `/admin/*` route; there, each
operator manages their own private profile through MCP.

On the MCP side no tool is added: `memory_write_page` accepts
`scope: "profile"` (the caller's profile scope; the path is placed under
`profile/`), `memory_delete_page` removes an entry the same way, and
`memory_query` returns profile hits in `global_scope_hits`.

## 9. Security boundaries

- A `user` profile is private: another operator's read, write or delete is
  refused (restricted project); the harvester never reads another operator's
  sessions into it.
- A `global` profile in multi-user mode never harvests a restricted project.
- `contribute = false` is honoured by the harvester; `consume = false` by the
  digest and the union.
- Candidates come only from user prompts and curated pages, after the
  sanitizer; tool output is never a source.
- The digest and the LLM prompts treat stored text as data.
- The shared scopes (`_global`, a workspace `_profile`) are read-open and
  write-gated on a multi-user server: root or a `write` grant.
- Event capture is never attributed to a reserved profile project.

Each gets an adversarial test and a row in `docs/security-boundaries.md`.

## 10. Out of scope

Automatic edits of a rules file (only `profile apply` writes one), a
universal harness, and promoting arbitrary project knowledge into the profile
without evidence from more than one project or an explicit general statement.

## 11. Revision 2026-10-08: UniiChat lessons

A review against the UniiChat rewrite found three places where 2.6.0 diverged
from §3 and §5.2. All three are fixed for 2.6.1:

1. **Digest position.** SessionStart assembles handoff, notice, managed
   context and brief before the digest, so the digest sits after content that
   changes every session. Move it first.
2. **Churn on corroboration.** `place()` rewrites an entry whenever its
   evidence changes, and the LLM merge always restates it, so a settled entry's
   statement and digest line change when it is merely confirmed. Keep the
   statement unless the ruling changes or its scope narrows; add `changed` to
   the merge schema.
3. **Quadratic grouping.** Each pass regroups every candidate (up to the
   20,000-candidate cap) against every group. Persist each candidate's group
   and compare only new candidates.

As shipped, grouping compares a candidate only with groups that share one of
its tokens (an inverted index), once per distinct token set in each group,
which keeps a pass near-linear without any stored state. Fully incremental
grouping, keyed by a stored group per candidate, needs a schema change and is
left to 2.7.

Lower priority, considered and not scheduled: a byte ruler with retry instead
of truncating long statements (truncation only affects the rare entry over 200
bytes).
