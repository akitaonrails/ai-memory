#!/usr/bin/env python3
"""OpenAI-compat -> Jev `/v1/systemone` reranker adapter for ai-memory.

ai-memory's LLM reranker (`AI_MEMORY_RERANKER=llm`) sends one structured chat
request per `memory_query`: a fixed system prompt plus a user JSON payload
`{"query", "candidates":[{"candidate","title","text"}]}`, and expects the
first balanced JSON object in the reply content to be
`{"scores":[{"candidate","relevance"}]}` (1-based indices, relevance in
[0,1]; a timeout, error, or invalid score set preserves the server's own
order).

This adapter recognises exactly that request (by the system-prompt prefix),
asks Jev one batched `choice` question over all candidates, and returns each
candidate's choice probability as its relevance, as plain chat content. A
malformed Jev answer (missing or extra keys, non-numbers, values outside
[0, 1], a distribution that does not sum to ~1) is an error, so ai-memory
keeps its own order instead of ranking on fabricated scores. Everything else
(consolidation, lint, bootstrap) is reverse-proxied unchanged to the
configured upstream, so only reranking rides the Jev endpoint.

Why: a hosted reranker model that answers in tens of seconds dwarfs the rest
of a `memory_query`, while a judge endpoint answers in well under a second,
which keeps `AI_MEMORY_RERANKER=llm` usable in interactive sessions.
docs/jev-reranker-adapter.md has the measurements, including how this choice
variant compares with the earlier per-candidate rubric adapter.

Env:
  JEV_URL     Jev systemone endpoint      (default http://127.0.0.1:18095/v1/systemone)
  JEV_MODEL   model name sent to Jev      (default jev-latest)
  UPSTREAM    OpenAI-compat upstream base (default http://127.0.0.1:8000)
  LISTEN      host:port to bind           (default 127.0.0.1:18097)

Stdlib only. No secrets are stored: Authorization headers are forwarded
verbatim to the upstream.
"""
import json
import math
import os
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

JEV_URL = os.environ.get("JEV_URL", "http://127.0.0.1:18095/v1/systemone").rstrip("/")
JEV_MODEL = os.environ.get("JEV_MODEL", "jev-latest")
UPSTREAM = os.environ.get("UPSTREAM", "http://127.0.0.1:8000").rstrip("/")
LISTEN = os.environ.get("LISTEN", "127.0.0.1:18097")

# Fixed prefix of the reranker's system prompt; see
# crates/ai-memory-llm/src/reranker.rs in the ai-memory tree.
RERANK_SYSTEM_PREFIX = "You are a retrieval reranker for a software project's memory wiki."

# Mirrors the four grades of the reranker prompt (1.0 / 0.7 / 0.3 / 0.0).
RUBRIC = [
    "0.0 unrelated — does not address the query at all",
    "0.3 tangential — loosely related to the query",
    "0.7 same topic — same subsystem or topic, useful supporting context",
    "1.0 direct answer — directly answers the query",
]


def log(msg):
    sys.stderr.write(f"[jev-rerank] {time.strftime('%H:%M:%S')} {msg}\n")
    sys.stderr.flush()


def _reject_dupes(pairs):
    """object_pairs_hook refusing duplicate keys in the Jev response JSON."""
    d = {}
    for k, v in pairs:
        if k in d:
            raise ValueError(f"duplicate key {k!r} in Jev response")
        d[k] = v
    return d


def extract_choice_probs(out, cand_ids):
    """Return one validated choice probability per candidate, in candidate order.

    Raises ValueError on any malformed shape — missing/renamed keys, extra
    candidates, non-numeric or boolean values, NaN/Inf, out-of-range numbers,
    or a distribution whose mass is nowhere near 1 — so the caller answers
    HTTP 500 and ai-memory keeps its original order, instead of silently
    ranking on fabricated 0.0 scores.
    """
    if not cand_ids:
        raise ValueError("no candidates")
    if len(set(cand_ids)) != len(cand_ids):
        raise ValueError("duplicate candidate indices in the rerank request")
    if not isinstance(out, dict):
        raise ValueError("Jev response is not a JSON object")
    answers = out.get("answers")
    if not isinstance(answers, dict):
        raise ValueError("Jev response has no answers object")
    best = answers.get("best")
    if not isinstance(best, dict):
        raise ValueError("Jev response has no answers.best object")
    probs = best.get("probabilities")
    if not isinstance(probs, dict):
        raise ValueError("Jev response has no answers.best.probabilities object")
    expected = {f"c{n}" for n in cand_ids}
    missing = sorted(expected - probs.keys())
    extra = sorted(probs.keys() - expected)
    if missing or extra:
        raise ValueError(
            f"probability keys mismatch: missing={missing} extra={extra}")
    vals = []
    for n in cand_ids:
        p = probs[f"c{n}"]
        if isinstance(p, bool) or not isinstance(p, (int, float)):
            raise ValueError(f"probability c{n} is not a number: {p!r}")
        p = float(p)
        if not math.isfinite(p) or p < 0.0 or p > 1.0:
            raise ValueError(f"probability c{n} outside [0, 1]: {p!r}")
        vals.append(p)
    total = math.fsum(vals)
    # The choice distribution is normalised over the candidate set; allow
    # generous rounding drift but reject degenerate shapes (all ~0 / all ~1).
    if not 0.5 <= total <= 1.5:
        raise ValueError(f"choice probabilities sum to {total:.4f}, not ~1.0")
    return vals


