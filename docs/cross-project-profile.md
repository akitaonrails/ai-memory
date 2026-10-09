# The cross-project profile

ai-memory keeps knowledge per project, so switching harness costs nothing:
Claude Code, Codex and OpenCode in one checkout read the same pages. The
**profile** removes the other cost, switching *project*. It is a small set of
pages recording how you usually work — the package manager you reach for, how
you lay out tests, the architecture you prefer, the review workflow you follow —
and every project receives it at session start, through every harness.

When you start a new repository and describe what you want, the agent already
has your baseline, so you do not re-explain it.

Design and rationale: [`design-cross-project-profile.md`](design-cross-project-profile.md).

## How the agent uses it: rules first, memory as the fallback

The profile never overrides anything more specific. The order is:

1. Your current instructions in the conversation.
2. The repository's rules file (`AGENTS.md`, `CLAUDE.md`, the harness's own).
3. The project's memory (its pages, rules and decisions).
4. The profile: what you usually do across projects.

So when a rules file says "use npm", the profile's "usually pnpm" does not
apply. When nothing says, the agent uses the profile instead of asking again,
and says which default it applied so you can correct it.

A profile entry is a preference you expressed, used as a default. Like all
stored memory it is still data, not a command: it can pick a tool, a layout or
a convention, but it cannot authorize anything your current instructions would
not (tool use, permission changes, disclosure, destructive actions).

## Defaults

| Deployment | Profile | Where it lives |
|---|---|---|
| Single user (no database users, no trusted proxy), any number of workspaces | **on**, shared by every workspace | `default/_global`, under `profile/` |
| Multi-user (database users or a trusted identity proxy) | **off**; opt in with `enabled = true` | one private profile per operator |

A single operator gets one profile across everything, including several
workspaces. A server with several operators keeps the profile off until the
operator enables it, and then each person gets their own private profile:
nothing one operator's agents learn reaches another's.

## What you see at session start

The session start opens with a short fenced section, ahead of any handoff,
project brief or inbox notice. Those change every session; the profile changes
only when you do, so leading with it keeps it in the part of the harness's
prompt that can be cached:

```text
> 🧭 ai-memory: your usual choices (cross-project profile)
> Your usual choices from other projects. Use them as defaults when this
> project's rules and the user say nothing; say which default you applied. …
- [javascript] Use pnpm, not npm. (`profile/tools/pnpm.md`)
- Keep integration tests in tests/suite. (`profile/testing/layout.md`)
```

- **One line per entry**, each naming its page, so the agent can open it with
  `memory_read_page` for your reasoning and the evidence behind it.
- **Filtered by stack.** An entry with `applies_to: [rust]` only shows in a
  project whose activity touches Rust (detected from file names such as
  `Cargo.toml` and source extensions in its captured activity). A project with
  no activity yet gets every entry.
- **Bounded.** The section stays within `digest_max_bytes` (default 3,000
  bytes); entries that do not fit are counted, and `memory_query` finds them.
- **Stable.** The order is fixed (category, then path) and nothing
  time-dependent is printed, so the text is byte-identical between sessions
  until the profile changes, which keeps it in the harness's cached prompt.
- **A baseline for new projects.** A project with no memory of its own gets a
  larger budget (`baseline_max_bytes`, default 6,000 bytes) and a note that the
  agent can offer `ai-memory profile apply` to write your usual choices into
  the new repository's rules file.
- Entries marked `enforced_by` (something else, like a hook, already enforces
  them) are left out of the digest.

The profile also travels with retrieval: `memory_query` returns profile entries
in `global_scope_hits`, next to the rest of the global scope.

## Adding, reading and removing entries

