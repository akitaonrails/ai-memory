# Local embeddings

*2.0 item 5.* `AI_MEMORY_EMBEDDING_PROVIDER=local` runs sentence
embeddings in-process, with no API key, external server, or GPU
required. It uses pure-Rust BERT inference (candle) with
`all-MiniLM-L6-v2` (384-dim), the sentence-transformers model the
comparable memory servers ship by default.

**As of 2.0 this is the default**: an install with no
`embedding_provider` configured gets local embeddings automatically, on a
best-effort basis. The model downloads in the background on the first start
(hybrid search enables on the next restart) and existing pages are
backfilled automatically. A host that cannot fetch the model (or a
slim build without the feature) keeps the FTS-only behaviour
with a warning. Opt out with `embedding_provider = "none"`; an
explicitly configured provider is never overridden.

```toml
# config.toml — all optional as of 2.0
embedding_provider = "local"     # explicit: hard-fails if unavailable
# embedding_provider = "none"    # opt out of vectors entirely
```

## Why: semantic recall without data egress

Search is hybrid: FTS5 (lexical), entities, graph, and, when an
embedder is configured, vectors. The vector stream lets
*"how do we deploy"* find the page that says *"release procedure"*
(paraphrase recall, with no shared keywords). Before this provider,
enabling it cost one of two things:

- **an API key** (OpenAI / Voyage / Google): per-call spend, and every
  page body and every search query leaves your machine, which is a lot
  to ask of a system that records everything you do;
- **a self-hosted engine** (Ollama / LM Studio via `openai-compat`):
  keyless, but another server to run, warm, and keep on the same
  network as ai-memory.

`local` removes both. Use it when any of these describe you:

- you run the zero-LLM path and want better recall without handing a
  provider your memory;
- the install is offline or air-gapped (drop the model files in
  manually, as described below);
- a homelab/team server where "one binary, one volume" is the whole
  operational story and adding an Ollama sidecar just for embeddings
  is not worth it;
- you want reproducible retrieval: same model files (checksum-pinned),
  same vectors, forever, with no provider-side model deprecations.

Stick with a hosted or self-hosted provider when you already run one
happily, want a larger/multilingual model, or want embedding compute
off the memory server's CPU.

### Why not ONNX?

ONNX is a model format plus a native runtime (`onnxruntime`). It is one
way to run local inference, and the one the comparable servers
use. We ship the same model through candle (pure Rust) instead. The
capability is identical, and there is no native C++ library to build,
license, and debug across every release target (Windows, macOS, the
sandboxed nix build). If a future model requires an ONNX-only runtime, the
`Embedder` trait is where it would slot in.

## The model files

The binary does not bundle the model (~87 MB). On the first start with
`local` configured, the server fetches three files into
`<data_dir>/models/all-MiniLM-L6-v2/`. Each is verified against a sha256
pinned in the source, so a drifted or tampered upstream file fails
loudly instead of silently changing every vector:

| file | sha256 (pinned 2026-09-01) |
|---|---|
| `model.safetensors` | `53aa5117…` |
| `tokenizer.json` | `be50c362…` |
| `config.json` | `953f9c0d…` |

**Offline installs**: download the three files from
`https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/`
on any machine and drop them into the directory above; the loader
verifies the same checksums and never touches the network. The model
is Apache-2.0.

## Coexistence and migration

Nothing is forced. `(provider, model, dim)` is stored on every
embedding row, and hybrid search ignores vectors whose triple does not
match the configured embedder. Local vectors therefore sit beside any
provider vectors you already have, and switching back is a config
change. `ai-memory embed --force` re-embeds a project under the
current provider when you want one consistent set.

Existing installs keep their configured provider; `local` is opt-in.
Fresh-install defaults are decided by benchmark numbers.
`docs/benchmarks/` has the zero-LLM vs local-embeddings
LongMemEval rows, reproducible via:

```bash
cargo run --release -p ai-memory-eval -- retrieval --embeddings local
```

## Operational notes

- Inference is CPU, off the async runtime (`spawn_blocking`);
  first-load reads ~87 MB into memory once per server process.
- The `local-embeddings` cargo feature (default on) carries the ML
  dependency tree; a slim build can disable it and keep every other
  provider.
- Tokenization truncates at 512 tokens, the model's positional limit.
  Longer page bodies contribute only their head, the same contract as the
  hosted embedders.
