# Segurança

> O modelo de segurança completo. Movido da página inicial do README; o README mantém um resumo.

Uma nova instalação requer um bearer token. O `ai-memory init` em um diretório
de dados novo grava um bearer de root aleatório em `<data_dir>/auth-tokens/default` (somente
o proprietário) e registra `[auth].bearer_token_file`; o servidor então recusa qualquer
rota exceto `/healthz` sem ele. Os hooks, a CLI e a
ponte stdio `ai-memory mcp-bridge` o leem do diretório de dados, de modo que o token
nunca é colado na configuração de um harness nem passado na linha de comando. Claude Code
e Meta Muse Code executam a ponte com reconhecimento de sessão, Codex, Prime Agent e Factory Droid referenciam
o token como `$AI_MEMORY_AUTH_TOKEN`, e todos os outros clientes MCP compatíveis com `install-mcp`
executam `ai-memory mcp-bridge --no-session-id` como um servidor stdio local. ZCode,
que aceita apenas entradas HTTP, é a exceção e precisa de uma entrada manual.

Uma instalação criada antes desse padrão mantém seu comportamento exclusivo para loopback
(`127.0.0.1:49374`), sem autenticação. Isso permanece protegido contra a rede, mas
qualquer processo local e página de navegador pode acessar o servidor. `ai-memory status`
e `ai-memory doctor` mostram `auth: none (loopback)` com a correção,
`ai-memory auth enable`, que gera o token, o registra na configuração, fornece
o token aos hooks e recadastra cada cliente MCP que aponta para este
servidor. Reinicie
o servidor depois.

O HTTP não autenticado fora de loopback agora falha de forma fechada. Defina
`AI_MEMORY_AUTH_TOKEN` ou vincule ao loopback; `--allow-insecure-no-auth` é uma
exceção intencional e perigosa apenas para HTTP puro. A autenticação não
criptografa bearer tokens: para acesso via LAN ou remoto, use os modelos prontos do
[Caddy](../../docker/compose.tls.caddy.yml) ou do
[Cloudflare Tunnel](../../docker/compose.tls.cloudflared.yml) descritos
no [guia de proxy reverso HTTPS](https-via-proxy.md).

Ative a autenticação por bearer quando o servidor estiver exposto além do loopback, quando
processos locais não confiáveis compartilharem a máquina ou quando o diretório de dados contiver
histórico confidencial de projetos:

```bash
TOKEN=$(ai-memory generate-auth-token)

docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 0.0.0.0:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
    akitaonrails/ai-memory:latest

ai-memory install-mcp   --client claude-code --apply \
    --server-url "http://<server-ip>:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "http://<server-ip>:49374" --auth-token "$TOKEN"
```