Ask the agent to remember a standing preference ("always use pnpm in my
projects"); it writes it with `memory_write_page` and `scope: "profile"`. You
can also write one yourself:

```text
memory_write_page { "scope": "profile", "path": "tools/pnpm.md",
                    "body": "# Pnpm\n\nUse pnpm, not npm.\n\nWhy: workspaces and a strict lockfile." }
```

The path is stored under `profile/` (`profile/tools/pnpm.md`). Use a category
folder: `stack`, `architecture`, `workflow`, `testing`, `tools`, `style` or
`habits`; the digest lists them in that order. Optional frontmatter:

| Key | Meaning |
|---|---|
| `summary` | The digest line (otherwise the first line of prose in the body). |
| `applies_to` | Stack tags the entry is scoped to, e.g. `[rust]` or `typescript`. |
| `enforced_by` | Something already enforces it (e.g. `pre-push hook`); kept out of the digest. |

`memory_write_page` and `ai-memory write-page` set the last two directly:
`applies_to: ["rust"]` and `enforced_by: "pre-push hook"` in the tool call,
or `--applies-to rust --enforced-by "pre-push hook"` on the command line
(with `--workspace default --project _global --path profile/...` in the
default deployment). A tag must be one the profile detects in a project
(`rust`, `typescript`, `python`, ...); an unknown tag is refused rather than
silently hiding the entry. Like the other metadata, a rewrite that omits them
clears them. Both are refused on a page the digest does not read: one outside
`profile/`, or a `profile/` path in a project other than `_global` or a
profile project.

Entries are ordinary wiki pages, markdown in git: edit them by hand, and every
change is versioned. `memory_delete_page` with `scope: "profile"` removes one
(git keeps its history). Most entries, though, are learned on their own; see
the next section.

From the command line (root token on a multi-user server):

```bash
ai-memory profile status                 # on/off, where it lives, entries, digest size, opt-outs
ai-memory profile list                   # every entry with its scope tags
ai-memory profile show tools/pnpm.md     # one entry
ai-memory profile forget tools/pnpm.md   # remove one entry (stays removed until you say it again)
ai-memory profile review                 # entries with their evidence, habits still short of the bar
ai-memory profile rebuild                # re-read every project from the start (safe to repeat; removes nothing)
ai-memory profile apply                  # write the entries that fit this repo into its rules file
ai-memory profile list --user alice      # an operator's private profile (share = "user")
ai-memory profile list --workspace work  # a workspace profile (share = "workspace")
```

## Turning the profile into hard rules for one repository

The digest is a default the agent applies when nothing more specific speaks.
When you want your usual choices to be **rules** in a repository, written down
where every harness and every collaborator sees them, run this from inside it:

```bash
ai-memory profile apply --dry-run   # show the block and the file it would go to
ai-memory profile apply             # write it
ai-memory profile apply --remove    # take it out again
```

- **Which file.** `--target FILE` (relative to the repository root) wins.
  Otherwise an existing `AGENTS.md`; else an existing `CLAUDE.md`, unless it
  only imports `@AGENTS.md`; else a new `AGENTS.md`. The repository root is the
  nearest directory holding `.git` (a linked worktree writes its own checkout).
- **What it writes.** The entries that fit this repository, selected like the
  digest (scoped to the stacks the project's activity shows, entries enforced
  elsewhere skipped, a new project gets them all), at most `apply_max_lines`
  (default 40), each with the page it came from, between
  `<!-- ai-memory:profile:start -->` and `<!-- ai-memory:profile:end -->`.
- **What it never touches.** Anything outside those delimiters, including the
  `<!-- ai-memory:start -->` routing block and your own rules. Re-running
  replaces only the block, and leaves the file byte-identical when the profile
  has not changed. `--remove` takes the block out and restores the file; a file
  `apply` created for the block alone is deleted.
- **Backups.** The previous version of the file goes to
  `<data dir>/backups/profile-apply/`, never next to the file, so nothing extra
  shows up in `git status`.
- **Opt-outs.** A project with `[profile] consume = false` refuses `apply`.

It is the only command that writes a file in your repository, and it only runs
when you run it: ai-memory never edits a rules file on its own. Because the
block then sits in the rules file, it outranks the digest for that repository
(step 2 of the order above).

## How entries are learned

The server builds most entries itself, from evidence, in two steps: once
shortly after startup, after every session that ends (a few seconds later, so
a burst of session ends costs one pass), and hourly. Each pass only reads what
is new since the last one.

**1. Harvest.** From every project that contributes, it collects:

- **Your own prompts** that read as a preference or a correction: *always*,
  *never*, *prefer X over Y*, *use X instead of Y*, *don't use*, *from now on*,
  *by default*, *I usually*, *in all my projects*, and the Portuguese
  equivalents (*sempre*, *nunca*, *prefiro*, *em vez de*, *não use*, *a partir
  de agora*, *por padrão*, …). A sentence also has to name an action or be a
  comparison, so "the build always fails" is not a preference; questions and
  code blocks never are. A sentence that reads like agent output (markdown
  bold, or a `file.ext:line` reference) is skipped too: on hosts where agents
  brief each other through the prompt channel, that text is a lead agent's
  task brief or a pasted review, not you. Your verbatim words are kept as
  evidence.
- **Curated pages** of the project: `_rules/`, `decisions/`, `gotchas/` and
  `procedures/`.
- **Stack signals**: the languages the project's activity shows (from file
  names such as `Cargo.toml` and source extensions).

