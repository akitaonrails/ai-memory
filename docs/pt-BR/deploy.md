# Implantando o ai-memory em um homelab

Este guia segue o padrão documentado em `bin/deploy`. Você termina
com um contêiner ai-memory de longa duração no host do seu homelab,
acessível na sua LAN em `http://<host>:49374/mcp`, configurado com
suas chaves de API de LLM/embedding, com backups gerenciados por aquilo que
você já usa para `/var/opt/docker/...`.

Se você preferir um serviço Linux nativo em vez do Docker, use o caminho
systemd do Arch/AUR em [`docs/install.md`](install.md#pacotes-nativos-do-arch-linux-aur).
Esse modo de instalação usa `/var/lib/ai-memory` mais `/etc/ai-memory/` para o
serviço de sistema, ou caminhos de usuário XDG para o serviço de usuário. A implantação Docker
abaixo permanece em `/data` dentro do contêiner e `/var/opt/docker/...` no
host.

As imagens Docker publicadas incluem manifestos `linux/amd64` e `linux/arm64`, permitindo que
hosts de homelab em x86_64 e ARM64 baixem a mesma tag nativamente.

## O que é commitado vs. o que permanece local

O repositório distribui **apenas modelos**. Os arquivos reais com as especificidades
do seu homelab e chaves de API ficam ao lado deles com o sufixo `.example`
removido e são ignorados pelo git.

| Versionado (modelo) | Ativo (gitignored) | O que contém |
|---|---|---|
| `bin/deploy` | (o próprio script; seguro para commit) | A lógica de build/push/restart |
| `bin/deploy.env.example` | `bin/deploy.env` | `SERVER`, `DEPLOY_DIR`, `IMAGE` |
| `docker/docker-compose.prod.yml.example` | `docker/docker-compose.prod.yml` | Tag da imagem, mapeamento de portas, caminho do volume |
| `docker/.env.production.example` | `docker/.env.production` | Chaves de API de LLM + embedding |

O `.gitignore` exclui os arquivos em produção. Se você algum dia vir um preparado (staged),
algo se desvirtuou; remova-o do stage antes de commitar.

## Configuração inicial (feita uma vez)

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

Após a etapa 4, o contêiner deve estar em execução. Verifique:

```bash
curl http://<homelab>:49374/mcp
# Expect a JSON-RPC error (which means the port is reachable and the
# server is responding). "Connection refused" means the container
# isn't up or the port mapping is wrong.

ssh "$SERVER" "docker inspect --format='{{.State.Health.Status}}' ai-memory"
# Expect: healthy
```

## TrueNAS SCALE e outros hosts NAS

Não existe um aplicativo no catálogo do TrueNAS, mas a imagem publicada roda como um app personalizado
em qualquer NAS que execute contêineres Docker. TrueNAS SCALE 24.10 e versões posteriores podem
instalar um a partir de um arquivo Compose. O que a imagem espera:

- a imagem `docker.io/akitaonrails/ai-memory:<version>` (fixe uma versão em vez de
  `latest`, para que uma atualização seja uma troca deliberada de tag);
- um dataset persistente montado em `/data`, pertencente ao uid e gid **1000**, o
  usuário `ai-memory` da imagem. Apps TrueNAS usam por padrão o uid 568, portanto passe a posse
  do dataset para 1000 ou execute o contêiner como o dono do dataset;
- porta `49374` publicada para a LAN;
- um bearer token e uma lista de hosts permitidos, pois o servidor fica acessível
  a partir da rede (consulte a próxima seção).

```yaml
services:
  ai-memory:
    image: docker.io/akitaonrails/ai-memory:<version>
    restart: unless-stopped
    ports:
      - "49374:49374"
    volumes:
      - /mnt/<pool>/apps/ai-memory:/data
    environment:
      AI_MEMORY_AUTH_TOKEN: "<64 hex chars from `ai-memory generate-auth-token`>"
      # Every name or address clients use to reach the NAS.
      AI_MEMORY_ALLOWED_HOSTS: "truenas.local,192.0.2.10,localhost,127.0.0.1"
```

O healthcheck da imagem executa `ai-memory status`, de modo que o app é exibido como saudável
assim que o servidor responder. Aponte os clientes para `http://<nas>:49374` com o mesmo
token, conforme descrito abaixo para qualquer servidor na LAN. Backups do dataset seguem
[Backups](#backups): tire snapshots com `ai-memory backup`, e não uma cópia de um arquivo
SQLite em uso.

## Segurança - autenticação por bearer token + transporte criptografado

O `docker-compose.prod.yml.example` padrão faz bind em `0.0.0.0:49374`
para que a LAN possa alcançar o endpoint MCP. **Um servidor vinculado à LAN sem
autenticação permite que qualquer pessoa na rede chame ferramentas MCP destrutivas** (excluir
todas as páginas, injetar observações falsas, esgotar seu orçamento de LLM). O ai-memory
inclui uma checagem embutida de bearer token; ative-a antes da primeira implantação.

```bash
# 1. Generate a token (32 bytes / 64 hex chars).
ai-memory generate-auth-token >> docker/.env.production
$EDITOR docker/.env.production    # prefix the new line with AI_MEMORY_AUTH_TOKEN=

# 2. Sync to the homelab + restart.
scp docker/.env.production "$SERVER:$DEPLOY_DIR/.env.production"
ssh "$SERVER" "cd $DEPLOY_DIR && docker compose up -d"
```

O log de inicialização agora mostrará `auth=true`. Verifique a partir do laptop:

```bash
curl -sI http://homelab:49374/handoff             # → HTTP/1.1 401 Unauthorized
curl -sI http://homelab:49374/handoff \
     -H "Authorization: Bearer $TOKEN"            # → HTTP/1.1 200 OK
```

**Em seguida, atualize cada cliente MCP** para enviar o mesmo token. O comando `ai-memory
install-mcp --client <name> --auth-token <token>` imprime o trecho
exato para cada cliente na [Matriz de Compatibilidade do README](../../README.md#support-matrix);
execute `ai-memory install-mcp --help` para os valores aceitos no momento. A CLI do agente
envia um cabeçalho `Authorization: Bearer <token>` em toda chamada; o middleware do
ai-memory valida com uma comparação em tempo constante. Para o Claude Code, prefira
a ponte com reconhecimento de sessão: `ai-memory install-mcp --client claude-code
--session-aware --apply --auth-token <token>` mantém o token em
`<data_dir>/auth-token` em vez de `~/.claude.json`, e após
`install-hooks --apply --auth-token <token>` o mesmo comando funciona sem
`--auth-token`, já que a ponte lê a chave mantida pelos hooks.

**Transporte criptografado.** HTTP em texto puro na LAN significa que qualquer um com
captura de pacotes pode ler o bearer token (e chaves nativas `aim_` assim que o modo
multiusuário for ativado) em trânsito. Adicione um proxy reverso
com terminação TLS à frente do ai-memory (Caddy com Let's Encrypt, Caddy com
sua CA interna, Cloudflare Tunnel, nginx ou arquivos de certificados externos)
quando você vincular além do loopback ou ativar múltiplos usuários.

**Consulte [`docs/https-via-proxy.md`](https-via-proxy.md)** para o guia completo
de implantação, incluindo:

- Quando adicionar TLS e quando dispensá-lo (os casos de loopback + stdio não precisam dele).
- Modelos prontos para copiar e colar para docker compose em [`docker/compose.tls.caddy.yml`](../../docker/compose.tls.caddy.yml) e [`docker/compose.tls.cloudflared.yml`](../../docker/compose.tls.cloudflared.yml).
- Instalação no repositório de confiança por sistema operacional para o caminho com CA interna (a etapa manual indispensável).
- [Hospedagem sob um subcaminho](https-via-proxy.md#hospedando-sob-um-subcaminho) via `--base-path` / `AI_MEMORY_BASE_PATH` quando o ai-memory compartilha um hostname com outros aplicativos.
- As seções de "o que pode dar errado", para que você não implemente segurança ilusória sem querer.

Para o Início Rápido de usuário único em loopback, o bearer token por si
só continua aceitável: o token barra o vizinho na LAN, e o loopback
impede a captura de pacotes. Adicione TLS assim que a implantação deixar de ser
"usuário único, máquina única".

## Implantações de rotina

Após a configuração inicial, cada implantação posterior é:

```bash
bin/deploy
```

O comando compila a imagem localmente, envia para o seu registro (registry), baixa no
homelab e reinicia. O arquivo compose + arquivo de ambiente no homelab permanecem
inalterados entre implantações; se precisar alterá-los, copie a nova
cópia com scp + execute novamente `bin/deploy`.

A etapa de reinicialização para o contêiner em execução com SIGTERM. O servidor
trata SIGINT e SIGTERM em ambos os transportes e limita cada espera em seu caminho
de encerramento a cinco segundos, portanto uma reinicialização (ou um simples `docker
stop` ou `docker compose down`) drena e encerra em poucos segundos
em vez de aguardar todo o período de carência do supervisor terminando em
SIGKILL. Antes da existência desses tratadores, os dois formatos de implantação falhavam
de maneiras diferentes. No contêiner, o servidor é o PID 1, e para o PID 1 o
kernel descarta um sinal cujo tratador não esteja instalado, de modo que o `docker
stop` queimava todo o seu período de carência e `docker kill` era a única saída.
Sob a unidade systemd nativa, o servidor não é o PID 1, de modo que o
`systemctl stop` caía na disposição padrão do kernel e o
matava instantaneamente: rápido, mas sem drenagem e cortando o worker de
consolidação durável de SessionEnd no meio do processo. Ali a parada
agora é mais lenta e limpa. O limite de cinco segundos é fixo e não
configurável. `docker kill` continua sendo a forma de parar o servidor sem
esperar pela drenagem. Não é necessário nenhum shim init de contêiner: o binário
instala seus próprios tratadores de sinal, portanto ele para corretamente como PID 1 e você
não precisa de `tini` ou `docker run --init`.

## Atualizando chaves de API

```bash
$EDITOR docker/.env.production
scp docker/.env.production "$SERVER:$DEPLOY_DIR/.env.production"
ssh "$SERVER" "cd $DEPLOY_DIR && docker compose up -d"
```

`docker compose up -d` lê o arquivo de ambiente e recria o contêiner
com os novos valores. Não é necessário recompilar.

## Opções de provedores LLM

O `.env.production.example` tem como padrão **Kimi 2.6 via OpenRouter**
(transporte openai-compat, $0.73/$3.49 por milhão de tokens). Outras
opções:

| Provedor | Modelo | Custo aprox. / consolidação | Observações |
|---|---|---|---|
| anthropic | `claude-haiku-4-5` | ~$0.02 | **Padrão recomendado.** Melhor equilíbrio entre velocidade, moderação e qualidade de classificação. Não é um modelo de raciocínio. |
| openai-compat (OpenRouter) | `moonshotai/kimi-k2.6` | ~$0.013 | Modelo de raciocínio; latência de ~2-3 min por consolidação. Aceitável porque a consolidação opera em fire-and-forget. |
| openai | `gpt-5.4-mini` | ~$0.002 | Alternativa mais barata e rápida. Qualidade decente. |
| openai-oauth | `gpt-5.5` | Assinatura ChatGPT | Backend ChatGPT/Codex. Execute `docker exec -it ai-memory ai-memory auth login openai-oauth` no host do servidor para que `<data_dir>/auth.json` fique no volume de dados montado. |
| copilot | `gpt-5.5` | Assinatura GitHub Copilot | Backend GitHub Copilot Chat. Execute `docker exec -it ai-memory ai-memory auth login copilot` no host do servidor ou defina `COPILOT_GITHUB_TOKEN`. |
| gemini | `gemini-3.5-flash` | nível gratuito cobre uso pessoal | Hospedado pelo Google, saída estruturada nativa com `responseSchema`. Defina `GEMINI_API_KEY` (ou `GOOGLE_API_KEY`). |
| openai-compat (Ollama) | `qwen3:32b` | $0 | Auto-hospedado. Defina `AI_MEMORY_LLM_BASE_URL=http://host.docker.internal:11434/v1`. Qualidade depende do modelo. |

> **O que não recomendamos:** modelos em modo de raciocínio (Kimi-K2.6 em modo de raciocínio,
> Claude com pensamento estendido, GPT-o3, variantes "thinking" do Gemini). Eles queimam
> orçamento de tokens em raciocínio interno antes de emitir a saída, travando ou emitindo respostas
> vazias com o prompt de consolidação em JSON estrito. Se você precisar usar um, desligue
> o raciocínio. Para um modelo da classe Qwen3 hospedado em vLLM / SGLang, o equivalente local
> é `AI_MEMORY_LLM_COMPAT_DISABLE_THINKING=true` (ele envia
> `chat_template_kwargs: {"enable_thinking": false}` em toda requisição
> openai-compat); se o mecanismo ainda truncar o payload estruturado ou retornar conteúdo
> vazio, o trabalho falha rapidamente com um erro terminal `truncated-response` /
> `empty-content` em vez de tentar novamente.

Os provedores da família OpenAI hospedados pelo ai-memory usam o modo estrito de `json_schema` para
saída estruturada. O provedor OpenAI normaliza a saída do schemars para o
subconjunto compatível da OpenAI (`additionalProperties: false`, `required` completo,
`anyOf` de enum gerado e nós `$ref` simples). Endpoints locais e de gateway
`openai-compat` usam a mesma requisição com restrição de esquema por padrão, com um
fallback tolerante para rejeição explícita de capacidade ou saída malformada. Defina
`AI_MEMORY_LLM_COMPAT_STRICT=false` para um endpoint incompatível. Se você mudar
para um modelo local de nicho, execute um rápido `ai-memory llm-test` antes de confiar nele.

Todo provedor de chat limita cada requisição HTTP em 300 segundos. Gateways hospedados
lentos (observados com planos de agregadores gratuitos) podem fazer streaming de uma conclusão longa
além desse teto e falhar todas as requisições com `http: error sending request`;
aumente `AI_MEMORY_LLM_TIMEOUT_SECS` no ambiente do contêiner para acompanhar o
tempo de geração no pior caso do gateway.

## Backups

> **Nota sobre a atualização para a 2.0:** a primeira inicialização de uma imagem 2.0 executa a migração
> do formato OKF, que arquiva todo o diretório de dados para
> `/data/backups/` no volume antes de alterar qualquer coisa (contêineres
> são detectados automaticamente; `AI_MEMORY_BACKUP_DIR` substitui). A
> página inicial da wiki mostra a localização do arquivo até que você o exclua. Consulte
> [`MIGRATION-2.0.md`](MIGRATION-2.0.md).

O diretório de dados é o que você montou em `docker-compose.prod.yml`
(padrão: `/var/opt/docker/utils/ai-memory/data/`). Ele contém:

```
data/
├── wiki/    # markdown — back up with rsync or git push to a remote
├── raw/    # immutable session log archive
├── db/     # memory.sqlite (FTS5 + entities + page_embeddings)
├── logs/   # daily rolling tracing
└── models/ # reserved for future local embedders
```

Para consistência em um ponto no tempo:

```bash
ssh "$SERVER" "docker exec ai-memory /usr/local/bin/ai-memory backup --to /data/snapshot-$(date +%F).tar.gz"
scp "$SERVER:$DEPLOY_DIR/data/snapshot-$(date +%F).tar.gz" ./backups/
```

O comando `ai-memory backup` usa a API de backup online do SQLite para que
escritas durante o snapshot permaneçam coerentes.

Para um padrão de backup remoto primeiro (fazer rsync do diretório de dados em um espelho git de acordo com
um agendamento, dar push para um repositório privado, combinado a um tarball para o banco de dados
e modelos), consulte [`docs/backup.md`](backup.md) e o exemplo prático em
[`docs/examples/backup/`](examples/backup/README.md).

## Compartilhando um servidor entre pessoas ou harnesses

Um servidor implantado é a forma aceita de compartilhar um projeto, seja entre
colegas de equipe ou entre vários harnesses que você mesmo executa. Verifique duas coisas
antes de distribuir a URL.

**Um servidor por diretório de dados.** Aponte dois processos `ai-memory serve` para o
mesmo `data/` (uma pasta sincronizada, um ponto de montagem NFS, dois contêineres em um mesmo volume) e
cada um executará seu próprio gravador e seu próprio controle de git na wiki. O SQLite
resiste a isso; a wiki e o estado em processo não. Execute um único servidor e deixe
todos se conectarem a ele, o que também é a base em que o modelo de conhecimento compartilhado
se apoia.

**Verifique o modo de isolamento.** Chamadas MCP sem escopo resolvem o "projeto atual"
por meio de um ponteiro que, a partir da v1.39, é indexado por chamador. A linha de inicialização informa
qual modo está ativo:

```
active-project isolation mode mode=PerActor …
```

`PerActor` é o padrão e aquele que você quer em um servidor compartilhado. `Single` é
um slot único para todo o processo. Isso funciona bem para um único harness, mas sessões concorrentes
o compartilharão, e **escritas** sem escopo também serão resolvidas por ele. Consulte
[auto-scope.md](auto-scope.md) e [users.md](users.md#running-for-a-team).

### Quanta carga um único servidor absorve

Toda escrita passa por um único ator gravador, o que é ideal para o SQLite, mas define um
teto. Estes números foram medidos, não estimados, com
`cargo test -p ai-memory-store --test writer_throughput -- --ignored --nocapture`:

| gravadores concorrentes | taxa de transferência (throughput) | latência média |
|---|---|---|
| 1 | 42/s | 23.9 ms |
| 8 | 295/s | 3.4 ms |
| 32 | 698/s | 1.43 ms |
| 128 | 700/s | 1.43 ms |

**O teto é de ~700 escritas/segundo**, atingido em torno de 32 gravadores concorrentes e
estável a partir daí: 128 gravadores produzem a mesma taxa de transferência com a mesma latência.
Após a saturação, o servidor aplica contrapressão (backpressure) em vez de se degradar. A fila
é limitada a 1024 com envio em espera, de modo que uma rajada maior que a fila desacelera
seus produtores e ainda grava toda escrita. Nada é descartado e nada
cresce descontroladamente.

**A latência de um único gravador é dominada por `fsync`, não por CPU.** Uma observação por
commit custa cerca de uma sincronização de disco; a concorrência permite ao SQLite agrupar commits do WAL,
e é por isso que a taxa de transferência sobe 17× enquanto a latência por escrita *cai*.

Para planejamento de capacidade: um agente trabalhando ativamente emite em média uma
escrita de ciclo de vida por chamada de ferramenta. Mesmo em um cenário pessimista de uma chamada de ferramenta por segundo
por agente, ~700/s corresponde a várias centenas de agentes ativos concorrentemente, muito além de uma
equipe e além da maioria das instalações compartilhadas. Algo diferente do gravador
vai quebrar primeiro.

Duas advertências antes de confiar cegamente nos números. Eles foram obtidos em um disco local
rápido; como o custo é de `fsync`, um sistema de arquivos de rede ou um volume lento terá números
substancialmente menores, o que é outro motivo para manter o diretório de dados em armazenamento
local. Eles também medem a camada de persistência, e não a porta de entrada HTTP; uma instalação que
sature isso terá muito mais probabilidade de ser limitada pelo lado do agente do que pelo
SQLite.

## Recuperando espaço em disco

O SQLite não devolve espaço excluído ao sistema de arquivos. Linhas excluídas deixam
suas páginas em uma **freelist**, que o SQLite reutiliza conforme o banco de dados cresce novamente.
Para uma base de dados em uso constante, esse é o comportamento correto e não exige atenção.

O `ai-memory status` informa o número que define a decisão:

```
  storage:      2.4 GiB on disk, 610.0 MiB reclaimable (25.4%)
    `ai-memory compact --confirm` would return it (blocks writes while it runs)
```

A linha de orientação só aparece quando a pendência vale a pena ser tratada. Quando isso
acontecer:

```bash
ssh "$SERVER" "docker exec ai-memory /usr/local/bin/ai-memory compact --confirm"
```

A compactação não exclui nada. Ela reconstrói os índices FTS e executa o `VACUUM`.

### Não agende um VACUUM noturno incondicional

Evite uma entrada de cron noturna para isso:

- O `VACUUM` obtém um **bloqueio exclusivo** e reescreve todo o banco de dados. Toda
  escrita bloqueia durante o processo, o que em uma base grande leva minutos. Este
  servidor atende a tráfego de hooks, de modo que bloquear escritas descarta capturas.
- Ele necessita de espaço livre em disco aproximadamente igual ao tamanho do próprio banco de dados, portanto o
  trabalho noturno também define um piso permanente de espaço livre.
- Como o SQLite reutiliza páginas livres, uma base em uso estável geralmente tem quase
  nada para recuperar. A execução noturna paga todo o custo sem nenhum benefício na maioria
  das noites.

O que deixa uma freelist volumosa é uma **exclusão pontual**: um
`purge-project`, uma grande varredura de retenção, um `forget-sweep` cobrindo meses de
páginas episódicas. Isso acontece uma vez e não de forma agendada, e os
comandos destrutivos já oferecem `--compact` em linha para esse momento.

Se você realmente quiser automatizar, torne o processo **condicional** ao número informado acima e
coloque-o em horários de pouco movimento:

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

Acione-o a partir de um timer do systemd (`OnCalendar=Sun 04:00`, `Persistent=true`) ou de uma
entrada de cron no host. Uma checagem semanal que geralmente não faz nada custa uma chamada simples
de `status`; um `VACUUM` noturno que geralmente não faz nada custa uma paralisação de escritas
toda noite.

### O que a compactação não faz

Ela não apaga dados permanentemente. O histórico git da wiki mantém o conteúdo das páginas em seus objetos e
mensagens de commit, e qualquer backup feito anteriormente continua contendo tudo. A compactação
devolve bytes do arquivo ativo do SQLite. Vale a pena fazer isso por seus próprios méritos,
mas ela não garante que o conteúdo seja irrecuperável. Consulte
[lifecycle-ops.md](lifecycle-ops.md) para os limites completos.

## Revertendo (rollback)

```bash
ssh "$SERVER" "cd $DEPLOY_DIR && \
               docker tag $IMAGE $IMAGE-rollback && \
               docker pull $IMAGE@sha256:<old-digest> && \
               docker tag $IMAGE@sha256:<old-digest> $IMAGE && \
               docker compose up -d --pull never --force-recreate ai-memory"
```

A reversão mais simples é trazer de volta uma imagem mais antiga por digest. Não
distribuímos um `bin/rollback` porque a maneira correta é manter a tag da
imagem anterior à mão antes de cada deploy (o Docker Hub guarda todo
push por digest gratuitamente).

## Acompanhando os logs

```bash
ssh "$SERVER" "docker logs -f --tail 100 ai-memory"
```

Ou navegue pelos logs diários rotativos no host:

```bash
ssh "$SERVER" "ls -la $DEPLOY_DIR/data/logs/"
ssh "$SERVER" "tail -100 $DEPLOY_DIR/data/logs/ai-memory.log.$(date +%F)"
```

## Solução de problemas

- **`Connection refused`** em `curl http://<host>:49374/mcp`: o
  contêiner não está em execução ou o mapeamento de portas está vinculado a `127.0.0.1`
  em vez de `0.0.0.0`. Verifique o `docker ps` no homelab.
- **Status `unhealthy`**: o contêiner está rodando, mas o healthcheck embutido
  `ai-memory status` está falhando. O mais provável é que as permissões do diretório
  de dados não correspondam ao usuário do contêiner (uid 1000). Corrija
  com `sudo chown -R 1000:1000 $DEPLOY_DIR/data` no host. Uma causa de manifestação
  mais lenta (dias, não minutos) é o esgotamento de descritores de arquivo (fd) por conexões de pares inativos
  (suspensão de laptop, oscilação de VPN) acumulando-se sem TCP keepalive; `serve`
  o habilita por padrão (`tcp_keepalive_secs = 60`, `AI_MEMORY_TCP_KEEPALIVE_SECS`
  para ajustar ou `=0` para desativar) (#792).
- **Incompatibilidade de embeddings após alteração de modelo**: a inicialização registra um aviso
  quando as tuplas `(provider, model, dim)` salvas diferirem da configuração.
  A busca híbrida ignora linhas desatualizadas até que sejam reincorporadas. Inicie
  o servidor normalmente, depois execute `ai-memory embed --force` para reconstruir
  todos os projetos no espaço de trabalho, ou adicione `--project <name>` para restringir
  o escopo da reconstrução. O backfill de embeddings agendado também pode preencher linhas
  ausentes quando ativado.
- **A captura parece atrasada, ou observações estão faltando**: `ai-memory
  status` relata a integridade do spool de hooks local: quantos eventos estão na fila
  do lado do cliente, a idade do mais antigo e o total de tentativas de entrega
  com falha:

  ```
    spool:
      pending:    2
      oldest:     15m 0s
      retries:    4
  ```

  Um spool não vazio com uma entrada mais antiga envelhecendo significa que os hooks estão alcançando
  a fila local, mas não o servidor; os eventos não são perdidos, eles são drenados
  assim que ele fica acessível. `pending: 0` significa que a captura está acompanhando normalmente.

  O spool fica **do lado do cliente**, portanto esta seção reflete a máquina na qual você
  executa o comando, e não o servidor. Trata-se do diretório de dados local mesmo quando
  `AI_MEMORY_SERVER_URL` apontar para um homelab. Ela também é exibida quando o
  servidor não puder ser acessado de forma alguma (em stderr, para que consumidores de `--json` continuem
  recebendo um único objeto em stdout), que é quando uma fila pendente é mais crítica. `--json` traz os mesmos números sob um objeto `spool`
  (`pending`, `oldest_age_ms`, `retries_total`).

- **Falhas de provedores**: `ai-memory status` relata a integridade passiva do LLM e
  dos embeddings a partir da última chamada real do provedor. Um processo recém-iniciado
  relata `unknown` até que o servidor realmente use aquele papel; ele não
  sonda provedores nem gasta tokens para relatórios de integridade. Se chamadas de provedores
  falharem com `invalid peer certificate: UnknownIssuer` atrás de um
  firewall ou antivírus que inspeciona TLS, consulte
  [https-via-proxy.md](https-via-proxy.md#chamadas-de-saída-para-llms-falham-atrás-de-um-firewall-com-inspeção-tls).
- **Loop de reinicialização do contêiner**: verifique
  `docker logs ai-memory`. A linha `ai-memory starting` no topo
  relata a configuração resolvida; uma variável de ambiente obrigatória ausente (por exemplo
  `LLM_API_KEY` com `openai-compat` selecionado, mas sem modelo) falhará
  aqui com um erro claro.
