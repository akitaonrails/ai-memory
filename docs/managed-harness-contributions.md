# Adding a managed harness

Managed-workstream support is narrower than MCP or lifecycle-hook support. This
release can manage Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, and OMP.
Gemini CLI, Devin CLI, Cursor, Grok Build CLI, and the other integrations in
the README support matrix do not become managed merely because ai-memory can
capture their hooks.

A managed adapter must preserve a harness's real native session, deliver the
portable workstream delta exactly once, and import only visible history without
writing the harness's private store. Contributors adding another harness should
follow this protocol.

## 1. Establish the native contract

Start with current upstream documentation and repeatable fixtures. The
companion `ai-babel` research project is useful prior work, but it is not a
substitute for verifying the installed CLI version.

Document and test:

- the default executable and supported host platforms;
- fresh-session and resume syntax, including selector placement;
- explicit selectors supplied by the user;
- print/noninteractive modes and utility subcommands that must pass through;
- session-store roots, environment overrides, and path options;
- the stored checkout identifier and timestamp units;
- complete versus in-progress records, tool-pairing rules, compaction records,
  and records containing hidden reasoning or credentials;
- a supported way to inject startup context before the first model turn; and
- the native dangerous/approval-bypass option, if the harness has one.

If exact session identity, read-only extraction, or pre-turn context delivery
cannot be demonstrated, document the limitation instead of adding a partial
managed adapter.

## 2. Register the harness deliberately

Add or reuse the harness's `AgentKind`. A new kind must also be added to
`AgentKind::ALL` and to a forward SQLite migration that rebuilds the sessions
constraint and pairing triggers without losing existing rows. Never edit an
already-released migration.

Then wire the explicit managed surface:

- `RunHarnessChoice` in `ai-memory-cli`;
- `ManagedHarness`, its executable, and its `AgentKind` mapping in
  `ai-memory-workstream`;
- the server's managed-harness validation list; and
- README, install, architecture, design-decision, and changelog references.

Explicit support comes first. Add a harness to bare `ai-memory run` automatic
selection only after checkout-local candidate discovery is reliable. A local
file timestamp is a bootstrap hint; the server's current linked harness remains
authoritative for an established workstream.

## 3. Preserve native argv and session ownership

Implement fresh, resume, and explicit-selector behavior in
`crates/ai-memory-workstream/src/harness.rs`. Preserve every user argument and
its order except the exact wrapper-owned `--yolo` token. An explicit native
selector always wins over ai-memory's linked session. Help, version, login,
doctor, export, and similar utility commands must not receive session flags.

Generate a session id only when the native CLI officially accepts a caller
provided id. Otherwise let the harness create the session, then discover it by
exact checkout and launch time. Do not infer a session from "newest globally."

Map `--yolo` only to a verified native option and avoid duplicates. If the
harness has no equivalent, add no flag and document that fact.

## 4. Discover and export read-only

Implement candidate discovery and incremental export in
`crates/ai-memory-workstream/src/transcript.rs`.

The adapter must:

- restrict candidates to the exact current checkout;
- honor documented store-root environment and command-line overrides;
- open SQLite stores read-only and never create, migrate, vacuum, or repair
  them;
- tolerate an incomplete final JSONL record or an in-progress tool call without
  advancing past it;
- emit deterministic source record and event ids;
- resume from a persisted source cursor without duplicates;
- normalize visible user/assistant messages, completed tool calls/results, and
  compaction summaries; and
- exclude system/developer prompts, hidden reasoning, binary payloads,
  credentials, provider metadata, and unsupported records.

Extraction gaps should become bounded loss annotations. They must not cause the
adapter to copy a private record "just in case."

## 5. Deliver context before acknowledging it

The preferred path is a native SessionStart hook. The managed child inherits
`AI_MEMORY_RUN_ID`; the hook links the actual native session, renders the unseen
bounded workstream range, makes it model-visible, and only then accepts the
delivery cursor.

If the harness has no suitable hook, use a documented native context mechanism.
Crush is the reference: the launcher fetches without accepting, writes a private
temporary copy of the supported config plus an ephemeral context file, starts
the child, and acknowledges only after spawn succeeds. The original config and
session store are never written by ai-memory, the harness retains its normal
native writes, and the temporary directory is removed after exit.

Fetching must be repeatable until acceptance. A failed spawn, hook, or network
ack may redeliver context; it must never silently lose it. Historical tool calls
must be labelled completed evidence so another harness cannot interpret them as
pending work.

## 6. Keep workstream invariants intact

One logical workstream has at most one current native session per harness. A
new harness joining an established workstream starts a clean native session and
receives portable history; it must not adopt an unrelated older local session.
First-use adoption is allowed only while authoritative server state has no
linked native session or substantive portable event.

Keep leases, delivery cursors, source cursors, immutable sanitized segments,
batch limits, and idempotent imports on the existing shared path. Do not add a
harness-specific synchronization database or mutate private stores to resolve
precedence, directory renames, or conflicts.

## 7. Required tests

A managed-harness PR should include focused coverage for:

- fresh launch, linked resume, explicit-selector precedence, argv order,
  utility passthrough, path overrides, and `--yolo` mapping;
- candidate ordering, exact-checkout isolation, timestamp handling, read-only
  access, incremental cursors, stable ids, incomplete records, visible record
  inclusion, and private record exclusion;
- first-use adoption and the established-workstream obsolete-session guard;
- startup context fetch/injection/accept ordering and spawn-failure redelivery;
- the `AgentKind::ALL` schema invariant when a kind is added;
- deterministic fake-process acceptance in
  `scripts/managed-workstream-acceptance.sh`; and
- a manual real-harness pass that switches into the new harness, observes the
  prior sentinel, persists its reply, and resumes its original native session
  when revisited.

The deterministic phase remains credential-free and suitable for frequent
local runs. Real model calls stay opt-in and outside CI. Record the tested CLI
version and any platform limitation in the PR description.

Run the repository's complete Rust gate before requesting review:

```bash
cargo fmt --check
git diff --check
TAILWIND_SKIP=1 cargo test --workspace
TAILWIND_SKIP=1 cargo clippy --workspace --all-targets -- -D warnings
```
