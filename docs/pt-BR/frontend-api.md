# Integração de frontend: `/api/v1`

> API JSON somente leitura e modelo de hospedagem de interface customizada para construir frontends
> de terceiros contra um servidor `ai-memory`. Adicionado na **v0.6.0** (PR #7).
> Tudo abaixo tem como origem os manipuladores de rotas reais em
> `crates/ai-memory-web/src/routes/api.rs` e as estruturas de resposta em
> `crates/ai-memory-store/src/reader.rs`. Trate esses arquivos como a referência
> canônica se algo aqui sofrer divergência.

## 1. O que é esta superfície (e o que ela não é)

| | O que você pode fazer | O que você não pode fazer |
|---|---|---|
| `/api/v1/*` | Navegar por espaços de trabalho, projetos, páginas; ler o markdown completo da página + frontmatter + links inversos (backlinks); listar as versões armazenadas de uma página e comparar duas delas; buscar (FTS5 global, ou com escopo através do classificador híbrido de `memory_query`, em um ou vários projetos); ler os grafos de links interprojetos e intraprojeto; snapshots de "visão geral" agregados; analisar detalhadamente páginas obsoletas / duplicadas / órfãs; listar as sessões de um projeto e ler as observações brutas de uma sessão. | Escrever, excluir, renomear, verificar com lint, consolidar, executar varreduras, gerenciar handoffs. A superfície `/api/v1` é **somente leitura por construção**: os manipuladores contêm zero chamadas ao gravador, e o `crates/ai-memory-web/clippy.toml` proíbe métodos de escrita e destrutivos de `Wiki` em qualquer lugar da crate; assim, adicionar um método desses falha com `cargo clippy -D warnings`. Gravações continuam passando por `/admin/*` (usado pela CLI) ou ferramentas MCP. |
| `--web-ui-dir` | Hospedar qualquer SPA em `/web` (ou `--web-slug`), com a mesma origem da API, sob a mesma autenticação. O navegador `/web` embutido padrão continua como fallback quando a flag estiver ausente. | Hospedar a SPA em uma origem *diferente* sem um proxy reverso. Use hospedagem na mesma origem ou configure CORS deliberadamente (consulte §9). |

## 2. Modelo de autenticação

A navegação web embutida também lê `GET /web/inbox/notifications` (substitua
`/web` pelo ponto de montagem configurado). Ela retorna `pending`, `project_count` e até
cinco `projects`, cada um com `workspace`, `project` e `pending`, filtrados pelo
acesso ao repositório do visualizador. A resposta é protegida e não consome mensagens,
usa `Cache-Control: no-store` e não contém o corpo ou assunto da mensagem.
Links de notificação abrem `/web/inbox?workspace=...&project=...&state=pending`.
O filtro opcional `state` (`pending`, `claimed`, `cancelled`, ou vazio para
todos) é aplicado antes da paginação do histórico, e estados desconhecidos são recusados.
Correspondências recebidas usam `pending` como padrão quando `state` for omitido, e um
`state=` vazio mostra todo o histórico. Mensagens enviadas têm todos os estados como padrão.

As visualizações HTML de projeto e sessão aceitam `calls_q` (uma substring literal de modelo, harness ou
resposta, de no máximo 256 caracteres), `calls_sort=tokens|recent`,
`calls_signal=all|signals` e `calls_offset`. O banco de dados aplica escopo e
propriedade, busca, filtragem de sinais e ordenação antes de selecionar a
página de cinquenta respostas. Os totais e médias de sinais continuam cobrindo todas as respostas autorizadas
naquele escopo. Esses parâmetros de consulta HTML são separados da
paginação de observações e não adicionam capacidade de escrita nem um novo endpoint
em `/api/v1`. O consumo do projeto e da sessão é atualizado a cada dez segundos enquanto estiver visível,
e os controles de busca e o foco permanecem no lugar.

`/api/v1/*` e `/admin/*` são superfícies com autenticação dupla:

- Operadores humanos autenticam-se com `POST /auth/login`. O motor retorna um
  cookie `ai_memory_session` com `HttpOnly`, `SameSite=Strict` mais um cookie
  `ai_memory_csrf` legível. Requisições no navegador usam `credentials: "include"`;
  mutações também copiam o cookie CSRF para `X-CSRF-Token`.
- Clientes máquina enviam `Authorization: Bearer <token>`. O
  `AI_MEMORY_AUTH_TOKEN` estático tem autoridade de root de máquina; chaves de API nativas `aim_`
  são sempre de nível User. Bearers nunca autenticam em `/auth/*`.
- Um Bearer reconhecido tem precedência sobre qualquer credencial de navegador. Um Bearer
  inválido falha de forma fechada em vez de recorrer a fallback. Antes da ativação da autenticação humana,
  a compatibilidade legada com Basic/cookie é avaliada para requisições GET; após
  a ativação, Basic e esquemas desconhecidos não anulam uma sessão web válida.
- `/mcp`, `/identity`, hooks, handoffs e rotas de workstream são exclusivos de máquina. Um cookie
  de sessão web não pode autenticá-los.
- Um cabeçalho `Host` não permitido recebe `403 Forbidden` antes da avaliação de autenticação
  (proteção contra DNS-rebinding).

O shell customizado da SPA e seus ativos estáticos são públicos para que a tela de login possa
carregar. Suas rotas de dados permanecem protegidas. Quando `--web-ui-dir` estiver **ausente**, a
wiki embutida expõe páginas HTML públicas `GET {web_slug}/login` e
`GET {web_slug}/change-password` que chamam os mesmos endpoints `/auth/*`;
GETs navegacionais do navegador para rotas protegidas da wiki redirecionam para lá
em vez de retornarem JSON puro 401/403. `/api/v1` permanece em JSON em ambos os casos.
A wiki embutida também serve `GET {web_slug}/w/{workspace}/{project}/h/{*path}`
(o histórico de versões e diff de uma página, o par em HTML de §4.13) e
`GET {web_slug}/w/{workspace}/{project}/s/{session_id}` (as observações de uma
sessão em ordem de captura, uma página por vez, através dos mesmos leitores,
regra de visibilidade e limites de §4.11 e §4.12; `offset`, `limit` limitados
a `1..=200`, padrão `50`, e `at` para pular para a página contendo a posição
com base 1). Ambos seguem a mesma regra de leitura por projeto da visualização de página.
A wiki embutida também serve `GET {web_slug}/graph` (o par em HTML de
§4.9: um SVG circular de projetos mais uma tabela de arestas no nível de página) e
`GET {web_slug}/w/{workspace}/{project}/graph` (o par em HTML de §4.9.1).
A wiki embutida também serve a página `GET {web_slug}/pending`, exclusiva de root. Ela
lista propostas pendentes de autoaprimoramento e envia aprovações e rejeições para as
rotas existentes `/admin/pending-writes/*`. Um usuário não root conectado recebe uma
página 403, e não o redirecionamento para mudança de senha. Um navegador com a credencial legada Basic
somente GET vê os comandos `ai-memory pending-writes` de cada proposta no
lugar dos botões, visto que suas requisições POST seriam recusadas. A página `GET {web_slug}/health`,
exclusiva de root, segue a mesma barreira: contadores de hook (aceitos,
descartados, 429), fila do gravador, carretel e diário de recuperação sob o diretório
de dados do servidor, fila de consolidação, estado do embedder e provedor e as
checagens com falha do servidor com seus comandos de correção. É apenas HTML, responde
`Cache-Control: private, no-store` e exibe contagens, carimbos de data/hora e rótulos
configurados, nunca o texto de erro do provedor ou conteúdo de página.
`GET {web_slug}/search?q=…&workspace=…&project=…&explain=1` é a busca
no projeto: o mesmo classificador híbrido e regra de leitura por projeto de um `/api/v1/search`
com escopo (§4.5), e com `explain=1` um rastreamento de recall (classificações de fluxo por ocorrência,
pontuações brutas, contribuições de RRF, pontuação fundida, autoridade, reforço de intenção,
pontuação de rerank, mais tempos por fluxo). `limit` varia de `1..=100`, padrão
`20`; sem um escopo, trata-se da busca FTS5 entre projetos (padrão `50`), cujo
rastreamento é apenas o tempo de FTS. Comece
com:

```http
GET /auth/me
```

Uma resposta autenticada inclui a identidade do operador, o estado da alteração
de senha e as permissões calculadas pelo servidor. Em um servidor de loopback intencionalmente
sem configuração, ela retorna um snapshot anônimo explícito. Quando a autoridade
humana estiver configurada, o login é:

```http
POST /auth/login
Content-Type: application/json

{"username":"alice","password":"…"}
```

`/auth/login` e `/auth/recovery` respondem com 415 a qualquer outro tipo de conteúdo, portanto um
envio de formulário ou uma requisição `text/plain` de outro site não pode alcançá-los.

Não coloque `AI_MEMORY_AUTH_TOKEN`, chaves `aim_`, valores de sessão ou valores de CSRF
no `localStorage`. Antes que qualquer senha humana ou bootstrap concluído exista,
a compatibilidade legada de navegador somente GET pode aceitar o bearer de root via
HTTP Basic, ou um cookie HttpOnly `ai_memory_auth` definido por uma versão anterior; um login
via Basic não define mais esse cookie. A ativação humana desativa
esse caminho imediatamente. Consulte [`docs/users.md`](users.md) para bootstrap, rotação
de senha, recuperação, papéis, expiração de sessão e ciclo de vida de chaves de API.

Clientes máquina podem chamar `GET /identity` com sua chave bearer, mesmo quando a
interface web estiver desativada. Informa apenas o chamador autenticado e a versão do servidor:

```json
{"version":"2.5.2","level":"user","operator":"user:alice","distinguishes_operators":true}
```

`level` é `root`, `user` ou `anonymous`; `operator` é a chave de identidade
qualificada, ou `null` em um servidor não autenticado de usuário único. Cookies de sessão web
não podem autenticar esta rota. Sua resposta usa `Cache-Control: private,
no-store`. Consulte [users.md](users.md) para chaves de máquina e tipos de identidade.

## 3. Modelo de erros

Todos os erros retornam um corpo JSON no formato:

```json
{ "error": "human-readable message" }
```

com um destes status:

| Status | Quando |
|---|---|
| `400 Bad Request` | parâmetros de consulta inválidos, entrada malformada, escopo parcial (espaço de trabalho sem projeto ou vice-versa), escopos em excesso em `POST /search` (>25), `q` vazio, id de sessão malformado, `kinds` ou `order` de observações desconhecidos. |
| `401 Unauthorized` | sessão humana ou Bearer de máquina ausente, expirado, revogado ou inválido. |
| `403 Forbidden` | cabeçalho Host fora da lista permitida; mutação via cookie sem CSRF válido; mudança de senha pendente; ou a autoridade autenticada não possui a capacidade solicitada. |
| `404 Not Found` | espaço de trabalho, projeto ou página não existe; arquivo da página ausente no disco; ou um id de sessão que não é visível naquele projeto para o chamador. |
| `429 Too Many Requests` | limite de taxa de login/recuperação ou fila delimitada de KDF de senha saturada. |
| `500 Internal Server Error` | falha no pool de leitores / SQLite. O corpo é sempre um erro genérico fixo; a causa subjacente é registrada nos logs do servidor em vez de retornada, de modo que não pode vazar caminhos ou configurações para um navegador. |

## 4. Referência de endpoints

Todos os endpoints são `GET`, a menos que indicado. Caminhos sob `/api/v1/`.

### 4.1 Espaços de trabalho

```http
GET /api/v1/workspaces
```

**Resposta:** uma matriz JSON simples, `[WorkspaceSummary, …]`

```json
[
  {
    "workspace_name": "default",
    "project_count": 3,
    "page_count": 412,
    "last_updated": "2026-05-28T14:02:11.123Z"
  }
]
```

`last_updated` é `null` para um espaço de trabalho vazio.

### 4.2 Projetos

```http
GET /api/v1/projects                  # all projects across all workspaces
GET /api/v1/projects?workspace=NAME   # projects in one workspace
```

**Resposta:** uma matriz JSON simples, `[ProjectSummary, …]`

```json
[
  {
    "workspace_id": "0193e7a1-0000-7000-8000-00000000000a",
    "workspace_name": "default",
    "project_id": "0193e7a1-0000-7000-8000-00000000000b",
    "project_name": "ai-memory",
    "page_count": 138,
    "last_updated": "2026-05-28T14:02:11.123Z"
  }
]
```

`project_id` é o endereço estável de um projeto: nomes podem colidir (uma linha
gêmea legada pode carregar o nome antigo de outro projeto), e as operações administrativas
de projeto aceitam o id onde o nome for ambíguo. A linha de um projeto restrito,
id incluso, fica oculta para um chamador sem concessão de acesso.

### 4.3 Páginas (listar)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/pages
```

**Resposta:** uma matriz JSON simples, `[PageSummary, …]`

```json
[
  {
    "path": "decisions/0007-db.md",
    "title": "Standardised on Postgres",
    "kind": "decision",
    "tier": "semantic",
    "updated_at": "2026-05-27T09:12:00.000Z"
  }
]
```

`404` se o espaço de trabalho ou projeto não existir. Uma página cujo TTL de `expires_at`
já passou é omitida, assim como a busca e `recent` a omitem; a
leitura de página única abaixo ainda a retorna.

### 4.4 Página (leitura completa)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/pages/{*path}
```

O caminho na wiki é um curinga: `decisions/0007-db.md`, `concepts/foo/bar.md`,
etc. Retorna metadados mesclados + markdown do corpo + frontmatter + links
resolvidos + links inversos (backlinks).

**Resposta (objeto plano):**

```json
{
  "id": "01a11da7-0604-7cc1-b518-1f0e0f2d4705",
  "project": "ai-memory",
  "path": "decisions/0007-db.md",
  "title": "Standardised on Postgres",
  "kind": "decision",
  "tier": "semantic",
  "pinned": true,
  "created_at": "2026-05-27T09:12:00.000Z",
  "updated_at": "2026-05-28T11:04:33.123Z",
  "supersedes": null,
  "frontmatter": { "tags": ["adr"], "pinned": true },
  "body_markdown": "# Standardised on Postgres\n\n…",
  "links":     [ { "path": "concepts/db-rules.md", "title": "DB rules", "kind": "rule" } ],
  "backlinks": [ { "path": "sessions/2026-05-27.md", "title": "Session 2026-05-27", "kind": "session" } ]
}
```

`id` é o id da versão mais recente. Ele muda a cada nova versão, portanto é
o token a ser passado de volta como `expected_page_id` para um
`memory_write_page` ou `memory_delete_page` condicional.

`created_at` é quando a versão retida mais antiga da página foi escrita, portanto
permanece fixo entre reescritas; `updated_at` é quando a versão mais recente foi escrita.

`404` para espaço de trabalho/projeto ausente, linha de página ausente ou arquivo ausente
no disco (o corpo é lido do arquivo markdown no momento da requisição).

### 4.5 Busca

Duas formas: query-string para o caso comum de escopo único ou global, e um
corpo JSON para múltiplos escopos.

```http
GET /api/v1/search?q=karpathy&limit=20                                # global
GET /api/v1/search?q=karpathy&workspace=default&project=ai-memory     # one project
```

```http
POST /api/v1/search
Content-Type: application/json

{
  "q": "karpathy",
  "scopes": [
    { "workspace": "default", "project": "ai-memory" },
    { "workspace": "default", "project": "shared-notes" }
  ],
  "limit": 20
}
```

**Resposta:** um array JSON simples, `[SearchHit, …]`

```json
[
  {
    "workspace": "default",
    "project": "ai-memory",
    "path": "concepts/karpathy-wiki.md",
    "title": "Karpathy LLM Wiki pattern",
    "kind": "concept",
    "snippet": "Andrej <mark>Karpathy</mark>'s LLM wiki design …",
    "rank": -8.4
  }
]
```

Regras:

- `q` é obrigatório e não vazio (400 caso contrário).
- `limit` é limitado a `1..=100`. Padrão `10`.
- Escopo parcial é **rejeitado** com `400` (passar apenas `workspace` ou
  apenas `project` para manter o escopo inequívoco).
- `scopes` (POST) tem limite máximo de `25` entradas; não pode ser combinado com
  `workspace`/`project` no nível superior.
- `snippet` contém marcadores HTML do FTS5 (`<mark>…</mark>`) ao redor dos
  termos correspondidos.
- Uma busca **global** (sem escopo) é FTS5 sobre cada projeto que o chamador pode
  ler, como `memory_query(global=true)`; `rank` é a classificação FTS5.
- Uma busca **com escopo** (`workspace`+`project`, ou `scopes`) executa o ranqueador
  híbrido que `memory_query` usa para um projeto: FTS5, entidade, vetor (quando o
  servidor tem um embedder), streams de grafo e observação fundidos por RRF, o
  ajuste de autoridade delimitado e o reclassificador opcional. A mesma consulta
  retorna a mesma ordem que `memory_query`; `rank` é a pontuação fundida negada
  e ajustada por autoridade. Vários `scopes` são mesclados pela melhor classificação por
  página.
- `rank`: **menor é melhor** em ambos os modos.

### 4.6 Recentes

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/recent?limit=20
```

Páginas com `is_latest = 1` cujo TTL `expires_at` ainda não passou, ordenadas por
`updated_at` DESC por instante, com caminho ascendente para registros de data e hora iguais.
A precisão de fração de segundo nos registros de data e hora serializados não altera essa
ordem. `limit` limitado a `1..=100`, padrão `10`.

Toda superfície de leitura usa o mesmo contrato de `kind`. Um `kind` explícito no
frontmatter tem precedência; caso contrário, as famílias de caminho `_rules/`, `_slots/`, `sessions/`,
`decisions/`, `gotchas/`, `concepts/`, `procedures/` e `notes/` derivam
`rule`, `slot`, `session`, `decision`, `gotcha`, `concept`, `procedure` e
`note`, respectivamente. Outros caminhos recorrem a `fact`.

**Resposta legada:** um array JSON simples de objetos `PageSummary`.

```json
[
  {
    "path": "sessions/2026-05-28.md",
    "title": "Session 2026-05-28",
    "kind": "session",
    "updated_at": "2026-05-28T14:02:11.123Z"
  }
]
```

Forneça `updated_since` (RFC 3339) ou `cursor` para optar pela paginação incremental:

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/recent?updated_since=2026-09-30T00%3A00%3A00Z&limit=20
```

```json
{"pages": [], "next_cursor": null}
```

O corte é exclusivo (`updated_at > updated_since`). Os resultados são ordenados
por `(updated_at, path)` ascendente e buscados com uma consulta SQL delimitada. Passe
`next_cursor` inalterado para continuar; um corte opcional deve corresponder ao corte do
cursor. Os cursores usam base64 seguro para URL com versão e sem preenchimento, são opacos para os
clientes e são limitados a 8192 caracteres (6144 bytes decodificados). Eles estão vinculados
ao espaço de trabalho e projeto. Cursores inválidos, de escopo externo ou conflitantes
retornam 400. Cada requisição verifica novamente a autorização e usa
`Cache-Control: private, no-store`.

Resultados incrementais omitem páginas substituídas e expiradas, incluindo páginas fixadas expiradas.
Eles não fornecem snapshot ou feed de exclusão: alterações simultâneas podem aparecer
em uma página posterior, e páginas excluídas ou expiradas ficam ausentes. Chamadas sem nenhum
dos parâmetros retêm o array legado, a ordem decrescente e o comportamento de cache.

### 4.7 Briefing (snapshot estruturado)

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/briefing?limit=10
```

Mesmo payload que `memory_briefing` retorna: contagens + janelas de atividade +
última observação + handoffs abertos + `_rules/` + `_slots/` + N páginas mais
recentes, mais `lint` (as contagens de `_lint/report.md`) quando o projeto tem um
relatório. Sem LLM, determinístico.

**Resposta:** `BriefingSnapshot`

```json
{
  "counts": {
    "pages_latest": 138,
    "pages_all": 162,
    "sessions": 27,
    "observations": 4198
  },
  "activity_7d":  { "days": 7,  "sessions": 6,  "observations": 921,  "pages_updated": 41 },
  "activity_30d": { "days": 30, "sessions": 24, "observations": 3712, "pages_updated": 102 },
  "last_observation_at": "2026-05-28T13:58:02.123Z",
  "pending_handoff_count": 0,
  "rules": [{ "path": "_rules/postgres.md", "title": "Postgres only", "kind": "rule",  "updated_at": "…" }],
  "slots": [{ "path": "_slots/focus.md",    "title": "Current focus", "kind": "slot",  "updated_at": "…" }],
  "recent_pages": [
    { "path": "sessions/2026-05-28.md", "title": "Session 2026-05-28", "kind": "session", "updated_at": "…" }
  ]
}
```

### 4.8 Visão geral (agregados de workspace + projeto)

```http
GET /api/v1/workspaces/{workspace}/overview?limit=10
GET /api/v1/workspaces/{workspace}/projects/{project}/overview?limit=10
GET /api/v1/workspaces/{workspace}/projects/{project}/handoffs?state=open&limit=50
GET /api/v1/workspaces/{workspace}/projects/{project}/handoffs?all_owners=true
```

### Listagem de handoffs

`state` aceita `open` | `accepted` | `expired`; omita-o para listar todos os estados,
que é como você encontra um bastão que já foi consumido. Os resultados são delimitados por
proprietário: um chamador autenticado vê os seus próprios mais os handoffs compartilhados, um
navegador anônimo vê apenas os compartilhados. Um handoff com proprietário (e o texto derivado
de prompt dentro dele) nunca é renderizado para alguém a quem ele não pertence.
O mesmo escopo se aplica ao campo `handoff` de ambos os endpoints de visão geral e
ao `pending_handoff_count`, de modo que a contagem e a busca sempre concordem.
Para recuperação, uma requisição autorizada como root pode passar `all_owners=true` para listar as
linhas de todos os operadores. Requisições de usuário e anônimas recebem `403`; o padrão permanece
próprio mais compartilhado, inclusive para o root.

Em um servidor que autentica, os campos da listagem derivados de prompt
(`summary`, `open_questions`, `next_steps`) são servidos a um chamador que o servidor
possa identificar e ao operador root; um handoff automático os sintetiza
literalmente a partir dos prompts do operador, e a listagem retorna todo o
histórico do projeto em vez da única linha aberta mais recente. Um chamador que não seja
identificado nem root tem os campos ausentes e `redacted` definido como `true`; os metadados
(estado, registros de data e hora, agente, cwd, arquivos tocados, propriedade) são sempre servidos. Um
servidor sem autenticação configurada serve os corpos, já que ele já serve todo
corpo de página sem autenticação.

"Possa identificar" significa a identidade que o próprio nível de autenticação resolveu
(`ActorContext::identity_key()`): o par emissor/sujeito afirmado quando houver
um, caso contrário o nome de usuário. Um ingress que termina OIDC e encaminha ambos
`X-Memory-Actor-Issuer` e `X-Memory-Actor-Sub`, portanto, lê seus próprios
handoffs e os compartilhados com `redacted: false`, e nenhum degrau da cadeia de
autenticação produz um
chamador autenticado mas inominável hoje. O braço de redação é um piso
à prova de falhas, não um nível ativo. `owner` / `accepted_by` contêm a chave de armazenamento qualificada
(`user:alice`, `oidc:<issuer-byte-length>:<issuer><subject>`).

```json
{
  "handoffs": [
    {
      "id": "01930…",
      "agent": "claude-code",
      "at": "2026-07-28T12:00:00Z",
      "state": "accepted",
      "summary": "…",
      "open_questions": [],
      "next_steps": [],
      "redacted": false,
      "files_touched": [],
      "owner": "user:alice",
      "accepted_by": "user:alice",
      "accepted_at": "2026-07-28T13:00:00Z"
    }
  ]
}
```

Reúne o que um frontend geralmente precisa em sua visualização inicial em uma única viagem de ida e volta.

**Visão geral do espaço de trabalho** retorna o handoff aberto mais recente em todo o espaço de trabalho,
mais `briefing` e `health` agregados em todos os seus projetos:

```json
{
  "handoff":  { "agent": "claude-code", "at": "…", "project": "ai-memory", "summary": "…", "open_questions": [ … ], "next_steps": [ … ] },
  "briefing": { "counts": { … }, "activity_7d": { … }, "rules": [ … ], "recent_pages": [ … ] },
  "health":   { "stale": 4, "duplicates": 1, "contradictions": 0, "orphans": 12,
                "audited_at": null, "stale_pages": [HealthPage, …],
                "duplicate_pages": [ … ], "orphan_pages": [ … ] }
}
```

**Visão geral do projeto** usa o mesmo formato de resposta, delimitado àquele projeto. Em
qualquer uma das respostas, `handoff` é `null` quando nenhum handoff aberto corresponde ao escopo:

```json
{
  "handoff":  { "agent": "claude-code", "at": "…", "project": "ai-memory", "summary": "…", "open_questions": [ … ], "next_steps": [ … ] },
  "briefing": { … },
  "health":   { … }
}
```

`HealthPage`:

```json
{
  "workspace": "default",
  "project": "ai-memory",
  "path": "concepts/old-thing.md",
  "title": "Old thing",
  "kind": "concept"
}
```

> Nota: `handoff` **não** é consumido pela API de leitura. O
> handoff permanece "aberto" e ainda pode ser aceito pelo próximo agente.

### 4.9 Grafo entre projetos

```http
GET /api/v1/graph
GET /api/v1/graph?workspace={workspace}&project={project}
```

Retorna todo wikilink resolvido cujos endpoints estejam em projetos
diferentes, com espaço de trabalho + projeto + caminho de ambos os endpoints. Útil para
renderizar uma visualização de dependências em nível de projeto na SPA. Omita a query
string para o grafo inteiro. `workspace` e `project` devem ser fornecidos
juntos e mantêm apenas as arestas que tocam aquele projeto; passar apenas um
deles é `400`. Um projeto desconhecido ou ilegível é a mesma recusa
que as outras rotas com escopo (`404` / `403`). Com a autorização ativada, uma
aresta ainda exige que ambas as extremidades sejam legíveis.

```json
{
  "edges": [
    {
      "from_workspace": "default",
      "from_project":   "ai-memory",
      "from_path":      "decisions/0014-storage.md",
      "to_workspace":   "default",
      "to_project":     "infra",
      "to_path":        "runbooks/sqlite-wal.md"
    }
  ]
}
```

### 4.9.1 Grafo de links do projeto

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/graph
```

Páginas mais recentes não expiradas naquele projeto e os wikilinks do
mesmo projeto resolvidos entre elas. Links entre projetos e não resolvidos são omitidos.
Quando o projeto tem mais de 120 páginas ativas, a resposta mantém as
páginas com o maior número de vizinhos no mesmo projeto e, em seguida, as atualizadas mais
recentemente, e define `truncated` como `true`. A mesma regra de leitura por
projeto que a lista de páginas se aplica.

```json
{
  "workspace": "default",
  "project": "ai-memory",
  "truncated": false,
  "nodes": [
    { "path": "decisions/0014-storage.md", "title": "Storage", "degree": 2 }
  ],
  "edges": [
    { "from_path": "concepts/wiki.md", "to_path": "decisions/0014-storage.md" }
  ]
}
```

### 4.10 Ícone da aba do navegador

```http
GET /favicon.ico
```

Retorna o mesmo PNG transparente que a interface web integrada serve como
logotipo do cabeçalho. Os navegadores buscam esse caminho automaticamente. A rota está
presente sempre que a interface web estiver ativada (`--enable-web`) e é
montada na raiz absoluta do host, fora de `--base-path` e fora
do aninhamento `/web`, para que a busca automática do navegador o alcance mesmo
sob uma implantação em subcaminho. A resposta é `image/png` apesar da
URL `.ico` (navegadores modernos aceitam ícones PNG), e a rota é
**isenta de autenticação e de allowlist de host**: uma nova aba pode buscar o
ícone antes do login, e o PNG incorporado é o mesmo que qualquer visitante de `/web`
já vê, portanto a superfície de vazamento de informações é nula.

### 4.11 Sessões

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/sessions?limit=20&offset=0&include_open=false
```

Sessões que tocaram o projeto, as mais novas primeiro: uma sessão é listada quando sua
linha está ancorada no projeto OU pelo menos uma de suas observações foi registrada
lá, de modo que uma sessão que mudou de repositório no meio do caminho aparece em ambos.
`observation_count` conta apenas as linhas deste projeto. `limit` limitado a
`1..=100`, padrão `20`; `offset` padrão `0`; `include_open` padrão
`false` (apenas sessões efetivamente encerradas). Observações substantivas no
projeto solicitado após o encerramento persistido fazem a visualização de leitura abrir a sessão
novamente: `ended_at` é `null`, e `include_open=false` a exclui.
Observações apenas de ciclo de vida não a reabrem. O encerramento armazenado continua sendo a marca
d'água de finalização do gravador, e esta leitura não altera o histórico nem a admissão
de handoffs. Filtrado por proprietário como handoffs: um
chamador que o servidor possa identificar vê suas próprias sessões mais as não atribuídas;
um chamador anônimo vê apenas as não atribuídas. Nunca armazenado em cache (`no-store`).

**Resposta:** `{ "sessions": [SessionSummary, ...] }`

```json
{
  "sessions": [
    {
      "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
      "cwd": "/home/me/src/app",
      "agent_kind": "claude-code",
      "started_at": "2026-08-16T09:12:03.412Z",
      "ended_at": "2026-08-16T10:47:55.001Z",
      "observation_count": 143,
      "actor_user": null,
      "consolidation": {"state": "completed", "attempts": 1}
    }
  ]
}
```

`consolidation` é `null` quando não existe nenhum job para aquela sessão no projeto
solicitado. Caso contrário, contém o estado da geração mais recente (`pending`,
`running`, `completed`, `failed` ou `superseded`) e a contagem de tentativas. Os jobs são
selecionados no mesmo escopo e consulta que as sessões filtradas por proprietário; erros do
provedor são omitidos. Isso relata o estado do job, não o frescor de cada página.

### 4.12 Observações da sessão

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/sessions/{session_id}/observations?limit=50&offset=0&order=asc&kinds=user-prompt,stop&q=migration&body_max_chars=4000
```

Observações brutas de hook de uma sessão (prompts, chamadas de ferramenta, stops) como armazenadas,
paginadas. Apenas as linhas que caíram em `{workspace}/{project}` são retornadas;
`elided_other_scope` conta as linhas que a mesma sessão deixou em outro projeto.
A sessão deve estar visível sob o mesmo predicado de 4.11 (linha ou
observação no projeto, filtro de proprietário passa), caso contrário `404`. `limit`
limitado a `1..=200`, padrão `50`; `offset` padrão `0`; `order` é `asc`
(ordem de captura, padrão) ou `desc`; `kinds` é uma lista separada por vírgulas de
`session-start`, `user-prompt`, `pre-tool-use`, `post-tool-use`,
`pre-compact`, `post-compaction`, `notification`, `stop`, `session-end`,
`other`; `q` é uma consulta FTS5 restrita à sessão; `body_max_chars`
limitado a `200..=16384`, padrão `4000`, e um corpo mais longo termina com um marcador
visível `[body truncated; N chars omitted]`. `total` conta as linhas dentro do escopo
correspondentes a `kinds` e `q`, portanto pagine em `offset` sem uma segunda
chamada. Os corpos foram higienizados e delimitados na ingestão; trate-os como texto
histórico não confiável. Nunca armazenado em cache (`no-store`). Mesmo payload que a ferramenta MCP
`memory_read_session_observations`.

**Resposta:** `{ "session": SessionSummary, "observations": [ObservationRecord, ...], "total", "offset", "limit", "order", "elided_other_scope", "body_max_chars" }`

```json
{
  "session": {
    "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
    "cwd": "/home/me/src/app",
    "agent_kind": "claude-code",
    "started_at": "2026-08-16T09:12:03.412Z",
    "ended_at": "2026-08-16T10:47:55.001Z",
    "observation_count": 143,
    "actor_user": null,
    "consolidation": null
  },
  "observations": [
    {
      "id": "0198f0a2-4d5e-7f60-9a0b-1c2d3e4f5a6b",
      "session_id": "0198f0a2-3c4d-7e5f-8a9b-0c1d2e3f4a5b",
      "kind": "user-prompt",
      "title": "User prompt",
      "body": "Add a migration for the sessions table ...",
      "importance": 5,
      "created_at": "2026-08-16T09:12:10.020Z",
      "extension": null,
      "source_event": null
    }
  ],
  "total": 12,
  "offset": 0,
  "limit": 50,
  "order": "asc",
  "elided_other_scope": 0,
  "body_max_chars": 4000
}
```

### 4.13 Histórico e diff de páginas

```http
GET /api/v1/workspaces/{workspace}/projects/{project}/history/{*path}?limit=50&offset=0&from={id}&to={id}
```

Cada versão armazenada de um caminho de página, as mais novas primeiro, e um diff unificado
entre duas delas. Versões substituídas (`is_latest: false`) são listadas: uma
versão que outra gravação substituiu permanece legível, portanto dois gravadores simultâneos
aparecem. `limit` limitado a `1..=200`, padrão `50`; `offset` padrão `0`;
`total` conta cada versão no caminho. `to` tem como padrão a versão mais
recente, `from` a versão que `to` substituiu (nenhuma para uma primeira versão, que
então faz diff contra vazio). Ambos são ids de versão desta listagem (o `id`
também retornado por §4.4). Um id de outro projeto, espaço de trabalho ou caminho é um
`404`, o mesmo que um desconhecido; um id malformado é um `400`. Um caminho sem
versões é um `404`. Versões expiradas seguem a regra de TTL de qualquer outra leitura:
uma versão cujo `expires_at` já passou não é listada e seu id é um `404`,
e uma página cuja versão ativa expirou não tem histórico algum (`404`), porque
a próxima varredura de esquecimento exclui todas as linhas em seu caminho. Um link `supersedes` para uma
versão oculta é `null`. Nunca armazenado em cache (`no-store`).

O diff é delimitado. Um lado maior que 512 KiB ou 10.000 linhas, ou um par com
mais de 1.000 linhas alteradas de distância, retorna `status: "too_large"` com um
`reason` e sem hunks. Caso contrário, `hunks` carrega no máximo 2.000 linhas, com
três linhas de contexto, e `truncated: true` quando o teto o interrompeu.
Os corpos são textos de página não confiáveis; renderize-os escapados.

**Resposta:**

```json
{
  "workspace": "default",
  "project": "app",
  "path": "notes/plan.md",
  "total": 2,
  "offset": 0,
  "limit": 50,
  "versions": [
    {
      "id": "01a1…b2",
      "supersedes": "01a1…a7",
      "is_latest": true,
      "title": "Plan",
      "tier": "semantic",
      "created_at": "2026-10-12T09:12:00.000Z",
      "body_chars": 412,
      "author": { "username": "carol" }
    },
    {
      "id": "01a1…a7",
      "supersedes": null,
      "is_latest": false,
      "title": "Plan",
      "tier": "semantic",
      "created_at": "2026-10-11T17:40:12.000Z",
      "body_chars": 380
    }
  ],
  "diff": {
    "from": { "id": "01a1…a7", "title": "Plan", "created_at": "2026-10-11T17:40:12.000Z" },
    "to":   { "id": "01a1…b2", "title": "Plan", "created_at": "2026-10-12T09:12:00.000Z" },
    "status": "changed",
    "truncated": false,
    "hunks": [
      {
        "old_start": 3, "old_len": 2, "new_start": 3, "new_len": 2,
        "lines": [
          { "op": " ", "text": "shared line" },
          { "op": "-", "text": "revision 0" },
          { "op": "+", "text": "revision 1" }
        ]
      }
    ]
  }
}
```

`author` é omitido para uma gravação não atribuída. `status` é `same`, `changed`
ou `too_large`; `from` é `null` quando `to` não substituiu nada neste caminho.

## 5. Limites e paginação

- A maioria dos parâmetros de query `limit` é limitada a `1..=100`; histórico de handoffs,
  observações de sessão e histórico de página são limitados a `1..=200`. Listagem de sessões e observações
  de sessão aceitam um `offset`; observações também relatam `total`.
- Corpos de observações de sessão têm limite máximo por linha definido por `body_max_chars`
  (`200..=16384`, padrão `4000`) com um marcador visível de truncamento.
- `POST /api/v1/search`: no máximo **25 escopos** por requisição.
- Limite máximo do corpo HTTP: **10 MB** (compartilhado com o limite de corpo do MCP; você não
  atingirá isso no tráfego normal da API).
- **Cache-Control + ETag.** Endpoints de leitura independentes de identidade usam
  `Cache-Control: private, max-age=N` com um TTL específico do endpoint; leituras de página
  também carregam um `ETag` SHA-256, e um `If-None-Match` correspondente recebe `304 Not
  Modified`. Respostas de briefing, overview, lista de handoffs, lista de sessões e
  observações de sessão e histórico de página dependem do agente autenticado e, portanto, usam
  `Cache-Control: private, no-store`, de modo que
  um navegador não possa reutilizar a resposta derivada de prompt de Alice após as credenciais na
  mesma URL mudarem para Bob. Respostas de busca não são armazenáveis em cache porque o
  corpo da requisição afeta o resultado.

## 6. Hospedagem de UI customizada e caminhos base

Para um companion apenas de API, monte a superfície JSON protegida sem uma interface
de navegador:

```bash
ai-memory serve --transport http --enable-api
```

`--enable-web` continua a implicar `/api/v1` para compatibilidade. `--enable-api`
isoladamente não monta `/web`, suas páginas de login, assets estáticos ou a rota não autenticada
de favicon. A API usa o mesmo gate de autenticação Bearer de máquina ou sessão de navegador
em qualquer um dos modos.

```bash
ai-memory serve \
    --transport http \
    --bind 127.0.0.1:49374 \
    --enable-web \
    --web-ui-dir /path/to/your-spa/dist
```

O diretório estático é servido em `/web` via `tower-http::ServeDir`:

- **Casca pública, dados protegidos.** `index.html`, fallbacks do roteador do cliente e
  assets estáticos carregam sem credenciais para que o formulário de login possa renderizar.
  `/auth/me`, `/api/v1/*` e `/admin/*` impõem o contrato de sessão/Bearer
  acima. O roteador wiki integrado usado quando `--web-ui-dir` está ausente permanece
  protegido porque renderiza dados no lado do servidor.
- **Fallback de SPA.** Caminhos ausentes recorrem a `index.html`, para que um
  roteador do lado do cliente (React Router, SvelteKit, etc.) possa assumir
  `/web/whatever` sem 404s.
- **Path traversal é rejeitado** pela segurança padrão do `ServeDir`.
- **Validação prévia à inicialização:** o diretório deve existir *e* conter
  `index.html`, ou o `ai-memory serve` encerra com um erro claro antes de
  vincular à porta. Requer que `--enable-web` também seja definido.
- **Injeção de caminho base:** o ai-memory injeta `<base href="...">` e
  `<meta name="ai-memory-base-path" content="...">` na casca da SPA. Isso
  cobre caminhos diretos `/web`, `/web/index.html` e caminhos de fallback do roteador do cliente;
  assets estáticos são servidos inalterados.

Quando um proxy reverso mantém o ai-memory sob um subcaminho de URL, defina
`--base-path` (ou `AI_MEMORY_BASE_PATH`) para que todas as superfícies HTTP se movam juntas:

```bash
ai-memory serve \
    --transport http \
    --bind 127.0.0.1:49374 \
    --enable-web \
    --base-path /wiki
```

Com `--base-path /wiki`, a API fica em `/wiki/api/v1`, MCP em
`/wiki/mcp`, hooks em `/wiki/hook`, rotas de administração em `/wiki/admin/*` e a
interface web padrão em `/wiki/web`. Defina `--web-slug /` para montar a interface web ou SPA
customizada na raiz base (`/wiki`) em vez de `/wiki/web`.

**Regras de segurança do caminho base.** Tanto `--base-path` quanto `--web-slug` passam
pelo mesmo normalizador. Os segmentos devem ser caracteres não reservados da RFC 3986
(`[A-Za-z0-9-._~]`). Três coisas colapsam o prefixo para
`""` (montagem na raiz) com um `WARN` de inicialização para que você possa ver o
downgrade no log:

- Segmentos `.` ou `..`. Seus caracteres são não reservados por si sós,
  mas no limite do segmento eles significam "atual" e "pai", de modo que um
  único erro de digitação transformaria seu prefixo em um vetor de traversal.
- Qualquer caractere fora do conjunto não reservado (espaços, `<`, `"`, etc.).
- Entrada vazia / apenas com espaços em branco.

O redirecionamento de barra final em `{base_path}{web_slug}/` →
`{base_path}{web_slug}` mantém a query string. Os fragmentos ocorrem no
lado do cliente e nunca chegam ao servidor.

Quando `--web-ui-dir` estiver **ausente**, o navegador integrado `/web` no
lado do servidor é o padrão (renderização HTML somente leitura, busca,
árvore do projeto), inalterado em relação a antes.

## 7. Exemplo prático: fetch mínimo em SPA

```js
// Resolve bases from the SPA shell injected by ai-memory. The meta tag is
// empty at host root and e.g. "/wiki" behind a subpath reverse proxy.
const basePath = document
  .querySelector('meta[name="ai-memory-base-path"]')
  ?.getAttribute("content") ?? "";
const origin = `${location.origin}${basePath}`;
const API = `${origin}/api/v1`;

async function login(username, password) {
  const resp = await fetch(`${origin}/auth/login`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ username, password }),
  });
  if (!resp.ok) throw new Error(`login failed: ${resp.status}`);
  return resp.json();
}

function cookie(name) {
  const prefix = `${encodeURIComponent(name)}=`;
  return document.cookie
    .split(";")
    .map(value => value.trim())
    .find(value => value.startsWith(prefix))
    ?.slice(prefix.length);
}

async function apiGet(path, params) {
  const url = new URL(`${API}${path}`, location.origin);
  if (params) Object.entries(params).forEach(([key, value]) =>
    value != null && url.searchParams.set(key, value));
  const resp = await fetch(url, {
    credentials: "include",
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    const { error } = await resp.json().catch(() => ({ error: resp.statusText }));
    throw new Error(`${resp.status}: ${error}`);
  }
  return resp.json();
}

// Call login() from the UI first. The HttpOnly session is never visible here.
const overview = await apiGet("/workspaces/default/projects/ai-memory/overview", {
  limit: 10,
});
console.log(overview.briefing.counts.pages_latest, "pages");

// POST is CSRF-protected even when the operation is read-only.
const search = await fetch(`${API}/search`, {
  method: "POST",
  credentials: "include",
  headers: {
    "Content-Type": "application/json",
    "X-CSRF-Token": decodeURIComponent(cookie("ai_memory_csrf") ?? ""),
  },
  body: JSON.stringify({
    q: "karpathy",
    scopes: [
      { workspace: "default", project: "ai-memory" },
      { workspace: "default", project: "shared-notes" },
    ],
    limit: 20,
  }),
}).then(response => response.json());
```

Teste de fumaça com `curl`:

```bash
# Use the bearer already configured on the running server.
TOKEN='<server-configured bearer token>'
curl -fsS "http://127.0.0.1:49374/api/v1/workspaces" -H "Authorization: Bearer $TOKEN" | jq
```

## 8. Onde olhar no código-fonte (a especificação canônica)

Se uma especificação de documentação/formato de resposta algum dia entrar em conflito com o código, o código prevalece.
Leia estes:

| | Local |
|---|---|
| Registro de rotas + corpos dos handlers | `crates/ai-memory-web/src/routes/api.rs` |
| Structs de resposta (`PageHit`, `WorkspaceSummary`, `BriefingSnapshot`, `HealthPage`, `SessionSummary`, `ObservationRecord`, …) | `crates/ai-memory-store/src/reader.rs` |
| Listagem de sessões + leitores de observações por sessão (`sessions_for_scope`, `session_summary_scoped`, `session_observations_scoped`) | `crates/ai-memory-store/src/reader.rs` |
| Leitores do histórico de páginas (`page_versions`, `page_version_body`) e o diff limitado | `crates/ai-memory-store/src/reader.rs`, `crates/ai-memory-web/src/diff.rs` |
| Ranqueador de busca com escopo compartilhado com `memory_query` (`hybrid_search_scopes`) | `crates/ai-memory-web/src/recall.rs`, `crates/ai-memory-store/src/reader.rs` |
| Verificações do painel de saúde e linhas de correção | `crates/ai-memory-web/src/health.rs`, `crates/ai-memory-core/src/fix_lines.rs` |
| Testes de integração das rotas de API e web (autenticação, 400s, 404s, correção multi-escopo, fallback de SPA) | `crates/ai-memory-web/tests/suite/routes.rs` |
| Autenticação + camadas de middleware | `crates/ai-memory-cli/src/commands/serve.rs` (`mount_web_router`, `apply_http_layers`) |
| Validação do diretório de UI customizada | `crates/ai-memory-cli/src/commands/serve.rs` (`validate_web_ui_args`) |

## 9. CORS

`/api/v1` aceita requisições cross-origin quando o operador configura
a lista de permissões (allow-list). A camada CORS é delimitada apenas a esse roteador; `/mcp`,
`/hook`, `/admin/*` e `/web` permanecem de mesma origem (same-origin).

Configure via `--cors-allow-origin <origin>` (repetível) no
subcomando `serve` ou `AI_MEMORY_CORS_ALLOW_ORIGINS=<csv>` no
ambiente. A lista é validada na inicialização:

- Cada entrada deve ser uma URL `scheme://host[:port]` totalmente qualificada.
- Sem barra final, sem caminho, sem query, sem curinga (`*`).
- Mistura de `http://` + `https://` é aceita; escolha o que sua SPA serve.

Origens inválidas falham na inicialização com um erro claro em vez de aceitar
curinga silenciosamente. A camada permite `GET / POST / OPTIONS`,
cabeçalhos `Authorization` + `Content-Type`, e credenciais, com um
cache de preflight de 10 minutos.

## 10. Lacunas conhecidas e não-objetivos deliberados

- **Sem superfície de gravação, por design.** Esta não é uma iteração pendente: navegadores
  não podem mutar e não vão. A wiki é um registro do que um projeto
  produziu, de autoria de sumarização automatizada sobre observações
  capturadas; recuperação, proveniência e a trilha de auditoria repousam sobre
  ninguém ter voltado e ajustado o conteúdo. Editar uma página manualmente impede que ela
  responda a "o que este projeto produziu" e passa a responder
  "o que alguém queria que ela dissesse", sem nenhuma forma de distinguir os dois casos
  posteriormente.

  Entrada humana ainda é permitida. `memory_write_page` existe para
  anotações humanas duráveis e as coloca na mesma linhagem que
  tudo o mais (páginas são substituídas na alteração do corpo). A linha divisória está entre
  adicionar ao registro pelo caminho normal e editá-lo pelo
  lado de fora. Uma wiki editável por humanos é algo razoável de se querer e um
  produto separado; veja #482. A visualização de histórico mostra o que mudou entre
  versões; ela não tem botão de restauração, pelo mesmo motivo.
- **Rate limiting** é compartilhado com `/mcp` + `/admin` (apenas o limite
  do corpo é aplicado hoje). Um limitador global futuro restringiria o
  caso de mau comportamento autenticado.

Para atualizações de status sobre qualquer um desses pontos, o issue tracker é a fonte da
verdade.

### Exceção da caixa de entrada do navegador

A superfície `/api/v1` e a edição da wiki permanecem somente leitura. A interface web integrada adiciona
`/web/inbox` e `POST /web/messages` (sob o caminho base e web
slug configurados). O POST enfileira uma mensagem higienizada e admitida para um projeto e
harness existentes. Requer acesso de gravação à origem e ao destino, um corpo JSON,
o cabeçalho da interface e as verificações normais de CSRF de sessão do navegador. Não realiza mutação em
páginas da wiki, não retira mensagens da fila nem executa um agente. Veja
[agent messaging](agent-messaging.md#na-interface-web).

Três rotas POST adicionais oferecem suporte à edição de mensagens enfileiradas: `/web/messages/{id}/edit` a retira
da fila e retorna seu texto, `/web/messages/{id}/save` substitui o
assunto e o texto e a reenfileira, e `/web/messages/{id}/release` a reenfileira
inalterada. O corpo indica a caixa de correio (`workspace`, `project`), e `save` adiciona
`subject` e `body`. Elas precisam das mesmas verificações que o envio, mais acesso de gravação ao
projeto de onde a mensagem veio. Uma mensagem fora do estado esperado
retorna 409.

## Documentação embutida para agentes

O catálogo de documentação está em `GET /api/v1/docs?q=token`. Cada entrada tem um
`id`, título, descrição, categoria e caminho de `source` do repositório.
`GET /api/v1/docs/{id}` retorna esses metadados mais `markdown`, a
`version` do pacote do binário, uma `release` selecionada e as `releases` disponíveis. Estes são
guias de produto somente leitura, independentes de escopos de projeto e do conteúdo do banco de dados.
Eles usam a mesma autenticação que as outras rotas da API. A busca é limitada a
200 caracteres, e ids de documento e releases desconhecidos retornam 404.

O navegador usa `/web/docs` e `/web/docs/{id}`. Um sufixo `.md` retorna
`text/markdown` em vez de HTML. O changelog tem como padrão Unreleased, e
séries arquivadas têm como padrão sua primeira release. Use `?release=3.2.2` para uma
release ou `?release=all` para uma série inteira. Os guias e arquivos de changelog são
compilados no binário, incluindo imagens Docker construídas a partir do código-fonte, de modo que nenhuma conexão com
a internet ou LLM em execução é necessária. Links do repositório fora do catálogo
curado abrem no GitHub. Essas páginas descrevem capacidades entregues e não
expõem a configuração ou credenciais do servidor em execução. Aplique seu caminho
base e web slug configurados a estes exemplos.