def jev_rerank(payload):
    """Translate a reranker chat request into ONE batched Jev choice call.

    Choice-contrastive variant of the rubric adapter: a single `choice`
    question over all candidates, ranked by probability. On the same
    golden set and backend the rubric adapter answers from, comparative
    ranking beat per-candidate rubric scoring by double-digit hit@1 on
    both backends measured (see the comparison table in the docs), and it
    is the only shape in which replay-trained small models hold their
    quality. Relevance is the raw choice probability: monotonic in the
    probability order, which is all the reranker sorts on.
    """
    user = next((m.get("content", "") for m in payload.get("messages", [])
                 if m.get("role") == "user"), "")
    data = json.loads(user)
    query, cands = data["query"], data["candidates"]
    ids = [c["candidate"] for c in cands]
    lines = [f"User query: {query}", "", "Candidate documents:"]
    criteria = {}
    for c in cands:
        n = c["candidate"]
        desc = (c.get("title") or f"candidate {n}")[:120]
        lines.append(f"{n}. {desc} — {c['text'][:160]}")
        criteria[f"c{n}"] = desc
    # Pool v3 on the 35B backend: this wording beat "most relevant" 0.828 -> 0.879
    # hit@1 (5 fixed, 0 broken). Relevance stays the raw choice probability.
    questions = {"best": {"type": "choice",
                          "instructions": (
                              "Which candidate directly answers the user query? "
                              "Choose the specific page that contains the answer. "
                              "Do not choose an index, catalog, or summary page if a more specific page answers the query."
                          ),
                          "criteria": criteria}}
    body = {"model": JEV_MODEL, "state": "\n".join(lines), "questions": questions}
    t0 = time.perf_counter()
    req = urllib.request.Request(JEV_URL, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=25) as resp:
        out = json.loads(resp.read(), object_pairs_hook=_reject_dupes)
    dt = time.perf_counter() - t0
    vals = extract_choice_probs(out, ids)
    scores = [{"candidate": n, "relevance": round(p, 4)} for n, p in zip(ids, vals)]
    log(f"jev-choice {len(cands)} candidates in {dt:.3f}s query={query[:60]!r}")
    return {"scores": scores}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def _reply(self, status, body=b"", content_type="application/json"):
        self.send_response(status)
        if content_type:
            self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _proxy(self, raw):
        """Reverse-proxy this request unchanged to the upstream."""
        req = urllib.request.Request(UPSTREAM + self.path, data=raw, method=self.command)
        for k, v in self.headers.items():
            if k.lower() in ("host", "content-length", "connection", "accept-encoding"):
                continue
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=900) as resp:
                body = resp.read()
                self.send_response(resp.status)
                for k, v in resp.headers.items():
                    if k.lower() in ("transfer-encoding", "connection", "content-length"):
                        continue
                    self.send_header(k, v)
                self._reply_finish(body)
        except urllib.error.HTTPError as e:
            self._reply(e.code, e.read())
        except Exception as e:  # transport failure upstream
            log(f"forward error {self.command} {self.path}: {e}")
            self._reply(502, json.dumps({"error": str(e)[:200]}).encode())

    def _reply_finish(self, body):
        # headers besides Content-Length were already sent by the caller
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self._proxy(raw)
            return
        try:
            payload = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            self._reply(400)
            return
        system = next((m.get("content", "") for m in payload.get("messages", [])
                       if m.get("role") == "system"), "")
        if not system.startswith(RERANK_SYSTEM_PREFIX):
            self._proxy(raw)  # consolidation / lint / bootstrap traffic
            return
        try:
            result = jev_rerank(payload)
            out = {
                "id": "jev-rerank",
                "object": "chat.completion",
                "model": payload.get("model", JEV_MODEL),
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": json.dumps(result)},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            }
            self._reply(200, json.dumps(out).encode())
        except Exception as e:
            # The server treats any failure as "keep the original order".
            log(f"jev rerank failed: {e}")
            self._reply(500)

    def do_GET(self):
        self._proxy(None)


def main():
    host, port = LISTEN.rsplit(":", 1)
    srv = ThreadingHTTPServer((host, int(port)), Handler)
    log(f"listening {LISTEN} jev={JEV_URL} upstream={UPSTREAM}")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
