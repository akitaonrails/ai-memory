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

- Codex unit/recovery suite: 8 passed.
- Existing openai-oauth regression suite: 18 passed.
- Codex-related CLI/config tests: 17 passed.

The recovery suite compiles a fake Codex executable from Rust source. It runs
without network access or a real home directory and covers handshake messages,
interleaved notifications, wrong IDs, invalid and oversized output, stderr
limits, premature process exit, timeout, process cleanup, and auth-file
rotation. HTTP mocks cover account headers, model, reasoning, SSE, structured
output, reload-before-retry, and the one-retry ceiling.

Full workspace gates and live text/structured smoke results are appended only
after they run successfully.
