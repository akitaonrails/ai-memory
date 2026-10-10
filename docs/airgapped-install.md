# Air-gapped / offline install

Corporate IT-security review commonly asks for a documented offline
installation path before approving a dev tool. ai-memory's build and
runtime are already close to air-gap-friendly by design. This page collects
what the docs already say elsewhere into one answer; it does not add a new
mode.

## Build: no network required

The build is self-contained (`CONTRIBUTING.md`, "Dev setup"): SQLite is
bundled via `rusqlite`'s `bundled` feature and `libgit2` is vendored via
`git2`'s `vendored-libgit2` feature. No system libraries beyond a standard C
toolchain are required, and nothing is fetched from the network during
`cargo build`. Building from a vendored/mirrored crates.io cache (standard
practice for air-gapped Rust builds, e.g. `cargo vendor`) works the same way.

If you'd rather not build at all, tagged GitHub Releases publish prebuilt
binaries for Linux (`linux/amd64`, `linux/arm64`), macOS
(`ai-memory-macos-aarch64.tar.gz`, `ai-memory-macos-x86_64.tar.gz`), and
Windows (`ai-memory-windows-x86_64.zip`), each with a SHA-256 checksum
(`SECURITY.md`, "Published executable integrity"). Download once on a
network-connected machine, verify the checksum, and transfer the artifact
into the air-gapped environment.

## Runtime: no network calls unless you configure one

Covered in full in [`DATA_HANDLING.md`](../DATA_HANDLING.md), summarized
here: ai-memory has no telemetry, analytics, or phone-home behavior. The
server, CLI, and lifecycle hooks run entirely against the local data
directory. The only things that make a network call are things you
explicitly configure:

- **A cloud LLM/embedding provider**, if you want AI-assisted consolidation,
  search, or provider-based embeddings. Skip this and use the `local`
  embedding provider (below) to stay fully offline.
- **`capture_assistant`** and **`AI_MEMORY_RERANKER=llm`**, both off by
  default; see `DATA_HANDLING.md`.

## Local embeddings without a network dependency

For semantic search without a cloud API key or a self-hosted inference
server, set the embedding provider to `local`
([`docs/local-embeddings.md`](local-embeddings.md)). On first use it fetches
three small model files (~87 MB total, Apache-2.0 licensed) into
`<data_dir>/models/all-MiniLM-L6-v2/`, each checked against a sha256 pinned
in source so a tampered or drifted file fails with an error instead of
loading.

For a fully offline install, download those three files
(`model.safetensors`, `tokenizer.json`, `config.json`) from
`huggingface.co/sentence-transformers/all-MiniLM-L6-v2` on a
network-connected machine and place them in that directory before first
start; the loader verifies the same checksums and never touches the network
(`docs/local-embeddings.md`, "Offline installs"). After that, embeddings run
entirely on-host via the bundled `candle` (pure Rust) runtime, with no ONNX
runtime or other native library to source separately.

## What still needs a decision from you

- **git remote sync**, if you use it to push the wiki repository somewhere,
  is your own channel to secure (`SECURITY.md`, "Remote sync security";
  out of scope for ai-memory itself). [`docs/backup.md`](backup.md) has a
  worked example of the rsync + `git push` pattern with the appropriate
  secret and derived-state exclusions.
- **Update/patch delivery** in an air-gapped environment is manual: pull a
  new release and checksum on a connected machine, then transfer it in,
  the same as the initial install.

## Related documents

- [`DATA_HANDLING.md`](../DATA_HANDLING.md): what leaves the host and when.
- [`docs/local-embeddings.md`](local-embeddings.md): embedding provider
  choices, including the offline path in detail.
- [`docs/deploy.md`](deploy.md): Docker/homelab deployment pattern.
- [`SECURITY.md`](../SECURITY.md): release integrity and threat model.
