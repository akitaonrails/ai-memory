# Jev Reranker Adapter

> Route ai-memory's LLM reranker to a Jev judge endpoint while keeping the
> hosted LLM for everything else. Stdlib-only Python, no Rust changes.

[`AI_MEMORY_RERANKER=llm`](llm-providers.md) reorders `memory_query`
candidates with the configured chat provider. That works until the provider
is a hosted reasoning model: the reranker prompt is long, a graded judgement
over up to 30 candidates, and a slow model answers in tens of seconds —
longer than one `memory_query` should ever take, and long enough to trip the
server's completion timeout, which turns the reranker into a dead feature
that stalls every query before falling back to the original order.

The reranker prompt, though, is already a scoring rubric: grade each
candidate 1.0 (direct answer) / 0.7 (same topic) / 0.3 (tangential) /
0.0 (unrelated). A judge endpoint that scores a fixed rubric answers the
same question without autoregressive decoding. The adapter in
[`docs/examples/jev-reranker-adapter/jev_rerank_shim.py`](examples/jev-reranker-adapter/jev_rerank_shim.py)
sits between ai-memory and the provider, translates exactly that request
into one batched Jev `score` call, and proxies everything else unchanged.

## How it works

ai-memory 2.4.0 has a single chat-provider configuration, so the reranker
and consolidation share one base URL. The adapter does not try to split
configuration; it splits traffic by request shape:

1. The reranker's system prompt starts with the fixed sentence
   `You are a retrieval reranker for a software project's memory wiki.`
   (see `crates/ai-memory-llm/src/reranker.rs`). Requests whose system
   message has that prefix are reranker requests.
2. The user message is `{"query", "candidates": [{"candidate", "title",
   "text"}]}` with 1-based indices. The adapter renders one `state` block
   (query + numbered candidates) and one `score` question per candidate
   whose `criteria` mirror the reranker prompt's four grades, then makes a
   single POST to the Jev `/v1/systemone` endpoint.
3. Each answer's rubric index maps back to `relevance` 0.0 / 0.3 / 0.7 /
   1.0 (index / 3), and the adapter returns
   `{"scores": [{"candidate": n, "relevance": f}]}` as plain chat-completion
   content — the exact shape the reranker's tolerant parser expects.
4. Every other request — consolidation, lint, bootstrap, plain chats — is
   reverse-proxied to the real upstream byte-for-byte, `Authorization`
   forwarded verbatim. The adapter stores no secrets.

Failure semantics match the server contract: if the Jev call fails, the
adapter answers HTTP 500 and ai-memory keeps its own candidate order, the
same as any provider outage. A judge endpoint that is down degrades to
"no reranking", never to "no search".

One cosmetic note: the server logs still show the provider's configured
model name for reranking (it comes from provider config, not from the
adapter's reply), so the reranker leg will be attributed to your hosted
model in `docker logs` even while the adapter serves it.

## Deploy

The adapter is stdlib-only Python 3. Configure with environment variables
(all defaults are loopback examples):

| Env | Meaning | Default |
|---|---|---|
| `JEV_URL` | Jev `/v1/systemone` endpoint | `http://127.0.0.1:18095/v1/systemone` |
| `JEV_MODEL` | model name sent to Jev | `jev-latest` |
| `UPSTREAM` | real OpenAI-compat base URL | `http://127.0.0.1:8000` |
| `LISTEN` | adapter bind address | `127.0.0.1:18097` |

Run it next to the server (systemd unit adapted to your paths):

```ini
[Unit]
Description=ai-memory Jev reranker adapter
After=network-online.target

[Service]
ExecStart=/usr/bin/python3 /opt/ai-memory/ops/jev_rerank_shim.py
Environment=JEV_URL=http://127.0.0.1:18095/v1/systemone
Environment=UPSTREAM=http://127.0.0.1:8000
Environment=LISTEN=127.0.0.1:18097
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

Then point the provider at the adapter instead of the upstream:

```console
AI_MEMORY_LLM_PROVIDER=openai-compat
AI_MEMORY_LLM_BASE_URL=http://127.0.0.1:18097/v1
AI_MEMORY_RERANKER=llm
```

Verify the split with `journalctl -u <unit>`: reranker requests log
`jev N candidates in X.XXXs`, everything else is silent (proxied).

## Benchmarks

Offline A/B, same candidate pool per query, 102 queries / 99 scored against
a hand-curated golden set for a real production wiki (FTS5 + vector + graph
hybrid, `explain=true` score details, identical server build):

| Reranker | hit@1 | MRR | NDCG@10 | mean rerank latency |
|---|---|---|---|---|
| none (server order) | 0.495 | 0.651 | 0.739 | — |
| hosted model A (reasoning, max) | 0.778 | 0.840 | 0.875 | **20.2 s** |
| hosted model B (reasoning) | 0.768 | 0.834 | 0.870 | 13.9 s |
| **Jev via this adapter** | **0.778** | **0.838** | **0.873** | **0.205 s** |

The judge endpoint matches the hosted reasoning models' quality at
~100× lower latency. Hosted model A's 20.2 s mean sat exactly on the
server's 20 s completion timeout, so in production every rerank call timed
out and the feature was effectively off (queries stalled 20 s, then fell
back to the original order).

Live end-to-end (production server, `AI_MEMORY_RERANKER=llm` through the
adapter, full golden set): hit@1 0.657 / MRR 0.788 / NDCG@10 0.843,
0 errors in 102 queries, mean `memory_query` latency 2.2 s (p50 2.21 s,
max 3.07 s), 110/110 rerank translations served, 0 adapter failures. The
gap to the offline arm is candidate-pool shape, not scoring: the live
server over-fetches 15–30 candidates with its own bounded snippets (the
offline arm rescored a fixed top-10 pool), and in 32 of 34 non-top-1
queries the expected page was still returned — mostly at rank 2 — with
hit@5 at 0.949. Versus the no-reranker baseline that is +16.2 points
hit@1 and +10.4 points NDCG@10 end to end.

Judge latency scales with the candidate count in the batch (the rubric
question is asked once per candidate): ~0.2 s for 10 candidates, ~1.6 s
mean for the 15–30-candidate batches the live server sends, still an order
of magnitude under any hosted reasoning model.

## Caveats

- The adapter keys on the exact system-prompt prefix above. If the
  reranker prompt wording changes in a future release, detection (not
  scoring) breaks first: reranker requests would be proxied to the hosted
  model, which is the pre-adapter behavior, not an outage.
- Scoring uses Jev's calibrated rubric grades. Do not substitute
  per-candidate choice probabilities: those are normalized across options
  (winner ≈ 0.99, rest ≈ 0.001) and do not fit the reranker's 0–1
  relevance semantics.
- Run the adapter on loopback or a trusted private network. It forwards
  `Authorization` headers verbatim and adds no authentication of its own.