Quando os usuários não puderem ver os projetos uns dos outros de forma alguma, execute servidores separados.
Uma única máquina ainda pode realizar a captura para vários deles: registre cada servidor como
um perfil nomeado com `ai-memory server add` e deixe o `.ai-memory.toml` de cada repositório
selecionar um com `server = "<name>"`. O marcador nunca contém
uma URL ou token, um perfil pode ser restrito aos diretórios que atende, e
uma seleção que não puder ser resolvida descarta o evento em vez de enviá-lo para
outro servidor. Consulte
[`marker-file.md`](marker-file.md#roteamento-de-captura-para-outro-servidor-server).

A autenticação por bearer protege `/mcp`, `/hook`, `/handoff`, `/workstream/*`, e
chamadas de máquina para `/admin/*` e `/api/v1/*`. Humanos entram em
`POST /auth/login`; o console usa um cookie de sessão `HttpOnly` com CSRF,
não um Bearer no `localStorage`. HTML SPA personalizado em `/web` é estático e público.
Quando a autenticação humana escuta além do loopback,
`AI_MEMORY_AUTH__SECURE_COOKIE=true` é obrigatório e sinaliza que um proxy
reverso HTTPS confiável é responsável pela borda voltada ao navegador. Isso torna o cookie de sessão
exclusivo para HTTPS. Feche ou redirecione o acesso HTTP direto a esse hostname.
Vínculos fora do loopback também devem definir `AI_MEMORY_ALLOWED_HOSTS` para proteção contra
DNS rebinding.

Para uma LAN privada onde o operador aceita deliberadamente transporte em texto simples,
`serve --allow-insecure-human-auth` (variável de ambiente
`AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`) permite um listener fora de loopback
com um cookie de sessão humana não seguro (Non-Secure). Esta exceção vem desativada por padrão
e exibe um aviso incondicional na inicialização. Ela não enfraquece verificações de senha,
autorização de sessão ou de chave de API, CSRF, limites de taxa, a lista de permissões de Host,
ou a proteção separada de exposição sem autenticação. Ela também não fornece criptografia:
qualquer par capaz de observar ou alterar o tráfego da LAN pode roubar senhas, credenciais
Bearer/API, cookies de sessão e dados de memória. Prefira o caminho com proxy HTTPS.
A opção não altera nenhum estado salvo de usuário, senha, sessão ou chave de API; removê-la
restaura a recusa na inicialização.

Para a imagem oficial do Docker, a forma via variável de ambiente evita substituir seu
comando `serve` padrão:

```bash
docker run -d --name ai-memory \
    -p 192.168.1.100:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true \
    -e AI_MEMORY_ALLOWED_HOSTS="192.168.1.100,localhost,127.0.0.1" \
    akitaonrails/ai-memory:latest
```

Sem nenhum token (o padrão de loopback), uma página web aberta no seu navegador
ainda pode acessar `127.0.0.1:49374`. O servidor recusa essas requisições nas
rotas de máquina e de administração: uma requisição que o navegador marca como cross-site
(`Sec-Fetch-Site`), ou uma cujo `Origin` não seja o próprio host do servidor, recebe um
403. O mesmo vale para qualquer requisição de navegador que não seja uma chamada `fetch()` de script
(`Sec-Fetch-Dest` diferente de `empty`): um link, imagem ou navegação é recusado
mesmo quando parte das próprias páginas do servidor ou de outra aplicação.
Hooks, a CLI e clientes MCP nunca enviam esses cabeçalhos, e as próprias chamadas de
administração da interface web usam `fetch()`.

Navegadores enviam esses metadados de fetch sobre HTTP puro apenas para `localhost`,
`*.localhost`, `127.0.0.0/8` e `[::1]`. Quando um servidor não autenticado é
acessado por qualquer outro nome na lista de permissões de `Host` (um endereço ou nome de LAN
com `--allow-insecure-no-auth`, ou `host.docker.internal` na lista padrão da
imagem Docker), uma requisição de imagem não carrega metadados de fetch nem
`Origin`. Nesses casos, a busca de handoff no início da sessão, o único `GET` que altera estado,
também deve conter o cabeçalho `X-AI-Memory-Client` que todo hook do ai-memory,
comando da CLI e integração gerada envia; uma página web não pode adicioná-lo sem
um preflight CORS, que essas rotas nunca respondem. Hooks configurados antes da existência
deste cabeçalho recebem 403 em tal servidor até que você execute novamente
`ai-memory install-hooks` (ou `ai-memory upgrade`); em um host loopback eles
continuam funcionando sem alterações. Uma requisição que contenha um cabeçalho de encaminhamento de proxy
(`Forwarded`, `X-Forwarded-For`, `X-Forwarded-Host`, `X-Forwarded-Proto`,
`X-Forwarded-Server`, `X-Forwarded-Port`, `X-Original-Forwarded-For`,
`X-Real-IP`, `X-Client-IP`, `CF-Connecting-IP`, `True-Client-IP` ou `Via`)
conta como fora de loopback aqui mesmo quando seu `Host` for um endereço de loopback,
porque um proxy reverso HTTP puro pode reescrever o `Host` para o endereço para o qual
ele encaminha enquanto o navegador por trás dele viu uma origem fora de loopback. Os próprios
clientes do ai-memory conectam-se diretamente e não enviam nenhum desses cabeçalhos. O servidor não pode
enxergar um proxy que nada informa, no entanto: o `proxy_pass` padrão do nginx reescreve
o `Host` para o endereço upstream e não adiciona nenhum cabeçalho `Forwarded`, `X-Forwarded-*` ou
`Via`, fazendo com que suas requisições pareçam as de um cliente local. Um proxy HTTP puro à
frente de um servidor não autenticado deve, portanto, adicionar `X-Forwarded-For`
(`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`) ou manter o
host do navegador (`proxy_set_header Host $host;`, o que também exige esse nome na
lista de permissões de `Host`). Melhor ainda, termine o TLS no proxy ou defina um bearer
token. Um navegador que não envia metadados de fetch nem mesmo para o loopback
(Safari anterior ao 16.4) passa apenas pela checagem de `Origin` lá, o que uma imagem ou link
não dispara; defina um bearer token se você o utilizar. A lista de permissões
de `Host` recusa hostnames de DNS-rebinding, e as rotas exclusivas para JSON recusam os
tipos de conteúdo que um navegador pode enviar sem um preflight CORS. As rotas públicas
de login (`POST /auth/login`, `POST /auth/recovery`) aceitam apenas
corpos `application/json` também, de modo que uma página em outro site não pode autenticar seu
navegador nem tentar senhas. Imagens em páginas de wiki
renderizadas pela interface web podem usar apenas caminhos relativos que fiquem dentro da interface, de modo que
visualizar uma página salva não pode fazer seu navegador chamar essas rotas.

Servidores de hooks compartilhados e ocupados também podem definir `AI_MEMORY_HOOK_RATE_PER_SEC` (tokens por
segundo por origem de ator/sessão) e opcionalmente `AI_MEMORY_HOOK_RATE_BURST` para
limitar uma sessão descontrolada sem bloquear origens de hooks não relacionadas. A taxa não definida ou `0`
mantém o limitador desativado.

Para impedir que o servidor armazene determinadas chamadas de ferramentas independentemente do que o cliente
enviar, liste-as em `[ingest]`: `drop_tool_use_id_prefixes` (por exemplo
`["toolu_plugin_"]`, o prefixo de ID que o Claude Code atribui a chamadas de ferramentas de plugins) e
`drop_tool_names` (nomes exatos). O ID e o nome são lidos dos campos próprios de cada
harness, como o `callID` do OpenCode. Um evento PreToolUse ou
PostToolUse correspondente é reconhecido e descartado antes de ser sanitizado,
ter a taxa limitada ou ser armazenado. As listas se aplicam a todos os projetos no servidor.

Para servidores compartilhados onde cada desenvolvedor deve autenticar suas próprias escritas
de hook, hooks nativos do Claude Code podem usar um token de dispositivo OIDC armazenado em vez de
incorporar um token estático compartilhado:

```bash
ai-memory auth login oidc-device \
    --issuer "https://issuer.example.com/realms/team" \
    --client-id "ai-memory-cli"

ai-memory install-hooks --agent claude-code --apply \
    --server-url "http://<server-ip>:49374"
```

A autenticação de hook via OIDC exige o caminho de comando nativo `ai-memory hook ...`. O wrapper
Docker mantém hooks em script shell por padrão; configure o OIDC a partir de um binário
nativo de release ou de instalação pelo código-fonte. Comandos HTTP de cliente leve como `ai-memory status`
e `ai-memory search` também usam o access token OIDC armazenado quando nenhum
`AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` estático estiver configurado; o bearer estático
ainda prevalece quando presente. Isso se destina a gateways/pontes compatíveis com OIDC; a autenticação nativa
do servidor ai-memory ainda aceita bearer de root estático / tokens de usuário do banco de dados, e
`/admin/*` permanece exclusivo para root a menos que um gateway converta a autenticação OIDC aceita em
autenticação upstream que o ai-memory aceite.

IDs de sessão OIDC/Keycloak são sessões do provedor de login, não sessões de agente do
ai-memory. Servidores compartilhados que dependem do isolamento de sessão de `[auto_scope]` ainda
precisam de `workspace` + `project` / `scopes` explícitos, ou de uma ponte que encaminhe o
ID de sessão real do hook de ciclo de vida nas requisições MCP.

**HTTPS.** O ai-memory deliberadamente não faz a terminação TLS por conta própria; coloque um
proxy reverso bem testado à frente dele.
[`docs/https-via-proxy.md`](https-via-proxy.md) é o guia
de implantação, com modelos prontos para copiar e colar para docker compose em
[`docker/compose.tls.caddy.yml`](../../docker/compose.tls.caddy.yml) (Caddy
com Let's Encrypt ou CA interna) e
[`docker/compose.tls.cloudflared.yml`](../../docker/compose.tls.cloudflared.yml)
(Cloudflare Tunnel, sem portas abertas). Ambos são recomendados a partir do momento em que você
ativa múltiplos usuários ou vincula além do loopback. O padrão do Quick Start
de usuário único em loopback não precisa de TLS; o guia diz isso
explicitamente para que você não adicione configurações desnecessárias.

**Atribuição multiusuário (v0.8, opcional) mais login humano.** Quando mais
de uma pessoa compartilha um servidor, o ai-memory atribui cada escrita a um
usuário nomeado. Humanos entram com nome de usuário/senha; agentes e CLIs usam
`Authorization: Bearer` (`AI_MEMORY_AUTH_TOKEN` para automação root, ou
uma chave `aim_` de `ai-memory api-key add`). Os dados permanecem single-tenant:
não há RBAC por página. Um
`[auth].token_pepper` é necessário para autenticação de usuário do banco de dados, mas criar a
primeira linha de usuário é o que imediatamente torna todo endpoint `/admin/*`
exclusivo para root, incluindo rotas de status/busca/leitura de página e gerenciamento de usuários.
O `ai-memory init` gera um pepper para novas instalações sem alterar
o comportamento de usuário único até que um usuário seja adicionado. Um gateway SSO pode usar, em vez disso,
um `[auth].actor_proxy_bearer_token` dedicado e cabeçalhos `X-Memory-Actor-*` confiáveis;
sua credencial é deliberadamente separada do bearer de root para que uma identidade
ausente não possa se tornar root. Consulte
[`docs/users.md`](users.md) para o passo a passo completo e a
escada de autenticação de quatro degraus.

Consulte [`docs/deploy.md`](deploy.md) para o padrão completo de homelab
com autenticação por bearer, lista de permissões de hosts e opções de TLS/proxy reverso.
