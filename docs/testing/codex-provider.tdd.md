# Codex provider TDD evidence

Date: 2026-09-11

This development branch intentionally has no issue/PR reference and no
`CHANGELOG.md` entry yet. It is a local validation branch, not merge-ready.

## RED

Commit: `3b215cb8 test: add codex provider configuration reproducers`

Command:

```text
cargo test -p ai-memory-llm codex_auth_round_trips_only_resolved_paths --no-default-features
```

Expected failure observed: Rust reported that `ProviderAuth::codex`,
`AuthRequirement::CodexAuthFile`, and `ProviderChoice::Codex` did not exist.

## GREEN

Focused commands after implementation:

```text
cargo test -p ai-memory-llm codex::tests --no-default-features
cargo test -p ai-memory-llm openai_oauth::tests --no-default-features
cargo test -p ai-memory-cli codex --no-default-features
```

Observed results:

- Codex unit/recovery suite: 10 passed.
- Existing openai-oauth regression suite: 18 passed.
- Codex-related CLI/config tests: 17 passed.

The recovery suite compiles a fake Codex executable from Rust source. It runs
without network access or a real home directory and covers handshake messages,
interleaved notifications, wrong IDs, invalid and oversized output, stderr
limits, premature process exit, timeout, process cleanup, and auth-file
rotation. HTTP mocks cover account headers, model, reasoning, SSE, structured
output, reload-before-retry, and the one-retry ceiling.

## Local gates and smoke

- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
- `TAILWIND_SKIP=1 cargo clippy --workspace --all-targets -- -D warnings`:
  passed.
- `cargo deny check`: passed (`advisories`, `bans`, `licenses`, `sources`).
- `TAILWIND_SKIP=1 cargo test --workspace`: provider and downstream suites
  passed until two unrelated `ai-memory-core::routing_skills` assertions hit
  CRLF bytes in the existing Windows checkout. Neither the embedded skill
  assets nor their tests are changed by this branch.
- The complementary run excluding `ai-memory-core`, followed by all other
  `ai-memory-core` tests with only those two checkout-sensitive assertions
  skipped, passed. The standalone companion importer suite also passed (19
  tests).
- `cargo llvm-cov` was unavailable locally, so the 80% target was not measured;
  the new parser, HTTP/retry path, structured output, protocol limits, native
  process recovery, failure cleanup, and concurrent recovery are directly
  exercised.
- Live text smoke (`gpt-5.6-luna`, effort `medium`): returned
  `AI_MEMORY_CODEX_TEXT_OK`.
- Live structured smoke (`gpt-5.6-luna`, effort `medium`): returned
  `{"answer":"AI_MEMORY_CODEX_STRUCTURED_OK"}`.
- The Codex auth-file SHA-256 was identical before and after both smokes, and
  `codex login status` remained authenticated. The digest itself is omitted
  from this repository artifact.