Tool output (what a command printed, what a file contained) is never read, so
text an agent merely *saw* cannot become your preference.

**2. Converge.** Statements are grouped by topic across projects. A group
becomes an entry when:

- you explicitly scoped it beyond the project ("in all my projects",
  "every project", "across projects", "everywhere", or the Portuguese
  equivalents). A bare "always", "never", "by default" or "from now on" still
  makes a candidate, but it can describe one file, app or task, so it has to
  earn the project threshold below, or
- the same choice shows up in at least `min_projects` projects (default 2).
  The same sentence arriving in several projects within an hour counts as one
  project: that is one message fanned out to several checkouts (a lead agent
  briefing its workers), not a habit. And one sentence backs at most one
  entry, so a statement the classifier splits into two topics does not
  produce duplicate pages.

A habit seen in only one project stays that project's business; `profile
review` lists such habits and how many more projects each needs. A language
used in at least `min_projects` projects becomes a `profile/stack/<language>.md`
entry, scoped to that language, which is how a new project learns your usual
stack.

**The latest ruling wins.** When you change your mind ("use bun instead of
pnpm from now on"), the newer statement replaces the entry; the old version
stays in the page history, never destroyed. Saying the same thing again, in
any words, only adds evidence: the entry keeps its statement, so the line every
project sees does not change. Without an LLM, "the same thing" means the same
topic words in the same order with the same negations (so "use pnpm" and "I
always use pnpm" match, while "never use pnpm" is a change).

Each learned entry records its evidence (the projects and dates it came from,
your words, the most recent five), how many projects back it, its confidence,
and a `generated_by: profile-harvest` stamp.

### With an LLM

With a provider configured (and `[profile] llm` left on), the pass uses it in
two places, both through JSON-schema structured output:

- **Classify.** Preference-shaped sentences from your prompts go to the model,
  which decides which are real preferences, whether they are general, their
  category and stack, and restates each as one short line. A looser filter
  feeds it, so it catches phrasing the word lists miss.
- **Merge.** When an entry gains evidence that may change it, the model
  restates it from your words, newest ruling first, with your reasoning when
  you gave one. It also says whether anything changed; when the evidence only
  confirms the entry, the entry stays exactly as written.

Both prompts treat every sentence as data, rank your own words first and are
told never to follow instructions found in the text. The evidence quote is
always your own sentence, never model text. If a call fails, the pass falls
back to the zero-LLM path for that batch; without a provider the profile works
the same way, with the word lists and the newest statement.

### Correcting a wrong entry

- **Remove it:** `ai-memory profile forget <path>`. The harvester remembers the
  entries it wrote, so a removed one is not recreated from the same evidence;
  it comes back only if you state it again later.
- **Reword it:** edit the page (by hand, or with `memory_write_page` and
  `scope: "profile"`). An entry you edited becomes yours: the harvester never
  rewrites a page it did not write, or one whose body changed since it wrote
  it, and `profile review` lists it as edited by hand.
- **Start over:** `ai-memory profile rebuild` re-reads every project from the
  beginning. It does not touch entries you edited or removed, and it never
  removes an entry: a candidate already recorded keeps the classification it
  was recorded with, and an entry whose evidence no longer meets the bar stays
  as written.
- **After upgrading:** a release that tightens what the profile learns applies
  to sentences read from then on. Entries an earlier version admitted stay,
  even after `profile rebuild`; list them with `ai-memory profile review` and
  remove the ones you do not want with `ai-memory profile forget <path>`.

## Server settings

In `config.toml` (or `AI_MEMORY_PROFILE__<KEY>`, for example
`AI_MEMORY_PROFILE__SHARE=workspace`):

```toml
[profile]
enabled = "auto"            # auto | true | false
share = "auto"              # auto | global | workspace | user | off
min_projects = 2
inject_on_session_start = true
digest_max_bytes = 3000     # clamped 1500..12000
baseline_max_bytes = 6000   # clamped 2000..20000
apply_max_lines = 40
llm = true
```

- **`enabled`**: `auto` turns the profile on for a single-operator server and
  off for a multi-user one; `true` and `false` override that for any
  deployment. A multi-user server keeps `auto` off even if you set `share`, so
  enabling it there is always an explicit `enabled = true`.
- **`share`**: where the profile lives.
  - `global`: one profile for the whole server, in `default/_global/profile/`,
    for every workspace. On a multi-user server this is a **team** profile:
    readable by everyone, writable only by root or an operator holding a
    `write` grant on `_global`.
  - `workspace`: one profile per workspace, in that workspace's reserved
    `_profile` project. Use it when workspaces separate things that must not
    mix, such as work and personal, or two clients. On a multi-user server it is
    write-gated like `_global` (root or a `write` grant on that `_profile`).
  - `user`: one private profile per operator, in a restricted project of the
    default workspace named `_profile.<user id>` whose creator is that operator.
    Only they and root can read or write it. On a single-operator server it
    behaves like `global`.
  - `off`: no profile at all.
  - `auto` (default): `global` on a single-operator server, `user` on a
    multi-user one.
- **`min_projects`**: how many distinct projects a choice must appear in before
  it joins the profile on its own (a choice you state as general — "in all my
  projects" — needs only one).
- **`inject_on_session_start`**: `false` keeps the digest out of session start;
  the profile still reaches `memory_query`.
- **`digest_max_bytes`** / **`baseline_max_bytes`**: the digest budgets, in
  UTF-8 bytes. The floors keep room for the fixed precedence and security lines
  plus a few entries.
- **`apply_max_lines`**: the most lines `ai-memory profile apply` writes into a
  rules file.
- **`llm`**: use the configured LLM provider, when there is one, to classify and
  merge entries; `false` keeps the profile fully zero-LLM.

`ai-memory profile status` prints the effective values.

### Common customizations

| You want | Set |
|---|---|
| Keep work and personal (or two clients) apart | `share = "workspace"` |
| No LLM involvement in the profile, even with a provider configured | `llm = false` |
| Only admit habits you repeat in more projects | `min_projects = 3` (or more) |
| A smaller digest at every session start | `digest_max_bytes = 1500` |
| The profile in retrieval only, no digest | `inject_on_session_start = false` |
| No profile at all on a single-user server | `enabled = false` (or `share = "off"`) |
| One project neither teaches nor receives it | `[profile] contribute = false` and `consume = false` in its marker |

### Enabling it on a multi-user server

```toml
[profile]
enabled = true          # share stays "auto": a private profile per operator
```

Each operator authenticates with their own API key; their agents write and read
their own `_profile.<user id>`. Requests made with the root token have no
private profile (there is no database user behind them). The `_profile` and
`_profile.*` names are reserved: an operator can name only their own private
profile explicitly (`workspace = "default"`, `project = "_profile.<their id>"`),
so nobody can create another operator's profile ahead of them. Profile projects
cannot be renamed and their access mode is fixed.

For a shared team profile instead, set `share = "global"` (or `"workspace"`)
and grant the curators `write` on `_global` (or that workspace's `_profile`):

```bash
ai-memory user grant --user alice --workspace default --project _global --level write
```

**Team-profile trust model.** A team profile reaches every operator's session
start, so the harvester holds it to a higher bar than a personal one: a
statement is admitted only on evidence from **at least two distinct
operators**, on top of `min_projects` (or a general statement). One operator
repeating "always run X before tests" in two open projects is not enough;
`profile review` lists such an entry as waiting for another operator. The
digest is headed **"team defaults"** rather than "your usual choices", so the
agent knows the defaults are shared, not this user's own. Restricted projects
never feed a team profile. Stack entries (which only describe the languages
seen in file names) keep the plain project threshold. Entries written directly
with `scope: "profile"` still need root or a `write` grant on the scope.

## Per-project opt-outs

In a project's `.ai-memory.toml`:

```toml
[profile]
contribute = false   # never learn from this project (client or NDA work)
consume = false      # no profile digest here, and no profile in this project's queries
```

Both default to on. Whenever a hook client finds the project's marker it sends
both values explicitly (`0` or `1`) at session start, and the server records
them on the project, so they apply to every harness. Removing the key from the
marker turns the setting back on at the next session start. A session start
that sends no value at all (no marker found, an older hook bundle, a client not
yet regenerated) leaves the recorded setting as it is: an opt-out is never
undone by a client that did not say anything. The recorded value follows the
marker of the most recent session start, so keep the setting in the marker every
checkout of the project shares (two checkouts whose markers disagree flip it
back and forth).

A marker that sets only `[profile]` keys is a settings boundary, like one
that sets `[briefing]` keys: an outer marker's `workspace`/`project` do not
apply through it. `ai-memory profile status` lists the projects that opted out
of contributing.

## Privacy and isolation

- A private profile (`share = "user"`) is a restricted project: another
  operator's read, write or delete is refused, and a query never unions it for
  anyone but its owner.
- The shared scopes (`global` and `workspace` profiles) are readable by every
  operator but writable only by root or a `write` grant, because they reach
  everyone's session starts.
- Event capture is never attributed to a profile project, whether the name comes
  from a directory or a marker.
- Entries go through the same sanitizer as every page; `contribute = false`
  keeps a project out of the profile entirely, including evidence harvested
  before you set it.
- Harvesting reads only your prompts and curated pages, never tool output.
- A private profile only ever learns from its own operator's sessions and
  pages, in projects that operator can read. A shared profile on a multi-user
  server (`global` or `workspace`) never learns from a restricted project,
  because everyone can read it.
- Text sent to the LLM goes through the sanitizer again first.

## Troubleshooting

- **No digest at session start.** Run `ai-memory profile status`: the profile
  may be off (a multi-user server needs `enabled = true`), it may have no
  entries yet, the project may set `consume = false`, or every entry may be
  scoped to a stack this project does not use. A session started inside the
  profile's own scope (for example in `_global` itself) gets no digest, since
  those pages are already its project memory.
- **Kimi Code.** Kimi re-fetches on every prompt; the digest is delivered on
  the first prompt of each session only.
- **The agent did not apply a default.** The rules file and your instructions
  win; check that the repository does not say otherwise. Run
  `memory_read_page` on the entry to see what it actually says.
- **A habit never became an entry.** `ai-memory profile review` shows it under
  "not in the profile yet" with how many more projects it needs; say it once as
  a general rule ("in all my projects, …") to admit it now, or lower
  `min_projects`.
- **An entry stopped updating.** It was edited by hand, so the harvester leaves
  it alone (`profile review` says so). Delete it to hand it back.
- **An opt-out came back on.** Only an explicit value changes it: check that the
  project's marker still says `contribute = false` / `consume = false` and that
  the agent runs in a directory under that marker (`ai-memory profile status`
  lists the projects opted out of contributing).
- **A team entry is "waiting for another operator".** A team profile admits a
  statement only when at least two operators said it (see the trust model
  above). Have a second operator confirm it, or write the entry directly with
  `scope: "profile"` if you hold the grant.
- **A private profile is unreachable after `ai-memory reindex`.** Reindex
  rebuilds a clean database from the wiki, and users are database-only state:
  operators created again get new ids, so an old `_profile.<old id>` project
  belongs to nobody (only root can read it). Its entries are still markdown in
  that project's wiki directory, `wiki/<workspace id>/<project id>/profile/`
  (entries always live in a `profile/` folder, so
  `find <data_dir>/wiki -type d -name profile` lists them). To carry them over, have the
  operator write one entry with `scope: "profile"` (which creates their new
  `_profile.<new id>`), then copy the old `profile/` files into the new
  project's wiki directory while the server runs; the wiki watcher indexes
  them into the new profile.
