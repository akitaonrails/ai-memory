# Deploying ai-memory to a homelab

This walks through the pattern documented in `bin/deploy`. The end
state is: a long-lived ai-memory container on your homelab host,
reachable on your LAN at `http://<host>:49374/mcp`, configured with
your LLM/embedding API keys, with backups handled by whatever you
already use for `/var/opt/docker/...`.

If you want a native Linux service instead of Docker, use the Arch/AUR
systemd path in [`docs/install.md`](install.md#arch-linux-native-packages-aur).
That install mode uses `/var/lib/ai-memory` plus `/etc/ai-memory/` for the
system service, or XDG user paths for the user service. The Docker deployment
below remains `/data` inside the container and `/var/opt/docker/...` on the
host.

Published Docker images include `linux/amd64` and `linux/arm64` manifests, so
homelab hosts on x86_64 and ARM64 can pull the same tag natively.

## What gets committed vs. what stays local

The repo ships **templates only**. The real files with your homelab
specifics and API keys live next to them with the `.example` suffix
stripped, and are gitignored.

| Committed (template) | Live (gitignored) | What it holds |
|---|---|---|
| `bin/deploy` | (the script itself; safe to commit) | The build/push/restart logic |
| `bin/deploy.env.example` | `bin/deploy.env` | `SERVER`, `DEPLOY_DIR`, `IMAGE` |
| `docker/docker-compose.prod.yml.example` | `docker/docker-compose.prod.yml` | Image tag, port mapping, volume path |
| `docker/.env.production.example` | `docker/.env.production` | LLM + embedding API keys |

`.gitignore` excludes the live files. If you ever see one staged,
something has drifted - unstage before committing.

## First-time setup (one-time)

```bash
# 1. Stamp your homelab values into the local config.
cp bin/deploy.env.example bin/deploy.env
$EDITOR bin/deploy.env                # fill SERVER / DEPLOY_DIR / IMAGE

cp docker/docker-compose.prod.yml.example docker/docker-compose.prod.yml
$EDITOR docker/docker-compose.prod.yml   # set the image tag + adjust ports if needed

cp docker/.env.production.example docker/.env.production
$EDITOR docker/.env.production        # fill credentials; pick an LLM provider (model override optional)

# 2. Create the deploy dir on the homelab. Source bin/deploy.env so
#    SERVER/DEPLOY_DIR are exported in this shell.
source bin/deploy.env
ssh "$SERVER" "sudo mkdir -p $DEPLOY_DIR/data && \
               sudo chown -R 1000:1000 $DEPLOY_DIR"

# 3. Copy the compose + env to the homelab.
scp docker/docker-compose.prod.yml "$SERVER:$DEPLOY_DIR/docker-compose.yml"
scp docker/.env.production         "$SERVER:$DEPLOY_DIR/.env.production"

# 4. Run the first deploy.
bin/deploy
```

After step 4, the container should be running. Verify:

```bash
curl http://<homelab>:49374/mcp
# Expect a JSON-RPC error (which means the port is reachable and the
# server is responding). "Connection refused" means the container
# isn't up or the port mapping is wrong.

ssh "$SERVER" "docker inspect --format='{{.State.Health.Status}}' ai-memory"
# Expect: healthy
```

## Security - bearer-token auth + encrypted transport

The default `docker-compose.prod.yml.example` binds to `0.0.0.0:49374`
so the LAN can reach the MCP endpoint. **A LAN-bound server with no
auth lets anyone on the network call destructive MCP tools** (delete
all pages, inject fake observations, drain your LLM budget). ai-memory
ships a built-in bearer-token check; turn it on before the first deploy.

```bash
# 1. Generate a token (32 bytes / 64 hex chars).
ai-memory generate-auth-token >> docker/.env.production
$EDITOR docker/.env.production    # prefix the new line with AI_MEMORY_AUTH_TOKEN=

# 2. Sync to the homelab + restart.
scp docker/.env.production "$SERVER:$DEPLOY_DIR/.env.production"
ssh "$SERVER" "cd $DEPLOY_DIR && docker compose up -d"
```

The startup log will now show `auth=true`. Verify from the laptop:

```bash
curl -sI http://homelab:49374/handoff             # → HTTP/1.1 401 Unauthorized
curl -sI http://homelab:49374/handoff \
     -H "Authorization: Bearer $TOKEN"            # → HTTP/1.1 200 OK
```

**Then update every MCP client** to send the same token. `ai-memory
install-mcp --client <name> --auth-token <token>` prints the exact
snippet for every client in the [README Support Matrix](../README.md#support-matrix);
run `ai-memory install-mcp --help` for the current accepted values. The agent
CLI sends an `Authorization: Bearer <token>` header on every call; ai-memory's
middleware validates with a constant-time comparison.

**Encrypted transport.** Plain HTTP on the LAN means anyone with a
packet capture can read the bearer token (and per-user tokens once
multi-user mode is on) in transit. Add a TLS-terminating reverse
proxy in front of ai-memory — Caddy with Let's Encrypt, Caddy with
its internal CA, Cloudflare Tunnel, nginx, or external cert files —
when you bind beyond loopback or turn on multi-user.

**See [`docs/https-via-proxy.md`](https-via-proxy.md)** for the full
deployment guide, including:

- When to add TLS and when to skip it (the loopback + stdio cases honestly don't need it).
- Copy-paste docker compose templates in [`docker/compose.tls.caddy.yml`](../docker/compose.tls.caddy.yml) and [`docker/compose.tls.cloudflared.yml`](../docker/compose.tls.cloudflared.yml).
- Per-OS trust-store install for the internal-CA path (the load-bearing manual step).
- [Hosting under a subpath](https-via-proxy.md#hosting-under-a-subpath) via `--base-path` / `AI_MEMORY_BASE_PATH` when ai-memory shares a hostname with other apps.
- The explicit "what can go wrong" sections so you don't ship security theatre by accident.

For the single-user-on-loopback Quick Start, the bearer token alone
remains acceptable — the token is what stops the LAN neighbour, and
loopback is what stops the packet capture. TLS earns its keep once
the deployment shape stops being "single user, single machine."

## Routine deploys

After the first-time setup, every subsequent deploy is just:

```bash
bin/deploy
```

It builds the image locally, pushes to your registry, pulls on the
homelab, and restarts. The compose file + env file on the homelab are
unchanged between deploys; if you ever need to change them, scp the
new copy + re-run `bin/deploy`.

## Updating API keys

```bash
$EDITOR docker/.env.production
scp docker/.env.production "$SERVER:$DEPLOY_DIR/.env.production"
ssh "$SERVER" "cd $DEPLOY_DIR && docker compose up -d"
```

`docker compose up -d` reads the env file and recreates the container
with the new values. No rebuild needed.

## LLM provider choices

The `.env.production.example` defaults to **Kimi 2.6 via OpenRouter**
(openai-compat transport, $0.73/$3.49 per million tokens). Reasonable
alternatives:

| Provider | Model | Approx. cost / consolidation | Notes |
|---|---|---|---|
| anthropic | `claude-haiku-4-5` | ~$0.02 | **Recommended default.** Best balance of speed, restraint, and classification quality. Not a reasoning model. |
| openai-compat (OpenRouter) | `moonshotai/kimi-k2.6` | ~$0.013 | Reasoning model; latency ~2-3 min per consolidation. Fine because consolidation is fire-and-forget. |
| openai | `gpt-5.4-mini` | ~$0.002 | Cheaper, faster alternative. Decent quality. |
| openai-oauth | `gpt-5.5` | ChatGPT subscription | ChatGPT/Codex backend. Run `docker exec -it ai-memory ai-memory auth login openai-oauth` on the server host so `<data_dir>/auth.json` lands in the mounted data volume. |
| copilot | `gpt-5.5` | GitHub Copilot subscription | GitHub Copilot Chat backend. Run `docker exec -it ai-memory ai-memory auth login copilot` on the server host or set `COPILOT_GITHUB_TOKEN`. |
| gemini | `gemini-3.5-flash` | free tier covers personal use | Google hosted, native `responseSchema` structured output. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| openai-compat (Ollama) | `qwen3:32b` | $0 | Self-hosted. Set `AI_MEMORY_LLM_BASE_URL=http://host.docker.internal:11434/v1`. Quality depends on the model. |

> **What we don't recommend:** reasoning-mode models (Kimi-K2.6 in reasoning mode,
> Claude with extended thinking, GPT-o3, Gemini "thinking" variants) — they burn
> token budget on internal reasoning before emitting output and hang or emit empty
> responses with the strict-JSON consolidation prompt. If you must use one, turn
> reasoning off.

ai-memory's hosted OpenAI-family providers use `json_schema` strict mode for
structured output. The OpenAI provider normalizes schemars output into
OpenAI's supported subset (`additionalProperties: false`, complete `required`,
generated enum `anyOf`, and plain `$ref` nodes). `openai-compat` local and
gateway endpoints use the same schema-constrained request by default, with a
tolerant fallback for explicit capability rejection or malformed output. Set
`AI_MEMORY_LLM_COMPAT_STRICT=false` for an incompatible endpoint. If you switch
to a niche local model, run a quick `ai-memory llm-test` before trusting it.

Every chat provider bounds each HTTP request at 300 seconds. Slow hosted
gateways (observed with free aggregator tiers) can stream a long completion
past that ceiling and fail every request with `http: error sending request`;
raise `AI_MEMORY_LLM_TIMEOUT_SECS` in the container environment to match the
gateway's worst-case generation time.

## Backups

The data dir is whatever you mounted in `docker-compose.prod.yml`
(default: `/var/opt/docker/utils/ai-memory/data/`). It contains:

```
data/
├── wiki/    # markdown — back up with rsync or git push to a remote
├── raw/    # immutable session log archive
├── db/     # memory.sqlite (FTS5 + entities + page_embeddings)
├── logs/   # daily rolling tracing
└── models/ # reserved for future local embedders
```

For point-in-time consistency:

```bash
ssh "$SERVER" "docker exec ai-memory /usr/local/bin/ai-memory backup --to /data/snapshot-$(date +%F).tar.gz"
scp "$SERVER:$DEPLOY_DIR/data/snapshot-$(date +%F).tar.gz" ./backups/
```

The `ai-memory backup` command uses SQLite's online backup API so
writes during the snapshot are coherent.

## Reclaiming disk space

SQLite does not return deleted space to the filesystem. Deleted rows leave
their pages on a **freelist**, which SQLite reuses as the database grows again.
For a store in steady use that is the right behaviour and needs no attention.

`ai-memory status` reports the figure that decides it:

```
  storage:      2.4 GiB on disk, 610.0 MiB reclaimable (25.4%)
    `ai-memory compact --confirm` would return it (blocks writes while it runs)
```

The advisory line only appears once the backlog is worth acting on. When it
does:

```bash
ssh "$SERVER" "docker exec ai-memory /usr/local/bin/ai-memory compact --confirm"
```

Compaction deletes nothing — it rebuilds the FTS indexes and `VACUUM`s.

### Do not schedule an unconditional nightly VACUUM

The obvious move is a nightly cron entry. Resist it:

- `VACUUM` takes an **exclusive lock** and rewrites the entire database. Every
  write blocks for the duration, which on a large store is minutes — and this
  server's job is answering hook traffic, so blocking writes drops captures.
- It needs free disk space of roughly the database's own size, so the nightly
  job also sets a permanent floor on free space.
- Because SQLite reuses free pages, a store in steady use usually has almost
  nothing to reclaim. The nightly run pays the full cost for no benefit on most
  nights.

The case that genuinely leaves a large freelist is a **one-off deletion** — a
`purge-project`, a big retention sweep, a `forget-sweep` over months of
episodic pages. Those are events, not a schedule, and the destructive commands
already offer `--compact` inline for exactly that moment.

If you do want it automated, make it **conditional** on the figure above and
put it in off hours:

```bash
#!/usr/bin/env bash
# /usr/local/bin/ai-memory-compact-if-worthwhile
set -euo pipefail

RECLAIMABLE=$(ai-memory status --json | jq '.storage.reclaimable_bytes')
THRESHOLD=$((1024 * 1024 * 1024))   # 1 GiB — tune to your store

if [ "$RECLAIMABLE" -ge "$THRESHOLD" ]; then
    ai-memory compact --confirm
fi
```

Drive it from a systemd timer (`OnCalendar=Sun 04:00`, `Persistent=true`) or a
host cron entry. A weekly check that usually does nothing costs one cheap
`status` call; a nightly `VACUUM` that usually does nothing costs a write stall
every night.

### What compaction does not do

It is not erasure. The wiki git history keeps page content in its objects and
commit messages, and any backup taken earlier still holds everything. Compaction
returns bytes from the live SQLite file — worth doing on its own terms, and not
a guarantee that content is unrecoverable. See
[lifecycle-ops.md](lifecycle-ops.md) for the full boundary.

## Rolling back

```bash
ssh "$SERVER" "cd $DEPLOY_DIR && \
               docker tag $IMAGE $IMAGE-rollback && \
               docker pull $IMAGE@sha256:<old-digest>"
ssh "$SERVER" "cd $DEPLOY_DIR && docker compose up -d"
```

The simplest rollback is to bring back an older image by digest. We
don't ship a `bin/rollback` because the right way is to keep the
prior image tag handy before each deploy (Docker Hub keeps every
push by digest for free).

## Watching logs

```bash
ssh "$SERVER" "docker logs -f --tail 100 ai-memory"
```

Or browse the daily rolling logs on the host:

```bash
ssh "$SERVER" "ls -la $DEPLOY_DIR/data/logs/"
ssh "$SERVER" "tail -100 $DEPLOY_DIR/data/logs/ai-memory.log.$(date +%F)"
```

## Troubleshooting

- **`Connection refused`** on `curl http://<host>:49374/mcp`: the
  container isn't up, or the port mapping is bound to `127.0.0.1`
  instead of `0.0.0.0`. Check `docker ps` on the homelab.
- **`unhealthy`** status: the container is running but its embedded
  `ai-memory status` healthcheck is failing. Most likely the data
  dir's permissions don't match the container's user (uid 1000). Fix
  with `sudo chown -R 1000:1000 $DEPLOY_DIR/data` on the host.
- **Embedding mismatch after a model change**: startup logs a warning
  when stored `(provider, model, dim)` triples differ from config.
  Hybrid search ignores stale rows until they are re-embedded. Start
  the server normally, then run `ai-memory embed --force` to rebuild
  every project in the workspace, or add `--project <name>` to scope
  the rebuild. Scheduled embedding backfill can also fill missing
  rows when enabled.
- **Capture looks delayed, or observations are missing**: `ai-memory
  status` reports local hook-spool health — how many events are queued
  client-side, the age of the oldest one, and the total failed-delivery
  attempts:

  ```
    spool:
      pending:    2
      oldest:     15m 0s
      retries:    4
  ```

  A non-empty spool with an aging oldest entry means hooks are reaching
  the local queue but not the server; events are not lost, they drain
  once it is reachable. `pending: 0` means capture is keeping up.

  The spool is **client-side**, so this section reflects the machine you
  run the command on, not the server — it is the local data dir even when
  `AI_MEMORY_SERVER_URL` points at a homelab. It is also printed when the
  server cannot be reached at all (to stderr, so `--json` consumers still
  get a single object on stdout), which is precisely when a backlog is
  worth seeing. `--json` carries the same numbers under a `spool` object
  (`pending`, `oldest_age_ms`, `retries_total`).

- **Provider failures**: `ai-memory status` reports passive LLM and
  embedding health from the last real provider call. A fresh process
  reports `unknown` until the server actually uses that role; it does
  not probe providers or spend tokens for health reporting.
- **Container restart loop**: check
  `docker logs ai-memory` - the `ai-memory starting` line at the top
  reports the resolved config; a missing required env var (e.g.
  `LLM_API_KEY` with `openai-compat` selected but no model) will fail
  here with a clear error.
