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
$EDITOR docker/.env.production        # fill API keys; pick LLM provider + model

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
snippet per client (Claude Code, Codex, OpenCode, Cursor, Claude
Desktop, Gemini CLI, OpenClaw, OMP/Pi, Antigravity CLI). The agent CLI sends an
`Authorization: Bearer <token>` header on every call; ai-memory's
middleware validates with a constant-time comparison.

**Encrypted transport.** Plain HTTP on the LAN means anyone with a
packet capture on the network can read the bearer token and the wiki
content in transit. Two pragmatic ways to add TLS:

| Option | When it fits |
|---|---|
| **Front with cloudflared** (your existing homelab tunnel) | You want to access ai-memory from outside the LAN too. cloudflared terminates TLS at Cloudflare's edge and tunnels to the container over HTTP. The bearer token still applies; Cloudflare provides the encryption + DDoS shield. Configure a new `--hostname ai-memory.your-domain.tld --url http://localhost:49374` route on the existing cloudflared instance. |
| **Caddy reverse proxy** in a sibling container | You want LAN-only TLS without public DNS. Caddy auto-issues a self-signed cert with a tofu prompt the first time each client connects. Mount your homelab's CA cert into the container running each MCP client. |

For most homelab use cases the bearer token alone over plain HTTP on
a trusted LAN is acceptable. The token is what stops the LAN
neighbour; TLS is what stops a packet-capture-with-a-laptop-on-the-WiFi.

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
| gemini | `gemini-2.5-flash` | free tier covers personal use | Google hosted, native `responseSchema` structured output. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| openai-compat (Ollama) | `qwen3:32b` | $0 | Self-hosted. Set `AI_MEMORY_LLM_BASE_URL=http://host.docker.internal:11434/v1`. Quality depends on the model. |

> **What we don't recommend:** reasoning-mode models (Kimi-K2.6 in reasoning mode,
> Claude with extended thinking, GPT-o3, Gemini "thinking" variants) — they burn
> token budget on internal reasoning before emitting output and hang or emit empty
> responses with the strict-JSON consolidation prompt. If you must use one, turn
> reasoning off.

ai-memory's consolidator uses OpenAI's `json_schema` strict mode for
structured output. The OpenAI provider normalizes schemars output into
OpenAI's supported subset (`additionalProperties: false`, complete
`required`, generated enum `anyOf`, and plain `$ref` nodes). Most modern
models honour this, but if you switch to a niche local model, run a quick
`ai-memory llm-test` (with a structured prompt) before trusting it.

## Backups

The data dir is whatever you mounted in `docker-compose.prod.yml`
(default: `/var/opt/docker/utils/ai-memory/data/`). It contains:

```
data/
├── wiki/    # markdown — back up with rsync or git push to a remote
├── raw/    # immutable session log archive
├── db/     # memory.sqlite (FTS5 + page_embeddings)
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
- **Container restart loop**: check
  `docker logs ai-memory` - the `ai-memory starting` line at the top
  reports the resolved config; a missing required env var (e.g.
  `LLM_API_KEY` with `openai-compat` selected but no model) will fail
  here with a clear error.
