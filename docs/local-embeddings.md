# Local embeddings

*2.0 item 5.* `AI_MEMORY_EMBEDDING_PROVIDER=local` runs sentence
embeddings in-process, with no API key, external server, or GPU
required. It uses pure-Rust BERT inference (candle) and ships two
checksum-pinned 384-dim models:

- `all-MiniLM-L6-v2` (default, ~87 MB, Apache-2.0) — the English-focused
  sentence-transformers workhorse the comparable memory servers ship by
  default.
- `intfloat/multilingual-e5-small` (alias `multilingual-e5-small`, ~470 MB,
  MIT) — covers ~100 languages for non-English or mixed-language wikis.
  The loader applies E5's asymmetric `"query: "` and `"passage: "`
  prefixes automatically (no `embedding_query_prefix` /
  `embedding_document_prefix` setting needed).

**As of 2.0 this is the default**: an install with no
`embedding_provider` configured gets local embeddings (`all-MiniLM-L6-v2`)
automatically, on a best-effort basis. The model downloads in the
background on the first start (hybrid search enables on the next restart)
and existing pages are backfilled automatically. A host that cannot fetch
the model (or a slim build without the feature) keeps the FTS-only
behaviour with a warning. Opt out with `embedding_provider = "none"`; an
explicitly configured provider is never overridden.

With no `embedding_provider`, `embedding_model` chooses between the two
local models. Any other name, such as a hosted model left behind after the
provider line was removed, is ignored: the server logs a warning at startup
and runs the default model, as releases before the multilingual model did.
With `embedding_provider = "local"` set explicitly, an unknown name stops
the server instead.

```toml
# config.toml — all optional as of 2.0
embedding_provider = "local"                           # explicit: hard-fails if unavailable
# embedding_model  = "intfloat/multilingual-e5-small"  # or alias "multilingual-e5-small"
# embedding_provider = "none"                          # opt out of vectors entirely
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

The binary does not bundle the model weights. On the first start with
`local` configured, the server fetches three files for the selected model
into `<data_dir>/models/<dir>/`. Each is verified against a sha256 pinned
in the source, so a drifted or tampered upstream file fails loudly instead
of silently changing every vector.

### `all-MiniLM-L6-v2` (default, ~87 MB, Apache-2.0)

Directory: `<data_dir>/models/all-MiniLM-L6-v2/`

| file | sha256 (pinned 2026-09-01) |
|---|---|
| `model.safetensors` | `53aa5117…` |
| `tokenizer.json` | `be50c362…` |
| `config.json` | `953f9c0d…` |

Offline base URL:
`https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main/`

### `intfloat/multilingual-e5-small` (~470 MB, MIT)

Directory: `<data_dir>/models/multilingual-e5-small/`

| file | sha256 (pinned revision `614241f6`) |
|---|---|
| `model.safetensors` | `1a55775f…` |
| `tokenizer.json` | `0b44a9d7…` |
| `config.json` | `69137736…` |

Offline base URL:
`https://huggingface.co/intfloat/multilingual-e5-small/resolve/614241f622f53c4eeff9890bdc4f31cfecc418b3/`

**Offline installs**: download the three files for your chosen model from
its base URL above on any network-connected machine and drop them into its
directory under `<data_dir>/models/`; the loader verifies the same
checksums and never touches the network.

## Coexistence and migration

Nothing is forced. `(provider, model, dim)` is stored on every
embedding row, and hybrid search ignores vectors whose triple does not
match the configured embedder. Local vectors therefore sit beside any
provider vectors you already have — and switching between
`all-MiniLM-L6-v2` and `intfloat/multilingual-e5-small` triggers the same
mismatch guard, because the stored `model` identity differs. Run
`ai-memory embed --force` to re-embed a project immediately under the new
model (or wait for scheduled backfill).

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
- Local inference runs through a small process-wide scheduler sized from
  the CPU: half the available parallelism, clamped to between 2 and 6
  slots (`local_capacity` in `ai-memory-llm`). all-MiniLM inference
  already parallelizes across cores inside one embed, so this bounds
  concurrent calls while mild oversubscription keeps the CPU busy.
  Document embeddings may use all but one slot, so page writes and
  administrative backfill embed several documents at once instead of
  serializing, and one slot stays reserved for search queries. A query
  that arrives while every slot is busy waits up to 250 ms for one before
  falling back to lexical/entity/graph retrieval, so a query that merely
  races a document embed keeps its vector stream; only sustained
  saturation degrades. Hosted embedding providers are not subject to
  this local scheduler.
- The `local-embeddings` cargo feature (default on) carries the ML
  dependency tree; a slim build can disable it and keep every other
  provider.
- Tokenization truncates at 512 tokens, the model's positional limit.
  Longer page bodies contribute only their head, the same contract as the
  hosted embedders.

The opt-in contention probe compares the raw local embedder with that
capacity scheduler. It needs the downloaded model and is not part of the
routine test tier:

```bash
AI_MEMORY_TEST_MODELS_DIR=$PWD/evals/models \
AI_MEMORY_EMBED_BENCH_WORKERS=8 \
cargo test -p ai-memory-consolidate \
  local_query_latency_under_document_load -- --ignored --nocapture
```
