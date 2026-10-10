# The experience pass (cross-session abstraction)

*2.0 item 6.* Per-session auto-improvement reviews one trajectory at a
time, so it cannot see that four sessions repeated the same workflow,
that the operator keeps re-stating a preference no page records, or
that recent sessions contradict a stored decision. The experience pass
reads the last N session summaries of a project **side by side** and
proposes that cross-trajectory knowledge.

## Why "experience"

It is the Reflection→Experience step that the 2026 agent-memory literature
settles on (the survey's third stage; TriMem's narrative layer): raw
capture → per-session summaries → durable knowledge distilled across
trajectories. The pass targets pattern/procedure/preference/
architecture pages, which make session #50 cheaper than session #5.

## When to turn it on — and when not to

Opt-in, off by default:

```toml
[auto_improve.scheduler]
experience_every_sessions = 5   # run after every 5 newly completed sessions
experience_sessions = 10        # read the last 10 session summaries
```

Turn it on when per-session auto-improve is already working for you and
the project has real session history (the pass skips scopes with fewer
session pages than the cadence floor). Leave it off when the project is
young (cross-session patterns need enough sessions to compare) or when no
LLM is configured: the pass is LLM-hosted and the zero-LLM default path
never runs it.

## What a proposal looks like

After eight sessions in which the operator repeatedly rebuilt the
release binary before running the eval harness, the pass might stage:

```markdown
# _pending/auto-improve/…-procedures-eval-run.md   (confidence 0.86)

## Proposed: procedures/eval-run.md

Before benchmarking, rebuild the release server first — the harness
spawns `target/release/ai-memory`, and a stale binary silently
benchmarks old code.

    cargo build --release -p ai-memory-cli
    cargo run --release -p ai-memory-eval -- retrieval

Evidence: sessions 2026-08-28 ("rebuilt release, numbers changed"),
2026-08-30 ("forgot the rebuild again — rerun").
```

It stays in `_pending/` (with `require_approval = true`) until a human
approves it, the same review flow as every other auto-improve
proposal.

## What it costs and what guards it

One LLM call per project per cadence trigger, prompt-bounded like the
per-session reviewer. Every proposal flows through the **identical**
machinery: JSON-schema constrained output, validation, the confidence
floor, the eval gate, the rejection buffer (rejected ideas are shown to
later runs so they are not re-proposed), and pending-writes staging with
sidecars, so every proposal is reviewable and none is applied silently. `require_approval` applies
unchanged. The system prompt demands evidence spanning **at least two
sessions**, naming them; single-session findings are the per-session
reviewer's job and are rejected here.

The cadence anchors on enablement: switching the pass on against an old
store does not re-digest history; it waits for the next
`experience_every_sessions` completed sessions.
