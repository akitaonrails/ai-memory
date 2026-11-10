# Arquivo marcador: `.ai-memory.toml`

Declare a qual espaço de trabalho (e opcionalmente a qual projeto) o
`cwd` de um agente pertence, sem depender do nome base do diretório.

## Por que

O ai-memory isola o namespace de cada página wiki por `(workspace, project)`. Por
padrão, `workspace = "default"`; um remoto `upstream`/`origin` válido fornece
o nome de projeto canônico baseado no caminho do repositório sem host, e apenas um checkout
sem remoto usa `project = basename($cwd)`. Isso funciona para um desenvolvedor individual em
`~/projects/<repo>`, mas não atende aos
casos para os quais este arquivo marcador foi criado:

- **Consultorias com múltiplos clientes** com `~/projects/<client>/<repo>`:
  cada cliente deve ficar em um espaço de trabalho dedicado, não no "default".
- **Separação entre trabalho / pessoal / código aberto** para desenvolvedores solo
  que desejam isolamento por contexto de vida.
- **Monorepositórios** onde você gostaria de todos os pacotes sob um único projeto
  (em vez de divisões pelo nome base de cada pacote), ou cada pacote
  sob seu próprio projeto, à sua escolha.

O arquivo marcador permite declarar esses mapeamentos sem criar um fork do
ai-memory nem executar comandos de CLI por diretório.

Clientes MCP estáticos também usam o marcador como a fonte do repositório para
argumentos explícitos de escopo. Para uso concorrente seguro, declare tanto `workspace` quanto
`project`: o roteamento gerenciado instrui clientes estáticos a passar esse par em cada
chamada de ferramenta com escopo de projeto porque eles não podem anexar o id real da sessão
do hook de ciclo de vida. Sem uma sobreposição por marcador, clientes estáticos derivam o projeto do `upstream`
normalizado, depois de `origin`, unindo o caminho completo sem host com o mapeamento
de nome seguro padrão; apenas checkouts sem remoto válido usam o nome base de sua pasta.
Pontes com reconhecimento de sessão mantêm o roteamento automático para o projeto atual. O comando sem sessão
`ai-memory mcp-bridge --no-session-id` aplica essa derivação por conta própria, a partir de seu
diretório de inicialização, para uma chamada que não especifique escopo
([`auto-scope.md`](auto-scope.md#the-session-less-bridges-launch-scope)).

## Onde colocá-lo

`.ai-memory.toml` em **qualquer ancestral permitido** do seu `cwd`. Hooks de ciclo de vida
percorrem para cima a partir do `cwd` em direção a `$HOME` (ou `/` se `$HOME` não estiver definido) e usam o
**primeiro** marcador encontrado. Quando o cwd está fora de `$HOME`, a busca para na
raiz do checkout mais próxima (arquivo ou diretório `.git`); fora de um checkout, apenas o próprio cwd
é verificado. Marcadores mais próximos sobrepõem os mais externos. Quando um marcador é encontrado,
scripts de hook também encaminham o `cwd` atual e a identidade remota normalizada para que
marcadores contendo apenas o espaço de trabalho resolvam o projeto canônico pelo caminho do repositório, com
`basename(cwd)` retido apenas quando nenhum remoto válido existir.

Um marcador cujo único conteúdo seja uma seção `[capture]` (veja
[Exclusões de captura](#exclusões-de-captura) abaixo) é **transparente** para essa
busca: `workspace`, `project`, `project_strategy` e as outras configurações
encaminhadas (`drop_subagent_captures`, `[recall] default_global`, `[briefing]`,
chaves de `[profile]`, `[handoff]` e `[recovery]`) são resolvidas a partir do marcador ancestral
mais próximo que declare ao menos uma delas, ignorando qualquer marcador
mais próximo que declare apenas `[capture]`. Um marcador de subdiretório adicionado apenas
para excluir alguns caminhos da captura, portanto, não redefine mais o escopo para
`default` / nome base para aquela subárvore. A própria seção `[capture]`/`ignore_paths` não
é afetada por isso e sempre provém do marcador mais próximo, mesmo que seja
apenas de captura.

O caminho do marcador é compartilhado pelos scripts de hook POSIX/PowerShell e pelas
integrações geradas em TypeScript do OpenCode / OMP / Pi / OpenClaw. Em todos os casos,
a captura de hook e a busca de handoff enviam os mesmos parâmetros de consulta `cwd`, `workspace`, `project`,
`project_strategy`, `drop_subagent`, `default_global`, `briefing`,
`briefing_budget`, `profile_contribute`, `profile_consume`, `handoff` e
`recovery` para o servidor conforme aplicável;
a busca de handoff também envia `cwd` quando nenhum marcador existe, de modo que a rota padrão
`project = basename(cwd)` funcione de forma consistente. Todo cliente também envia
`identity` / `identity_src` quando o checkout possui uma identidade de repositório (veja
[Identidade do repositório](#identidade-do-repositório)), com ou sem um marcador, e
um `identity_style=path|host_path` explícito junto a cada identidade remota.
Clientes atuais enviam `path` quando nenhuma rota de marcador/home a sobrepõe; a omissão
no lado do servidor permanece no padrão legado `host_path` para compatibilidade com clientes antigos.
Um `project` de marcador permanece o nome canônico solicitado. Seus `aliases` validados
viajam como um único valor de consulta `aliases` de array JSON codificado em porcentagem tanto em requisições de hook
quanto de handoff, e são ignorados a menos que o checkout forneça uma identidade de
remoto git normalizada com host.

## Criar um marcador interativamente

Execute isso de qualquer lugar dentro de um checkout para criar um marcador na raiz
do worktree atual:

```bash
ai-memory project init
```

O comando solicita `workspace` e `project`. Cada padrão é o escopo que
a CLI e os hooks já resolvem para o checkout: o valor de um marcador herdado,
caso contrário o projeto derivado do remoto normalizado `upstream`, depois `origin`
(`github.com/acme/api` → `default` / `acme-api`), caso contrário o nome do diretório raiz.
Aceitar os padrões, portanto, fixa o escopo onde sua memória já reside.
Fora do Git, o marcador é gravado no diretório atual.

Um nome não pode conter `/`, `\`, espaços em branco ou caracteres de controle. O
comando necessita de um terminal interativo, nunca sobrescreve um marcador existente,
e grava apenas `workspace` e `project`, de modo que todas as outras chaves mantêm seus
valores padrão até que você as adicione. Em um repositório, comite o arquivo para que cada checkout
e worktree seja roteado da mesma forma.

## Schema

```toml
# Required.
workspace = "movvia"

# Optional. When present, forces project = "pe-portais" for every
# cwd inside this marker's tree. Omit it to use the canonical remote path,
# falling back to basename(cwd) when no valid remote exists.
project = "pe-portais"

# Optional. Set it to "repo-root" to derive a remote-less project's name
# from the main git repository root, so
# linked worktrees and subdirectories share one project. Ignored when
# `project` is present.
project_strategy = "repo-root"

# Optional. Pin this checkout's repository identity: the key its captures
# route by, whatever the folder is called and wherever it is checked out.
# Outranks `project` and the git remote. Use it for a directory with no
# remote that must not collide with every other folder of the same name,
# for two checkouts that should share one memory, or for a monorepo
# subdirectory that deserves its own. Case-folded. See "Repository
# identity" below.
identity = "acme/platform"

# Optional. The default "path" names a remote-backed project from its whole
# repository path without the host ("acme-api" for github.com/acme/api).
# Set "host_path" to preserve the pre-v3 folder/split naming behavior.
# Existing identity-backed projects can be promoted in place only by an
# authorized capture/write. See "Naming projects by repository path" below.
identity_style = "host_path"

# Optional. Former names for this same repository in this workspace. At most
# 16 entries, each at most 128 UTF-8 bytes and matching
# ^[A-Za-z0-9][A-Za-z0-9._-]*$. Whitespace is trimmed and duplicates keep their
# first position. A hostful upstream/origin identity must match the stored
# project's identity; otherwise the aliases are refused. Reads use the
# existing UUID without renaming. Hook capture may rename that UUID to the
# canonical `project` above only after write authorization. Aliases are local
# routing hints, are not persisted, never select another workspace, and remain
# supported after the v3 dual-key compatibility window.
aliases = ["former-name", "old-worktree-name"]

# Optional. Opt this project into drop_subagent_captures: set it to "true"
# and the server accepts but does NOT store this project's subagent-session
# captures. A multi-agent harness fans one goal out to many subagent
# sessions whose per-event captures can flood a small instance; scoping the
# opt-in here keeps the drop from affecting other projects on the same
# server. Off by default (absent / "false").
drop_subagent_captures = "true"

# Optional. Broaden this repo's DEFAULT memory recall to every project:
# an unscoped `memory_query` from sessions in this tree behaves as
# `global=true`, and an unscoped `memory_recent` returns the most recent
# pages across every project (each hit annotated with workspace + project).
# Meant for meta-repos that constantly need sibling-project context.
# Explicit args always win — passing `workspace`/`project`/`scopes`/
# `global` overrides this for that call. Off by default. Note: while
# active, unscoped queries return cross-project `global_hits` instead
# of project `hits` + `global_scope_hits` (the `_global` preference
# pages still appear, annotated, among the global results).
[recall]
default_global = "true"

# Optional. Inject a compiled project brief at session start (and after a
# context clear — Claude Code re-fires SessionStart on /clear): the
# session-start handoff fetch also returns this project's pinned /
# `_rules/` / `_slots/` wiki pages plus recently-updated page titles, so the
# agent starts with the architecture context instead of re-exploring the
# codebase. Appended AFTER any pending handoff, and
# unlike the handoff it is not consumed — it is recomposed every opted-in
# session start. Only agents whose session-start hook injects stdout as
# context benefit (Claude Code, Codex, OpenCode, …). Off by default: the
# brief costs tokens on EVERY session start, so opt in per repo.
# Kimi Code note: kimi discards SessionStart hook stdout, so there the
# brief is delivered on the FIRST user prompt of the session instead
# (once per session, same as Claude). Its local delivery markers are created
# only for opted-in repositories and bounded to the 512 newest sessions;
# re-briefing after /clear is not supported in v1.
[briefing]
inject_on_session_start = "true"

# Optional. Char budget for the WHOLE brief (~4 chars per token), headers
# and footers included. Every core page first gets a short excerpt (its
# frontmatter `abstract:` or `summary:`, else the prose it opens with); what is
# left deepens pages to longer excerpts and then whole bodies, pinned and
# `_slots/` pages first, then the cheapest bodies. A body is shown whole or
# not at all, and a body shorter than its excerpt is always shown whole.
# Four or more unpinned excerpts from one folder (`_rules/`, `gotchas/`,
# `decisions/`, `procedures/`) share a heading. Pages the budget cannot
# cover are listed by path so the agent can `memory_read_page` them,
# degrading to a count when even the paths do not fit. Pin a page to give
# it depth first. The security notice and the untrusted-history markers
# are never traded for content. Clamped server-side to [1500, 20000];
# defaults to 4000.
max_chars = 4000

# Optional. Hand the same brief to each `claude-code` subagent when it starts
# (`SubagentStart`), so a delegated task begins with the project's rules and
# slots too. Independent of `inject_on_session_start`; `max_chars` bounds it,
# and that agent's 10,000-character hook context limit caps it. It comes
# from the read-only `GET /brief`, which never reads or claims a handoff, so a
# subagent cannot take its parent session's baton; a server without that
# route answers 404 and the subagent starts without a brief. Off by default:
# every subagent pays for the brief in tokens, so opt in per repo.
inject_on_subagent_start = "true"

# Optional. Stop delivering this project's handoff at session start. Only an
# explicit false / "no" / "off" / "0" turns it off; the default is to deliver.
# Handoffs are still created when a session ends, and stay open (an agent can
# still take one with `memory_handoff_accept`), so only the automatic delivery
# stops. The brief, the profile digest and everything else on start are
# unchanged.
[handoff]
deliver_on_session_start = false

# Optional. Stop delivering the unfinished-session recovery snapshot at
# session start. Only an explicit false / "no" / "off" / "0" turns it off;
# the default is to deliver. This also suppresses the snapshot's backfill
# marker. The source session remains open and its lifecycle is unchanged.
# Independent of [handoff]: handoffs, managed context, the brief, the profile
# digest and the inbox notice are unchanged.
[recovery]
deliver_on_session_start = false

# Optional. Show Claude Code the count-only pending-inbox notice on every
# user prompt while the count or the inbox's names change, instead of only
# at session start. The notice never carries message text; the agent still
# pops and reviews the message itself. A marker that sets only [messaging]
# is a settings boundary. Off by default.
[messaging]
inbox_notice_on_prompt = "true"

# Optional. The cross-project profile (docs/cross-project-profile.md): your
# usual choices from other projects, delivered at session start as defaults
# below this repository's rules file. Both keys default to on.
# `contribute = false` keeps this project out of the profile (client or NDA
# work); `consume = false` keeps the profile digest out of its session starts
# and the profile out of its queries. Forwarded by every hook client at
# session start and recorded on the project, so removing a key turns the
# setting back on. Quoted or bare; only an explicit false / "no" / "off" /
# "0" turns a key off.
[profile]
contribute = false
consume = false
```

A **nomenclatura** para `workspace` e `project`: o servidor armazena ambos os nomes
como fornecidos e não valida seus caracteres. Limite-se a letras ASCII minúsculas,
dígitos, pontos, traços e sublinhados (`^[a-z0-9][a-z0-9._-]*$`)
de qualquer forma: nomes assim dispensam aspas em URLs, shells ou argumentos de ferramentas.
Clientes de hook codificam em URL o que quer que um marcador de repositório declare. O
mapa de rotas da home do operador abaixo é mais estrito: um `route_workspace` ou
`route_project` fora desse padrão invalida o mapa inteiro. Nomes nunca
se tornam caminhos de sistema de arquivos, porque a wiki armazena cada projeto sob
`<data_dir>/wiki/<workspace_id>/<project_id>/`. Um nome como `../x` é
um rótulo incomum, não um caminho.

`aliases` funciona em regime tudo-ou-nada e intencionalmente usa um subconjunto portátil compartilhado por
Rust nativo, shell POSIX, PowerShell e TypeScript gerado: exatamente uma
declaração em nível de raiz em uma linha, contendo um array compatível com JSON de
strings sem escape. Declarações duplicadas, posicionamento em tabelas, arrays de várias linhas,
comentários/lixo no final, vírgulas residuais, valores com caracteres de controle/semelhantes a caminhos,
não strings, valores superdimensionados e listas com contagem excessiva fazem o roteamento
de hook e de handoff falharem de forma fechada (fail closed) sem ecoar seus conteúdos. Uma lista ausente ou vazia é
inerte. O `project` canônico ainda prevalece quando já for resolvido. Caso contrário,
cada ocorrência de alias local deve ser desduplicada para um projeto portando a identidade
remota exata com host do checkout; uma identidade diferente ou mais de um projeto
falha de forma fechada. A consulta de aliases nunca pesquisa em outro espaço de trabalho, e a captura pode
renomear apenas quando `project` for igual ao nome canônico no estilo de caminho derivado
daquela identidade remota exata.

### Rotas na home do operador

O marcador exato na home do operador (`$AI_MEMORY_HOME/.ai-memory.toml`, caso contrário
`$HOME/.ai-memory.toml`, com `%USERPROFILE%` como o fallback portátil no Windows)
pode rotear muitos checkouts não relacionados sem posicionar um marcador em cada repositório:

```toml
[routes.identity."github.com/acme/api"]
route_workspace = "oss"
route_project = "acme-api"
route_identity_style = "path"
route_aliases = ["main"]

[routes.path."~/src/api"]
route_workspace = "oss"
route_project = "acme-api"
route_aliases = ["old-api"]
```

Seletores de identidade devem ser identidades remotas exatas e normalizadas com host; um
seletor sem host `acme/api` é inválido. Seletores de caminho devem ser absolutos ou
relativos a `~/`. Eles são normalizados lexicalmente sem resolver links simbólicos, oferecem suporte a
raízes POSIX, de unidade do Windows e UNC, e correspondem por componentes de caminho: `/team-b`
não corresponde a `/team-bb`. Um seletor `~/` pode normalizar dentro de HOME (por
exemplo `~/a/../b`), mas é rejeitado se algum `..` escapar acima de HOME;
seletores absolutos retêm a fixação lexical de raiz. A correspondência de caminho mais longa prevalece, enquanto uma
correspondência de identidade exata prevalece sobre qualquer correspondência de caminho. Um marcador de configurações local acessível fora da home
prevalece sobre todas as rotas da home. Quando nenhuma rota coincidir, as configurações existentes no nível raiz
do marcador da home permanecem como fallback, seguidas pela identidade de caminho
remoto padrão e depois pelo fallback instalado/cwd; escopo explícito via CLI sempre prevalece. Uma rota correspondida substitui
apenas os campos de roteamento raiz `workspace`, `project`, `identity`, `identity_style`, `aliases` e
`project_strategy`. Configurações raiz de `server`, `drop_subagent_captures`,
`[recall]`, `[briefing]`, `[profile]`, `[handoff]`, `[recovery]` e
`[messaging]` continuam se aplicando.

Um mapa de rotas da home é limitado a 64 rotas, um arquivo de 64 KiB, 512 bytes UTF-8 por
seletor e 512 bytes UTF-8 por valor escalar. Sua gramática portátil permite
apenas cabeçalhos de tabela exatos `[routes.identity."selector"]` e `[routes.path."selector"]`
seguidos por uma declaração de cada campo `route_*` aceito. Espaço de trabalho
e projeto são obrigatórios; estilo e aliases são opcionais. Aliases devem ser um
array de strings compatível com JSON em uma única linha dentro dos limites normais de `MarkerAliases`.
Seletores brutos duplicados entre qualquer tipo de rota, caminhos normalizados duplicados,
identidades normalizadas iguais, campos duplicados, arrays multilinhas, vírgulas
finais, escapes, seletores ou campos malformados, chaves de rota desconhecidas, estilos
inválidos e aliases inválidos invalidam o mapa de rotas inteiro em vez de escolher
um projeto. A sintaxe de rota reconhecível inclui `[routes`, `routes =`, `routes.`,
e declarações órfãs de `route_*`; configurações malformadas exclusivas da raiz sem nenhuma
dessas formas retêm o comportamento histórico do analisador de linhas. Um mapa inválido ou superdimensionado nunca recorre ao escopo da raiz da home:
comandos sem um par explícito completo de espaço de trabalho/projeto falham, enquanto hooks
aceitam a chamada de ciclo de vida, mas a descartam antes do spool ou da rede. Aliases de rota usam os mesmos limites e gramática que aliases
de marcadores locais e são encaminhados apenas com um remoto git com host descoberto e
`project_src=marker`. Os nomes filhos `route_*` são intencionais: clientes mais antigos baseados em linha
os ignoram em vez de tratar campos de rota como configurações raiz. Clientes portáteis usam `HOME`, depois `USERPROFILE`; CLI/hooks nativos respeitam
`AI_MEMORY_HOME` primeiro. O wrapper do Docker passa o cwd do host, de modo que rotas de caminho
sejam comparadas no namespace do host em vez de `/work`, enquanto a descoberta de remotos pode
recorrer ao checkout montado.

`project_strategy` aceita apenas `repo-root` (ou `repo_root`). Valores
desconhecidos são ignorados e se comportam como a estratégia padrão `basename(cwd)`.

`[profile] contribute` e `consume` ficam ativados a menos que o valor seja explicitamente
falso (`false` / `0` / `no` / `off`, entre aspas ou puro). Um marcador que define apenas
chaves de `[profile]` é um limite de configurações, como um com chaves de `[briefing]`.

`[handoff] deliver_on_session_start` fica ativado a menos que o valor seja explicitamente falso
(`false` / `0` / `no` / `off`, entre aspas ou puro). Clientes enviam `handoff=0` na
busca de início de sessão apenas quando o marcador o desativa, de modo que o servidor não lê
nada, não reivindica nada e não adiciona aviso de handoff ou continuidade para aquele
projeto. Um marcador que define apenas `[handoff]` é um limite de configurações, como um
com chaves de `[briefing]` ou `[profile]`. Use-o para projetos cujos handoffs
automáticos sejam em sua maioria ruído, como uma frota de bots orientada por rotinas e webhooks.

`[recovery] deliver_on_session_start` também fica ativado a menos que seja explicitamente falso.
Clientes então enviam `recovery=0`, o que suprime apenas o snapshot de recuperação
de sessão inacabada e seu marcador de preenchimento retroativo (backfill). A sessão de origem permanece aberta, e
handoffs, contexto gerenciado, o briefing, o digest de perfil e o aviso da caixa de entrada
ainda são entregues. Um marcador que define apenas `[recovery]` é um limite de
configurações. Use isso em projetos com sessões paralelas de longa duração, onde o snapshot
de recuperação de outra sessão geralmente é ruído para essa disposição.

`[recall] default_global` se aplica às chamadas sem escopo `memory_query` e
`memory_recent` de um agente. Em `memory_query`, um `global: false` explícito mantém
o projeto resolvido, mesmo quando o marcador habilita a recuperação global. `workspace`
+ `project` explícitos ou `scopes` também sobrepõem o marcador. Uma consulta com
`as_of` lê o projeto atual como ele estava naquele instante, e um `global=true` explícito
com `as_of` é recusado. Os comandos de terminal `ai-memory search` e
`ai-memory recent` não leem essa configuração: esses comandos permanecem
com escopo restrito ao projeto atual e cobrem todos os projetos apenas quando você passa
`--global` (veja
[usage.md](usage.md#busca-a-partir-do-terminal) e
[usage.md](usage.md#páginas-recentes-a-partir-do-terminal)).

`default_global`, `inject_on_session_start`, `inject_on_subagent_start` e
`inbox_notice_on_prompt` aceitam um valor verdadeiro
(`true` / `1` / `yes` / `on`, entre aspas ou puro; chaves no estilo de seção são
analisadas com tolerância); qualquer outra coisa se comporta como ausente. `max_chars` é um
inteiro simples.

O briefing de início de sessão tem **escopo de projeto**: ele extrai apenas a partir do
`(workspace, project)` resolvido da sessão. Conhecimento entre projetos alcança
a sessão por meio do **digest de perfil**, uma seção cercada separada
após o briefing (veja `docs/cross-project-profile.md`): as entradas do perfil
são entregues como padrões que o agente aplica quando o usuário e o arquivo
de regras do repositório nada dizem. Uma regra estrita que deve valer neste
repositório independentemente do que os padrões digam continua pertencendo ao próprio arquivo de regras
do agente (`CLAUDE.md` / `AGENTS.md`), que tem precedência sobre a memória. Veja as
orientações de "Regras, memória e o perfil" em `docs/usage.md`. Outras páginas `_global`
permanecem acessíveis sob demanda por meio de `memory_query`, que as une.

`drop_subagent_captures` aceita uma string verdadeira (`"true"` / `"1"` /
`"yes"` / `"on"`); qualquer outro valor, ou sua ausência, mantém as capturas de
subagente deste projeto armazenadas como de costume. Sessões de nível superior (não subagentes) são
sempre armazenadas independentemente disso, incluindo sessões do Claude Code iniciadas com
`--agent`: `agent_type` por si só descreve o agente selecionado, enquanto um
`agent_id` não vazio identifica um subagente. O Claude Code relata um subagente sob o
`session_id` pai, de modo que apenas os eventos que carregam `agent_id` (mais
`SubagentStart`/`SubagentStop`) são descartados e os próprios prompts do pai,
Stop, SessionEnd e resumo são mantidos. O marcador `subagentType` do Grok
ainda identifica capturas de subagente. A extensão OMP envia todos os eventos de uma
sessão de subagente com `agent_id`, portanto toda a sessão de subagente é descartada,
enquanto chamadas de ferramentas de um orientador permanecem com a sessão que orientam. Isso é
intencionalmente por projeto: não há uma chave global no
servidor, de modo que optar por descartar em um projeto ruidoso nunca afeta as capturas de subagentes dos
outros em uma instância compartilhada.

## Modo lista de permissões (allowlist): o marcador como autorização

Por padrão, este arquivo é opcional: um repositório sem ele ainda é capturado,
e o marcador apenas *restringe* o que é coletado. Uma instalação pode inverter isso:

```bash
ai-memory install-hooks --apply --capture-mode allowlist
```

Sob o modo lista de permissões, a presença de um `.ai-memory.toml` **é** a autorização explícita (opt-in). Um
repositório sem ele não emite nenhum evento de ciclo de vida. Prompts, chamadas de ferramentas
e limites de sessão são descartados no processo de hook antes que qualquer coisa
alcance o spool local ou a rede. Nenhuma chave adicional é necessária: um marcador existente
já habilita seu repositório, qualquer que seja sua outra configuração.

Isso altera o custo de esquecer o arquivo. Sob o padrão, esquecer
significa que um repositório é capturado sem que você tenha planejado; sob o modo
lista de permissões, significa que um repositório permanece silencioso quando você talvez desejasse a captura. Escolha a
direção cuja falha você prefere ter que justificar.

O modo é armazenado por instalação e não por agente, e um comando posterior puro
`install-hooks --apply` (incluindo uma atualização de versão) o mantém ativo.

Ele é aplicado tanto pelos comandos nativos `ai-memory hook` quanto pelas
integrações geradas em TypeScript (`pi`, `omp`, `opencode`, `opencode2`, `openclaw`);
cada uma embute o modo selecionado e executa a mesma validação de presença do marcador
antes de fazer qualquer POST. Apenas os caminhos de fallback brutos por script (a
sobreposição de `AI_MEMORY_HOOK_PLATFORM`, o wrapper de host do Docker e
trechos de `setup-agent`) fazem POST diretamente para o servidor sem executar nenhum
dos pontos de aplicação, de modo que o modo lista de permissões não os bloqueia.

## Roteamento de captura para outro servidor (`server`)

Uma máquina pode entregar capturas de hook para mais de um servidor ai-memory, por
exemplo quando cada organização para a qual você trabalha opera o seu próprio. Registre cada servidor
localmente sob um nome e permita que o marcador de um repositório selecione um:

```bash
ai-memory server add team-a --url https://memory-a.example.com --root ~/work/team-a --auth-token-stdin
ai-memory server add team-b --url https://memory-b.example.com --root ~/work/team-b --auth-token-stdin
ai-memory server list      # names, URLs, roots, and whether a token is stored; never the token
```

```toml
# ~/work/team-b/.ai-memory.toml
workspace = "team-b"
server = "team-b"          # a profile NAME, never a URL
```

Perfis residem em `<data_dir>/servers.toml`; cada token é armazenado separadamente em
`<data_dir>/auth-tokens/<name>`, somente para o proprietário. Um marcador, uma configuração de hook renderizada
e uma linha de comando de processo nunca contêm uma URL ou token para um perfil. Um
repositório sem uma chave `server` continua usando o servidor configurado pelo
`install-hooks`, exatamente como antes. O `ai-memory uninstall` remove os tokens
armazenados juntamente com os hooks que os leem e mantém o registro, de modo que uma
reinstalação posterior lista cada perfil com `token: missing` até que você armazene um
novamente.

O marcador é conteúdo do repositório, portanto trate-o como não confiável: ele pode apenas escolher
entre servidores que **você** registrou. Quando existirem vários perfis, cada um deve
declarar diretórios `--root`, e um marcador fora das raízes de um perfil não pode
selecioná-lo. É isso que impede que um repositório clonado indique o perfil
de outra equipe.

Toda falha ocorre de forma fechada (fail-closed). Se o perfil indicado for desconhecido, não tiver token,
estiver fora de suas raízes, necessitar de raízes que não possui, ou se `servers.toml` estiver
malformado, o hook não emite **nada** para aquele evento: nada entra no spool, nenhum
handoff é buscado e um aviso de uma linha vai para o stderr do hook. Ele nunca
recorre ao padrão da instalação, porque isso entregaria a captura
de uma equipe ao servidor de outra equipe.

Os comandos que trabalham no checkout em que você os executa seguem a mesma rota,
de modo que alcançam o servidor para o qual seus hooks fazem a captura: `run`, `resume`, `continue`,
`show`, `workstreams`, `workstream-search`, `rename-workstream`, `doctor`,
`search`, `recent`, `read-page`, `write-page`, `delete-page`,
`restore-page`, `handoffs`, `message`, `pending-writes`, `profile`, `lint`,
`forget-sweep`, `curator`, `auto-improve`, `auto-improve-report`, `embed`,
`bootstrap`, `backfill`, `repair-backfill-timestamps`, `repair-split`,
`finalize-session`, `rename-project`, `move-project`, `move-session`,
`purge-project` e `purge-session`. Eles usam a URL
e o token armazenado do perfil, nunca o bearer da sua configuração, e falham
de forma fechada com o mesmo motivo que o hook relata quando o perfil não
resolve. Eles comparam as raízes do perfil com o diretório como o seu
shell o nomeia (`PWD`, quando é o mesmo diretório que o diretório de trabalho do processo),
do modo como um harness o informa aos hooks, de modo que um checkout alcançado
por um link simbólico, ou o `/var` do macOS (na verdade `/private/var`), roteia igual
para ambos. `AI_MEMORY_SERVER_URL` no ambiente ainda vence para uma
invocação, e `AI_MEMORY_IGNORE_MARKER=1` ignora o marcador, como faz para o
escopo. Os comandos de toda a instalação (`status`, `list-projects`, `backup`, `user`,
`api-key`, `project access`, `setup` e os demais comandos de administração) continuam usando
o servidor configurado. `run --jail` recusa um checkout roteado por perfil,
porque a execução na jail não consegue ler o token do perfil; defina
`AI_MEMORY_SERVER_URL` e um `AI_MEMORY_AUTH_TOKEN` de menor privilégio para essa
inicialização.

O roteamento é herdado pela árvore abaixo, diferentemente das outras chaves. O marcador mais próximo
que declare `server` decide, de modo que um marcador aninhado que defina apenas `workspace`,
ou apenas `[capture]`, mantenha o perfil de seu ancestral. Um marcador aninhado pode selecionar
um perfil diferente, mas somente se as raízes desse perfil o admitirem. Fora
de `$HOME`, a busca não para na raiz do checkout, portanto um
marcador em nível de organização acima de um repositório (`/srv/work/team-b/`) ainda
o roteia. Dentro de `$HOME`, ela para em `$HOME`, como para qualquer outra chave. Observe
que o modo lista de permissões mantém sua própria regra de marcador mais próximo: fora de `$HOME`, um
repositório cujo único marcador seja aquele em nível de organização é roteado, mas não
habilitado, portanto, sob o modo lista de permissões ele não emite nada até possuir um marcador
próprio.

A seleção também falha de forma fechada quanto ao formato:

- Um `server =` sem aspas ou vazio é uma seleção que falha na validação de nome,
  nunca um "sem seleção" silencioso.
- Um BOM UTF-8 ou um byte não UTF-8 solto não pode ocultar a chave.
- Um marcador no caminho ascendente que não possa ser lido é recusado
  (`rejected-unreadable-marker`), porque ele pode declarar um perfil.
- Um evento cujo payload não traga `cwd` é roteado pelo diretório de
  trabalho do processo de hook em vez de seguir para o padrão de instalação.

Executar novamente `ai-memory server add` para um perfil existente mantém suas raízes quando
`--root` é omitido, de modo que rotacionar um token não pode suspender a restrição. Alterar
a `--url` de um perfil sem um novo token descarta o token antigo primeiro: um token
pertence ao servidor para o qual foi emitido, portanto o perfil recusa eventos até que
você adicione um para a nova URL. Tanto tabelas `[servers.<name>]` quanto uma tabela inline
`servers = { ... }` podem ser lidas e atualizadas por esses comandos.

Verifique a decisão para um diretório sem enviar nada:

```bash
echo '{"cwd":"'"$PWD"'"}' | ai-memory hook --event user-prompt-submit --agent claude-code \
  --server-url http://127.0.0.1:49374 --check-capture
```

`server_resolution` é `install-default`, `marker`, ou um de
`rejected-unknown-profile`, `rejected-no-token`, `rejected-outside-roots`,
`rejected-roots-required`, `rejected-invalid-profile-name`,
`rejected-unreadable-marker` ou `rejected-invalid-registry`. `server_profile`
nomeia o perfil; nenhuma URL, caminho ou token é exibido.

**Integrações compatíveis.** Comandos nativos `ai-memory hook` roteiam perfis,
incluindo a busca de handoff de início de sessão, os eventos em spool escoados posteriormente
e o preenchimento retroativo único de inicialização. As integrações geradas em TypeScript
(`opencode`, `opencode2`, `omp`, `pi`, `openclaw`) ainda não realizam esse roteamento: um
repositório cujo marcador selecione um perfil não emite nada a partir delas e não busca
handoff. Os hooks por script (os pacotes `.sh` e `.ps1` usados pela sobreposição
`AI_MEMORY_HOOK_PLATFORM`, o wrapper de host do Docker e trechos de `setup-agent`)
também não podem rotear e, da mesma forma, descartam os eventos e a busca de handoff de um repositório roteado.
Clientes mais antigos que esta alteração ignoram a chave e entregam ao seu
padrão de instalação, portanto atualize todos os clientes antes de adicionar `server` a um
marcador compartilhado.

**Limitações conhecidas.**

- **Binários mistos em um único diretório de dados.** Eventos em spool registram seu perfil em um
  campo que binários mais antigos desconhecem. Se um `ai-memory` mais antigo escoar o mesmo
  spool, ele tratará eventos de perfil como padrão de instalação: poderá tentar novamente um
  evento rejeitado com o token de instalação ou reenviar para o servidor em `config.toml` um evento
  endereçado a uma porta de loopback inativa. Atualize todas as instalações que
  compartilham um diretório de dados.
- **MCP e `ai-memory run`.** Perfis roteiam apenas a captura de hook. A entrada
  do servidor MCP e o `ai-memory run` (seu livro-razão de execução gerenciada e heartbeats)
  continuam se comunicando com o servidor com o qual foram configurados, portanto em um repositório
  roteado eles alcançam o padrão da instalação. Aponte o cliente MCP daquele repositório
  para seu próprio servidor com um `.mcp.json` por repositório e evite
  `ai-memory run` lá até que ele suporte perfis.

Perfis usam apenas tokens estáticos. O `auth.json` do OIDC permanece com o padrão
da instalação e nunca é apresentado ao servidor de um perfil.

**Um servidor, várias identidades.** Dois perfis podem compartilhar uma URL e diferir
apenas em seu token. Em um servidor compartilhado que restringe projetos por usuário
(#708), isso permite que cada repositório se autentique como a conta que tem acesso
a ele:

```bash
ai-memory server add team-a --url https://memory.example.com --root ~/work/team-a --auth-token-stdin   # team-a account
ai-memory server add team-b --url https://memory.example.com --root ~/work/team-b --auth-token-stdin   # team-b account
```

Eventos de perfis diferentes nunca compartilham uma requisição, e as novas tentativas
de cada perfil usam apenas seu próprio token.

## Exclusões de captura

Use o formato exato por repositório `[capture]` mais `ignore_paths = [...]`
abaixo para manter fora da captura atividades reconhecidas de ferramentas de arquivo e ferramentas de shell
sob caminhos correspondentes:

```toml
[capture]
ignore_paths = ["private/**", "~/personal-notes/**"]
```

> **`ignore_paths` é um filtro de melhor esforço para comandos de shell e ferramentas
> desconhecidas.** Ferramentas de arquivo são correspondidas por seus campos explícitos de caminho, mas um comando
> de shell é lido apenas lexicalmente e uma ferramenta não listada pelo comparador
> não é lida de forma alguma. As [lacunas listadas abaixo](#capture-exclusion-residuals) permanecem
> abertas: variáveis de shell (`$VAR`), um `cd` dentro de um comando, links simbólicos, uma
> busca recursiva em um diretório pai, ferramentas desconhecidas e scripts que um
> comando lê de um arquivo ou de um heredoc. Para um projeto sensível, use o
> **modo de captura por lista de permissões** (`install-hooks --capture-mode allowlist`, acima)
> e deixe aquele repositório sem marcador. Esse modo é a garantia
> estrita: nada de um repositório sem marcador é capturado.

Um repositório que mantém seus registros de decisão na árvore (um diretório de ADRs,
uma árvore `context/` do [Keep the Why](https://github.com/oliver-zehentleitner/keep-the-why))
pertence aqui também: `ignore_paths = ["docs/adr/**"]` ou
`["context/**"]`. O repositório detém esse registro; sem a exclusão, a leitura
feita por um agente é capturada e a consolidação a compila em páginas wiki que
não acompanham o repositório, de modo que a cópia fica desatualizada no instante em que o registro é substituído
(veja a seção "Registros de decisão nativos do repositório" em [`usage.md`](usage.md)).

O `.ai-memory.toml` **mais próximo** é autoritativo; seções de marcador não se
mesclam. Uma seção `[capture]` ausente ou `ignore_paths = []` é inativa e
preserva o comportamento atual. `[capture]` aceita apenas `ignore_paths`: chaves
desconhecidas, tipos/globs/raízes inválidos, marcadores ilegíveis ou um marcador com mais de 64 KiB
invalidam a política de captura inteira em vez de aplicá-la parcialmente. Qualquer
grafia TOML para a tabela funciona (`[ capture ]`, `["capture"]`, ou um ponto no nível superior
`capture.ignore_paths = [...]`), e os formatos que o TOML não converte em tabela
simples (um `capture = { ignore_paths = [...] }` em linha, `[[capture]]`, uma
subtabela `[capture.x]`, um segundo `[capture]`) são inválidos. O hook nativo e
as integrações geradas em TypeScript chegam à mesma decisão para cada um.

Padrões correspondem a um caminho inteiro lexicalmente normalizado, não a uma substring. Use apenas
`*`, `?` e `**`; padrões relativos têm raiz no diretório do marcador,
caminhos relativos de ferramentas de arquivo são resolvidos a partir do `cwd` real do evento, e `~/`
expande para o diretório pessoal. Prefira barras normais (`/`) em todas as plataformas.
A correspondência POSIX diferencia maiúsculas de minúsculas; a correspondência de unidade/UNC no Windows não
diferencia maiúsculas de minúsculas ASCII. Os limites são 128 padrões, 1.024 caracteres por padrão, 32
candidatos diretos e 4.096 caracteres por candidato, e 1.000.000 de comparações
delimitadas entre padrão/candidato.

Um `**/` corresponde a um ou mais diretórios, nunca a zero, diferentemente do `.gitignore`:
`**/.env` corresponde a `sub/.env`, mas não ao `.env` no diretório do marcador, e
`docs/**/x.md` não corresponde a `docs/x.md`. Liste ambas as formas quando quiser ambas
(`[".env", "**/.env"]`). Um `/**` final corresponde ao próprio diretório
(`secrets/**` cobre `secrets`).

Para ferramentas de arquivo diretas comprovadas por fixtures, o ai-memory lê apenas campos explícitos de caminho
e arrays diretos documentados para chamadas multi-arquivo. Se algum candidato corresponder, o
evento inteiro é **descartado localmente** antes do spool, fila, rede, logs de transporte
ou armazenamento do servidor. Com uma política ativa, ferramentas reconhecidas de busca/listagem são
descartadas conservadoramente; candidatos a arquivos reconhecidos ausentes ou malformados, um
esquema reconhecido sem suporte, ou uma política inválida (para ferramentas de arquivo, busca/listagem e
shell) tornam-se **somente metadados**.
Esse formato contém apenas metadados delimitados de roteamento/ferramenta/decisão, nunca caminhos,
padrões, argumentos, saída, erros, títulos ou payload aninhado. Ferramentas que leem
a saída de um comando iniciado por uma chamada anterior (`BashOutput`
e `TaskOutput` do Claude Code, `write_stdin` do Codex, `process` do Hermes e OpenClaw,
`read_bash`/`write_bash` da GitHub Copilot CLI e suas gêmeas do PowerShell) tornam-se
somente metadados sob uma política ativa ou inválida, porque nada prova que o
comando lido por elas tenha evitado todos os caminhos ignorados. Pelo mesmo motivo, uma ferramenta de arquivo ou
shell que lê um arquivo no qual o harness salvou a saída de uma ferramenta anterior é
somente metadados sob uma política ativa ou inválida: resultados grandes salvos do Claude Code
(`~/.claude/projects/<project>/<session>/tool-results/…`) e
saída de tarefas em segundo plano (`<tmp>/claude-<uid>/<project>/<session>/tasks/<id>.output`),
saída truncada do OpenCode (`<data>/opencode/tool-output/…`) e do Pi
(`<tmp>/pi-bash-<id>.log`). Um glob que possa corresponder a tal arquivo conta também
(`cat ~/.claude/projects/*/*/tool-results/*`): cada segmento de glob se ajusta a um
nome de diretório ou arquivo com o qual compartilha uma correspondência, e pelo menos um segmento compatível
deve conter parte do texto característico daquele nome (`claude`, `projects`,
`tool-results`, `tasks`, `.output`, `opencode`, `tool-output`, `pi-bash-`)
literalmente, com pelo menos três caracteres seguidos: `tool-res*` e `/tmp/pi-*`
contam, enquanto `ls src/*`, `tail -n 50 log/*.log` e `cat /tmp/*.log` (cujo
curinga ocupa o lugar onde `pi-bash-` estaria) são globs comuns. Outras ferramentas desconhecidas
mantêm o comportamento atual. Os nomes em tempo de execução `view`, `create` e
`str_replace_editor` da GitHub Copilot CLI são ferramentas de arquivo lidas a partir de seu argumento `path`.

A ferramenta `read_many_files` do Gemini CLI é uma ferramenta de arquivo cujas entradas de `include` (`paths`
em releases mais antigas) são globs que o Gemini expande sem diferenciar maiúsculas/minúsculas, com `**`
alcançando qualquer profundidade. O ai-memory lê todas as entradas e descarta a chamada inteira quando
qualquer uma indicar um caminho ignorado ou puder se expandir em um, comparando sem diferenciar maiúsculas/minúsculas
como o Gemini: `docs/adr/x.md`, `docs/*/x.md`, `docs/**` e `**/*.md` são
todos descartados sob `docs/adr/**`, enquanto `src/**/*.rs` e um `*.md` no nível da raiz
não são. Um `..` após um `**` é lido de ambas as formas, já que `**` pode corresponder a nenhum
diretório bem como a muitos. O Gemini lê uma entrada que nomeia um diretório recursivamente, e o
comparador não pode distinguir um diretório de um arquivo, portanto uma entrada que possa conter um
caminho ignorado também descarta a chamada: `docs`, `.` e `docs/` são descartados sob
`docs/adr/**`, enquanto `src` e `README.md` não são. Sua lista `exclude` nunca
subtrai um caminho ignorado. Uma entrada usando sintaxe
de glob que o comparador não possa avaliar (`[…]`, `{…}`, `(…)`, `!`) torna a chamada
**somente metadados**.

`read` e `write` do OMP podem nomear um arquivo por meio de um sufixo de `path`: um seletor
de linha (`notes.txt:450-455`, `:-5`, `:raw`), um membro de arquivo compactado
(`vault.zip:notes/a.md`), uma tabela ou linha SQLite (`app.db:users:42`), uma
visão executável (`bin:main`), um tempo de vídeo ou uma consulta (`data.json?q=.items`).
O OMP lê o arquivo com o nome completo quando ele existe e, caso contrário, a parte
antes do sufixo, de modo que o ai-memory também compara todas as partes do caminho antes de um
`:` ou `?`: `secrets.env:1-20` e `key.pem:1-40` são descartados sob `secrets.env` e
`*.pem` como os nomes puros, e `vault.zip:notes/a.md` é descartado sob
`vault.zip`. Um prefixo de unidade ou dispositivo Windows (`C:`, `\\?\`) não encerra nenhuma parte, e
um caminho com mais de 32 partes é **somente metadados**.

Ambos os harnesses também reescrevem o caminho de uma ferramenta de arquivo antes de abri-lo, e o ai-memory
compara o caminho reescrito também. `read`, `write` e `edit` do OMP descartam um
`:` inicial avulso antes de um caminho (`:/repo/secrets.env`, `:./key.pem`) e um
`@` inicial antes de um caminho absoluto ou `~` (`@/repo/secrets.env`). As do Pi descartam
qualquer `@` inicial (`@secrets.env`). Ambos leem uma URL `file://` como o caminho que ela
indica. Na extensão gerada, um caminho que começa com `:` é
**somente metadados**, já que a extensão não pode localizá-lo.

Ferramentas de shell reconhecidas (`Bash`, `PowerShell`, `shell`, `exec`, `execute_bash`,
`terminal`, `run_shell_command` do Gemini CLI, …) não possuem campo de caminho, portanto a linha de comando é dividida em palavras lexicalmente, da forma
como um shell POSIX as coloca entre aspas e as separa, sem expandir ou executar nada.
Chamadas de shell do Codex, incluindo seu caminho `exec_command`, chegam aos hooks como `Bash`
com o comando em `tool_input.command`.
Um comando fornecido como um vetor de argumentos mantém cada elemento como uma palavra (um caminho
com espaços permanece inteiro, até 256 caracteres) e também divide cada elemento por
conta própria. Qualquer palavra que contenha mais de uma palavra é dividida novamente da mesma forma,
seja ela um elemento de vetor de argumentos, um script entre aspas (`bash -c '…'`,
`sh -c "…"`), ou uma substituição de comando dentro de aspas duplas (`"$(…)"` ou
crases), de modo que um script é lido como qualquer linha de comando, até quatro níveis
de aninhamento de aspas; um script com aninhamento mais profundo é **somente metadados**. Uma mensagem entre aspas que
mencione um caminho ignorado (`git commit -m "…docs/adr/…"`) portanto descarta o
evento também. Uma string ANSI-C (`$'…'`) é decodificada, uma barra invertida antes de uma
letra comum é descartada, e uma barra invertida seguida de quebra de linha fora de aspas simples
une as duas linhas, como o shell faz. Uma palavra com um parâmetro padrão
(`${X:-docs/x.md}`, também `:=`, `:+` e as formas sem `:`) também é lida
com o operando em seu lugar. Sob a ferramenta `PowerShell`, ou em um host
Windows, o comando também é lido sem a crase do PowerShell e os caracteres de escape de acento circunflexo do cmd.
Cada
argumento que não seja uma flag, mais o valor de uma palavra `--flag=value`, `NAME=value`,
ou `-Param:value` do PowerShell, mais um valor anexado a uma flag curta
(`-fdocs/x.md`), mais o caminho local de uma URL `file:` em qualquer combinação de maiúsculas/minúsculas (cortada em sua
consulta `?` ou fragmento `#`, depois com escapes percentuais ASCII decodificados), é resolvido como um caminho de ferramenta de arquivo: a partir do próprio
argumento `workdir` da ferramenta (`dir_path` do Gemini CLI, `Cwd` do Antigravity) quando houver, caso contrário a partir do `cwd` do evento.
`~/` (e o `~\` do PowerShell) é o diretório pessoal e `~+` o diretório
de trabalho. Um argumento sob `~-`, `~N` ou `~name` (o diretório anterior,
a pilha de diretórios, a home de outra conta) indica um diretório que apenas o
shell em execução conhece, portanto o comando é **somente metadados** sob uma política
ativa. Apenas um til expandido pelo shell conta: um entre aspas
(`git commit -m "fix ~approx"`, `grep '~[a-z]+'`), um dentro de um elemento
de vetor de argumentos, ou um em uma mensagem entre aspas é literal; um em um script executado por
`sh -c` ou substituição de comando é lido. O PowerShell resolve `~` por conta própria, portanto
sob a ferramenta `PowerShell` ou em um host Windows todo til conta. Um
parâmetro padrão com aninhamento superior a quatro níveis torna o comando
**somente metadados**, e um escape de barra invertida dentro de um padrão
(`${X:-d\ocs/x.md}`) é lido como o shell o lê.
O caminho em um nome de objeto git (`HEAD:docs/x.md`, `:docs/x.md`) é resolvido
tanto a partir do `cwd` quanto do diretório do marcador, porque o git o lê a partir da raiz do
repositório. Se
um corresponder a um padrão, o evento inteiro é **descartado**, exatamente como uma leitura
de arquivo correspondente. Um argumento contendo `*` ou `?` também corresponde quando seu glob puder
alcançar o diretório de um padrão: `cat docs/*/0001.md` é descartado sob
`docs/adr/**`, mas `cat *.md` na raiz do repositório não é. Um `**` corresponde a nenhum
diretório bem como a muitos sob o `globstar` do zsh e bash, portanto um `..` após ele
é lido de ambas as formas. O bash antes da versão 5.2 (o macOS inclui a 3.2) também permite que um padrão que
começa com um ponto corresponda à entrada `..`, portanto um segmento de diretório que pudesse
(um ponto seguido de `*`, `?` ou uma classe que admita ponto) é lido como `..`
também. Um comando que excede
o orçamento de correspondência é descartado. Um comando ao qual nenhum argumento corresponda diretamente é
**somente metadados** quando usa sintaxe que o comparador não avalia: uma
expansão de chaves sem aspas (`docs/{adr,x}/1.md`, `{1..3}`), uma classe entre colchetes
(`docs/ad[r]/1.md`, incluindo uma que comece com `]`), um grupo de glob ou
extglob do zsh (`docs/(a|b)/1.md`, `@(…)`), ou um escape numérico ou de controle em uma
string ANSI-C. O mesmo vale para aqueles dentro de um script executado por `sh -c`
(inclusive após opções do shell e seus valores, como em `bash -o pipefail -c`),
`su -c`, `script -c`, `eval` ou uma substituição de comando, mas não dentro de uma mensagem ou padrão entre aspas
(`git log --format='%h {a,b}'`). Chaves e colchetes entre aspas ou com escape,
`${…}`, `{}`, `[ -f x ]`, `$(…)`, `<(…)` e uma subshell `( … )` não são
afetados; uma palavra anexada a um parêntese (`f()`, `(a|b)x`) também conta como um
grupo de glob. Código de interpretador inline (`python3 -c`, `node -e`, `ruby -e`,
`perl -e`, `php -r`, `lua -e`, `Rscript -e`, `osascript -e`, `deno eval`,
também após opções que recebem um valor, como em `python3 -W ignore -c`) é
**somente metadados** sob uma política ativa
a menos que uma de suas palavras indique um caminho ignorado diretamente, o que o descarta:
código pode construir um caminho em tempo de execução (`'docs/' + name`), de modo que nenhuma correspondência lexical pode
descartá-lo com segurança. A ferramenta `execute_code` do Hermes Agent, que executa o código-fonte de programas
fornecido a ela, é somente metadados sob uma política ativa pelo mesmo motivo.
Executar um arquivo de script ou um módulo (`python3 -m pytest`,
`node build.js`) não é afetado. Um comando de shell com mais de 1 MiB (1.048.576
caracteres, somados sobre os elementos de um vetor de argumentos) não é lido de forma
alguma: ele é somente metadados sob uma política ativa, e sua busca por marcador
(abaixo) conta como não comprovável, o que o torna somente metadados mesmo em um
diretório sem marcador. Variáveis, substituição de comandos, estado de `cd` e
comandos que não indicam nenhum caminho (`rg TODO`, `git diff`) não são acompanhados, de modo que
sua saída ainda é capturada. Uma política inválida torna um comando de shell
**somente metadados**, como uma ferramenta de arquivo, porque um marcador quebrado não pode provar que seus
argumentos evitam todos os caminhos ignorados; isso vale mesmo quando o comando não puder ser
lido. No macOS, cujo sistema de arquivos padrão ignora maiúsculas e minúsculas, o hook nativo e as
integrações geradas comparam caminhos de ferramentas de arquivo e shell sem diferenciar maiúsculas/minúsculas
(`NOTES/X.md` corresponde a `notes/**`, `ÄRGER/x.md` corresponde a `ärger/**`) e tratam um
acento decomposto como a letra composta, como faz seu sistema de arquivos; outros hosts
POSIX comparam diferenciando maiúsculas de minúsculas, e caminhos no Windows sempre ignoram maiúsculas/minúsculas ASCII.

Em uma sessão Windows (o `cwd` do evento é uma unidade ou caminho UNC), o caminho de uma ferramenta
de arquivo, argumento de shell ou `workdir` é lido como o arquivo que o Windows abre, de modo que outras
grafias de um arquivo ignorado correspondem ao seu padrão também: Git Bash, MSYS2 e Cygwin
`/c/Users/me/repo/x` e `/cygdrive/c/...` são `C:\Users\me\repo\x`; um caminho
com raiz sem unidade (`/repo/x`, `\repo\x`) está na unidade do `cwd`; os
prefixos `\\?\`, `\\.\` e `\??\` e o compartilhamento administrativo local
(`\\localhost\C$\...`, `\\127.0.0.1\C$\...`) nomeiam a própria unidade; um
sufixo de fluxo de dados alternativo (`private.md::$DATA`, `private.md:stream`) e os
pontos e espaços finais de cada segmento (`private.md.`, `secrets. \x`) são
descartados. Qualquer outro caminho de dispositivo (`\\.\pipe\x`) torna uma ferramenta de arquivo
somente metadados. Uma ferramenta nativa lê `/c/x` a partir da raiz da unidade do `cwd`
(`D:\c\x` a partir de `D:\repo`) em vez de como o Git Bash faz, de modo que ambas as leituras são
verificadas e qualquer uma que corresponda descarta a chamada. O `cwd`, o diretório do
marcador e um padrão absoluto são lidos da mesma forma, de modo que uma sessão iniciada
a partir de `\\?\C:\repo`, `\\?\UNC\server\share\repo` ou `\\localhost\C$\repo`
compara `docs/private/**` contra `C:\repo\docs\private\a.md`, e um padrão
grafado como `\\?\C:\repo\docs\private\**` corresponde a ele a partir de `C:\repo`. O PowerShell
e o cmd não usam barra invertida para escape, de modo que um comando de shell também é lido com
todas as barras invertidas mantidas: `Get-Content \\server\share\x`, sem aspas ou com aspas duplas,
nomeia o caminho UNC. O prefixo de provedor do PowerShell é descartado primeiro:
`FileSystem::C:\repo\x` e `Microsoft.PowerShell.Core\FileSystem::C:\repo\x`
(qualquer combinação de maiúsculas/minúsculas) nomeiam `C:\repo\x`. Um argumento de shell ou `workdir` que o comparador não possa
posicionar torna o comando somente metadados: um caminho relativo à unidade (`C:docs\x`, ou um
`C:` isolado), que o Windows resolve a partir do diretório atual daquela própria unidade, e
um caminho de dispositivo que não nomeia unidade ou compartilhamento (`\\?\GLOBALROOT\Device\...`,
`\\.\HarddiskVolume3\...`, `\\?\Volume{...}\...`). Uma flag como `/s`, uma URL
e um caminho comum continuam sendo posicionáveis. Um marcador no Windows não pode conter um padrão
absoluto POSIX (`/c/repo/docs/**`, `/docs/**`): nenhum candidato do Windows poderia
corresponder a ele, portanto o marcador é inválido e falha de forma fechada; declare-o relativo ao
marcador ou com uma unidade (`C:/repo/docs/**`). Nada disso se aplica a uma sessão POSIX, onde `/c/x`, um
ponto final ou dois-pontos faz parte do nome.

As exclusões de um checkout protegem seus arquivos onde quer que uma chamada seja feita. Para
todo alvo de ferramenta de arquivo (para `apply_patch`, os arquivos indicados pelos cabeçalhos `*** Add File:`,
`*** Update File:`, `*** Delete File:` e `*** Move to:`, lidos
após espaços em branco iniciais conforme o Codex os lê), todo diretório, arquivo e
argumento glob semelhante a caminho de uma ferramenta de busca/listagem (Grep, Glob, LS, `grep_search`,
`glob`, `list_directory`, `search_file_content`, `search_files` do Hermes com seu
filtro `file_glob`, e os equivalentes de outros agentes; um `pattern` de grep é uma
expressão regular e não é lido), e todo argumento de shell que represente um
caminho (um com um separador, após a expansão de `~/`) mais o `workdir` da ferramenta, o hook nativo e as
integrações geradas encontram o marcador que rege aquele local, pela
mesma busca lexical que a do próprio evento, e quando for de outro checkout (ou o
diretório do evento não tiver nenhum), aplicam a política daquele marcador também: `cat
../other/secrets.env` ou um Grep de `../other/secrets.env` a partir de um checkout
irmão, ou uma chamada de terminal do Hermes a partir de um diretório sem marcador, é descartada quando
`other` ignorar `secrets.env` (uma busca que atinja um checkout com uma política
ativa é descartada, como em seu próprio checkout), e um marcador corrompido lá torna a
chamada somente metadados. Cada diretório é consultado uma vez, e o que uma consulta
descobre sobre um diretório atende a todos os outros locais abaixo dele, de modo que muitos caminhos
em uma árvore profunda custam uma verificação por diretório. Uma chamada nomeando mais de 4.096
locais de caminho ou 16 outros marcadores é somente metadados, assim como uma ferramenta de arquivo
cujo destino não possa ser lido ou resolvido. As consultas compartilham o orçamento
da travessia de globs abaixo (50 ms e no máximo 16.384 verificações de arquivo); os diretórios
iniciais de um glob são consultados antes da travessia abaixo deles. Locais fora do
próprio checkout do evento são consultados primeiro, e esgotar o orçamento lá torna
a chamada somente metadados; dentro do próprio checkout a própria política do evento já se
aplica, portanto esgotar lá encerra a consulta e a chamada mantém sua
disposição.

Um local com sintaxe de glob em um nome de diretório (`cat ../*/secrets.env`, `cat
*/secrets.env` a partir de uma pasta pai, `../{a,b}/x`, `../[ab]/x`)
é consultado a partir de seus diretórios literais iniciais. Quando estes estiverem fora do
próprio checkout do evento (o diretório contendo `.git`), todo diretório pelo qual o glob
pudesse passar é verificado quanto a um marcador próprio: um segmento `*`/`?` lista
os diretórios filhos cujos nomes ele corresponde (ignorando maiúsculas/minúsculas ASCII), um segmento de classe,
chave ou grupo lista todos os filhos, e um `**` lista toda a subárvore,
exceto o próprio checkout do evento, de modo que `ls ../**/README.md` não lista o
checkout no qual é executado. Um glob cujos diretórios iniciais estejam
dentro do próprio checkout do evento não é percorrido, de modo que `cat packages/*/package.json`
em um monorepositório não custa nada, não importando quantos pacotes ele possua. Um `.git` em ou acima
do seu diretório pessoal (um repositório de dotfiles) ou na raiz do sistema de arquivos não
torna aquele diretório o próprio checkout para essas regras, de modo que `cat */secrets.env`
a partir de `~/Projects` ainda é verificado contra os checkouts que alcança. Links simbólicos
nunca são seguidos. A travessia lê no máximo 32.768 entradas de diretório, visita no
máximo 256 diretórios e é executada por no máximo 50 ms por chamada; além de qualquer limite a chamada
é somente metadados, assim como sintaxe de glob que possa sair de seus diretórios iniciais:
uma chave ou grupo que abranja um separador, uma alternativa `..` ou `~`, uma expansão de
chaves que construa uma, ou um nome de diretório construído por chaves que um bash mais antigo
expande para `..` (`.{*,x}/`). Em um argumento de shell, um `(` ou `{` que o shell não
expande é texto, não sintaxe de glob: um dentro de aspas ou com escape
(`awk '{print $1/2}'`, `jq '(.a // "x")'`, `echo 'a/{b'`), ou em um elemento de
um vetor de argumentos que nenhum shell executa. Chaves entre aspas que ainda listem
alternativas (`rg -g '{a/x,b}'`) são consultadas uma vez por alternativa também,
já que um programa que processa globs em seu próprio argumento as expande. Uma expansão de chaves grande demais para listar (mais de 64
palavras) conta como saída apenas quando suas partes puderem construir tal palavra; sequências
de números inteiros e letras não podem, portanto `touch test/file{1..200}.txt` e `cat
data/{a..h}{1..9}.csv` são percorridos como qualquer outro glob. Um `..` após um `**`,
e um segmento de diretório dot-glob que um bash mais antigo possa expandir para `..` (`.*`,
`.?`, uma classe como `.[[:punct:]]`, ou um extglob como `.@(.)` ou
`.+(.)`), são consultados em ambas as leituras, e um grupo de glob do zsh ou um escape
numérico ANSI-C é consultado conforme o shell o expande, inclusive dentro de um
script com aspas duplas (`bash -c "cat \$'…'"`, `eval "…"`), cuja barra invertida
antes de `$` o shell externo remove. Um servidor mais antigo descarta esses eventos de leitor de saída e shell
somente metadados, portanto atualize o servidor antes dos clientes. Excluir conteúdo antes do transporte
é fundamental porque impede que ele alcance observações/FTS, páginas de sessão, handoffs,
solicitações de revisores, propostas ou logs.

Este é um limite lexical de captura, **não um DLP completo**. Ele não resolve
links simbólicos, junções, montagens vinculadas (bind mounts) ou aliases 8.3 do Windows. Comandos de shell são
comparados apenas lexicalmente (acima), e um patch é lido apenas quanto aos seus cabeçalhos de arquivo; prompts,
texto do assistente, notificações e conteúdo entre aspas não são atribuíveis a caminhos.
Uma cópia do conteúdo de um arquivo sob outro caminho também não é vinculada de volta a ele.
Os arquivos de saída de harness listados acima são lidos de volta como somente metadados sob uma
política; para descartar completamente essas releituras, adicione por exemplo
`"~/.claude/projects/**/tool-results/**"` a `ignore_paths` (para cada arquivo,
não apenas os ignorados). Adicione cada alias visível relevante
explicitamente e não confie neste recurso para detectar todas as formas como conteúdo privado
pode ser mencionado.

<a id="capture-exclusion-residuals"></a>Estas lacunas são inerentes à correspondência lexical e permanecem abertas:

- variáveis de shell (`$VAR`) não são expandidas, e a saída de uma substituição
  de comando não é conhecida (suas próprias palavras são lidas);
- um `cd` dentro de um comando não é acompanhado (o próprio `workdir` da ferramenta é);
- links simbólicos não são resolvidos, por design;
- uma busca recursiva de um diretório pai por meio de um shell (`grep -r x docs`,
  `rg x`) não indica nenhum caminho ignorado, portanto sua saída é mantida;
- um script lido de um arquivo (`bash run.sh`, `python3 tool.py`) não é lido,
  e um heredoc fornecido a um interpretador (`python3 - <<EOF`) é lido apenas para as
  palavras que ele explicita;
- o git resolve `REV:path` a partir da raiz do repositório, que é aproximada pelo
  diretório do marcador e pelo `cwd`, e uma opção de diretório da própria ferramenta
  (`git -C dir`, `make -C dir`) não é acompanhada;
- executores de scripts e interpretadores não listados pelo comparador (`awk` com
  `getline`, `watch`, `flock … -c`, `su user -c`) são lidos apenas para as palavras
  que eles explicitam: um caminho ignorado indicado lá ainda descarta o evento, mas
  expansão de chaves ou código inline dentro deles não é reconhecido;
- uma palavra avulsa nomeando o diretório de outro checkout (`grep -r x other`) não é
  consultada quanto ao marcador daquele checkout, apenas argumentos que contenham um separador
  de caminho o são; uma busca recursiva de um diretório literal (um Grep cujo
  `path` seja `..`) não entra nos checkouts abaixo dele, um glob cujos
  diretórios iniciais estejam dentro do próprio checkout do evento não é percorrido, de modo que um
  checkout aninhado dentro dele só é alcançado por um caminho que o nomeie, e um
  glob não é seguido através de um diretório com link simbólico; uma expansão de parâmetro
  junto à sintaxe de glob (`${X}/*/x`) não é percorrida; um arquivo de saída de harness mantido em um
  local diferente dos padrões listados acima (um diretório de configuração
  do Claude Code cujo nome não contenha `claude`) é lido como um arquivo comum;
- um glob de saída salva conta apenas quando contiver três caracteres do
  nome característico do arquivo de saída em sequência, de modo que um glob sem essa sequência literal
  (`/tmp/p?-b*`) seja lido como um glob comum; exigir o texto literal é
  o que mantém `cat /tmp/*.log` e `tail log/*.log` comuns;
- o próprio checkout é definido após a execução do comando, de modo que um comando que cria
  um `.git` (`touch .git; cat */x`) torne seu diretório o próprio checkout e seus
  globs não sejam percorridos; um `.git` no ou acima do diretório pessoal nunca conta,
  mas um em qualquer outro diretório conta;
- um marcador aninhado dentro do próprio checkout é encontrado apenas enquanto o orçamento de consulta
  durar (os caminhos mais rasos são consultados primeiro); além dele, a
  própria política do evento rege os caminhos restantes;
- um valor anexado a uma flag curta é lido apenas após a primeira letra da flag,
  não após um agrupamento (`-rf…`), e escapes percentuais não ASCII de uma URL `file:`
  não são decodificados;
- OMP e Pi resolvem o caminho de uma ferramenta de arquivo de maneiras que o comparador não segue:
  ambos expandem `~` e normalizam espaços Unicode antes de abri-lo, o Pi no
  Windows lê um `/c/x` do Git Bash como `C:\x`, e o OMP divide um caminho de `read`
  que não indica nenhum arquivo em vírgulas, ponto e vírgula ou espaços em branco em vários arquivos,
  portanto `ignore_paths` vê apenas o caminho como grafado, suas partes antes de um `:` ou
  `?`, e as reescritas de `:`, `@` e `file://` acima;
- em uma sessão Windows, o `workdir` de uma ferramenta de shell grafado como `/c/x` é lido apenas
  como o Git Bash o lê (`C:\x`), portanto argumentos resolvidos a partir dele perdem a leitura de uma
  ferramenta nativa (`D:\c\x` a partir de um `cwd` em `D:`); um caminho de ferramenta de arquivo ou argumento de shell
  grafado dessa forma é lido de ambos os modos;
- as integrações geradas em TypeScript verificam a tabela `[capture]` tão
  estritamente quanto o hook nativo e rejeitam erros estruturais do marcador
  em qualquer lugar do arquivo (uma linha que não seja cabeçalho nem par chave/valor,
  um array ou tabela inline deixada aberta), mas não validam o TOML de outros
  valores (um `x = nope` sem aspas, uma chave duplicada em outra tabela), de modo que tal
  erro invalida a política apenas no hook nativo;
- no Windows, apenas `localhost` e `127.0.0.1` são lidos como a máquina local:
  outro loopback ou alias de host para um compartilhamento administrativo (`\\127.0.0.2\c$`,
  `\\127.1\c$`, `\\<hostname>\c$`) ou um caminho `\\wsl$\...` é comparado como o
  caminho UNC que ele representa, não como a unidade que ele alcança (compartilhamentos administrativos
  normalmente exigem um processo elevado);
- a correspondência no Windows dobra apenas maiúsculas/minúsculas ASCII, portanto um padrão e um caminho que diferem
  no caso de uma letra não ASCII (`É` e `é`) não coincidem no Windows;
- em uma sessão Windows, um argumento relativo à unidade conta como posicionado quando o
  mesmo comando também contiver um caminho posicionável cuja leitura no shell POSIX (cada
  barra invertida escapando o caractere seguinte) o represente, do modo como o Git Bash lê
  `C:\repo\x` como `C:repox`; e uma palavra inofensiva no formato `X:y`
  (`sed s:a:b:`) ou um caminho de dispositivo como `\\.\NUL` ou `\\.\pipe\x` torna um
  comando Windows somente metadados;
- ferramentas que o comparador não lista mantêm seu comportamento atual.

Para um projeto cujo conteúdo nunca deva ser capturado, use o modo de captura por lista de permissões
(acima) e deixe aquele repositório sem um marcador.

### Integrações compatíveis e atualização

A política de captura v1 é aplicada pelos comandos nativos `ai-memory hook` (incluindo
comandos nativos de hook POSIX/Windows) e pelas integrações geradas do OpenCode, OMP, Pi e
OpenClaw, incluindo a correspondência lexical de comandos de shell acima.
Instaladores locais adotam como padrão comandos nativos onde esse
caminho for aceito. O caminho comum de `install-hooks` do wrapper Docker para Linux/macOS
usa seu cliente de host nativo com soma de verificação verificada e também é compatível.
Hooks legados `.sh`/`.ps1`, substituições explícitas de compatibilidade e
pacotes de script Docker manuais/somente remotos **não** a aplicam. Reinstale hooks
ou atualize/reinstale plugins gerados após a atualização; hooks/plugins existentes
mantêm seu comportamento anterior. A saída de recursos do instalador descreve a
integração selecionada.

Novos clientes continuam seguros com servidores antigos porque a remoção e o descarte ocorrem
no cliente. Clientes antigos comunicando-se com novos servidores retêm o comportamento antigo e não podem
aplicar uma política de marcador restrita ao host; o servidor não pode avisar sobre uma política que nunca
viu. A política não adiciona ferramenta MCP nem migração de banco de dados.

### Verificar uma decisão localmente

`ai-memory hook --event ... --agent ... --check-capture` lê um payload JSON
da entrada padrão (stdin) e não executa nenhum trabalho de spool, fila, escoamento, rede ou handoff. Ele
imprime apenas metadados de decisão delimitados (versão do protocolo, estado da política, família
de ferramentas, contagem de caminhos, disposição e estado de extração), nunca caminhos, padrões
ou conteúdo do payload:

```bash
printf '%s\n' '{"session_id":"demo","cwd":"/example/workspace","tool_name":"Edit","tool_input":{"path":"docs/example.md"}}' \
  | ai-memory hook --event post-tool-use --agent claude-code \
      --server-url http://127.0.0.1:49374 --check-capture
```

A mesma inspeção também relata dicas saneadas de `scope` (`workspace`, `project`,
`project_src`, `project_strategy`, `identity`, `identity_src`,
`identity_style`, `server_may_remap`) e `scope_resolution`. Estas são dicas locais, não uma
consulta ao servidor: a inspeção não cria projeto e pode relatar coordenadas derivadas do
servidor que o servidor remapeie posteriormente. Escopo parcial de marcador falha de forma fechada.
`event_admits_capture` relata elegibilidade no ciclo de vida. Produtores externos usam
`policy_admits_capture`, que também verifica exclusões e resolução de perfil de
servidor enquanto ignora `AI_MEMORY_CAPTURE_OWNER`. O campo existente
`admits_capture` ainda relata a admissão de captura nativa. Cada dica de escopo
é limitada a 512 bytes. Nomes superdimensionados são omitidos (`null`), relatam
`scope_resolution: "unavailable"` e recusam a admissão do produtor; o roteamento
nativo permanece inalterado.
O hook nativo pode derivar um projeto para um marcador que declare apenas
`workspace`. A pré-verificação do produtor é mais estrita: declare ambos os nomes, ou forneça
`repo-root` para que ambos sejam resolvidos localmente. Um cwd ausente também retorna escopo
indisponível e recusa a admissão do produtor.

O contrato normal de captura é intencionalmente restrito: eventos de ferramentas compatíveis do Claude Code,
OpenCode, Pi, OMP e Antigravity retêm apenas a família de ferramentas canônica,
um ID de chamada validado fornecido pelo agente quando seu esquema documentado comprovar um,
e uma classe de resultado PostToolUse. `PreToolUse` nunca retém comandos,
argumentos, caminhos, corpos de entrada ou nomes arbitrários de ferramentas. O único nome mantido é o de
uma ferramenta MCP: uma chamada nomeada `mcp__<server>__<tool>` (Claude Code, Codex, Hermes
Agent) permanece na família `unknown`, sem entrada ou saída, e adiciona
linhas `mcp_server` e `mcp_tool` quando o nome inteiro tiver no máximo 64 letras ASCII,
dígitos, `_` ou `-`. `PostToolUse` anexa
seu trecho existente de resposta/erro da ferramenta e limita o corpo renderizado completo a
2.000 bytes seguros em UTF-8. Eventos de edição de arquivo bem-sucedidos do Antigravity recorrem ao
campo delimitado de substituição ou conteúdo gravado em `toolCall.args` porque seu
payload de hook não inclui um campo de saída; edições que falharam retêm o erro
em vez disso. O fallback é exclusivo do Antigravity e é descartado com qualquer evento
rejeitado pelas exclusões de captura.
Invólucros de ferramentas incompatíveis não ganham um corpo PreToolUse,
e a associação se dá apenas por IDs de chamada correspondentes fornecidos pelo agente. User-prompt armazena seu texto
de prompt a menos que os hooks do Claude Code tenham sido instalados com `--no-capture-prompts`;
notification armazena sua mensagem/texto, e post-compaction armazena seu
resumo; corpos de outros eventos atualmente são vazios a menos que expressamente aceitos.
A captura de mensagens do assistente/Stop é desativada por padrão e nunca é persistida; ela está
disponível apenas por meio da autorização dupla explícita (double opt-in) descrita no guia de instalação
(`install-hooks --capture-assistant` no cliente mais `capture_assistant` no
servidor), onde o trecho é saneado em ambos os lados e limitado. Ela não é
controlada por este arquivo marcador: o texto do assistente não é atribuível a caminhos, portanto um
`.ai-memory.toml` não pode restringi-lo. O cabeçalho de metadados é fechado; o
trecho de resposta/erro de PostToolUse permanece a captura de conteúdo delimitada existente.
Exclusões de captura são avaliadas apenas onde caminhos possuem um esquema comprovado, de modo que não
afirmam filtrar esses outros corpos.

## Quatro exemplos canônicos

### Múltiplos clientes

```
~/projects/movvia/.ai-memory.toml     → workspace = "movvia"
~/projects/cliente-x/.ai-memory.toml  → workspace = "cliente-x"
~/personal/.ai-memory.toml            → workspace = "personal"
```

Resultado:

- `~/projects/movvia/pe-api-core` → workspace = `movvia`, project = `pe-api-core`
- `~/projects/cliente-x/api`      → workspace = `cliente-x`, project = `api`
- `~/personal/blog`               → workspace = `personal`, project = `blog`

### Monorepositório com pacotes agrupados

```
~/projects/movvia/.ai-memory.toml              → workspace = "movvia"
~/projects/movvia/pe-portais/.ai-memory.toml   → workspace = "movvia"
                                                  project   = "pe-portais"
```

Resultado:

- `~/projects/movvia/pe/pe-api-core`        → workspace = `movvia`, project = `pe-api-core`
- `~/projects/movvia/pe-portais/apps/web`   → workspace = `movvia`, project = `pe-portais`
  (marcador mais próximo prevalece)

### Worktrees do Git / estratégia repo-root

```
~/projects/.ai-memory.toml → workspace        = "oss"
                            → project_strategy = "repo-root"
```

Resultado:

- `~/projects/ai-memory`                → workspace = `oss`, project = `ai-memory`
- `~/projects/ai-memory/crates/cli`     → workspace = `oss`, project = `ai-memory`
- `~/projects/ai-memory-feature-branch` → workspace = `oss`, project = `ai-memory`

Se o marcador estiver dentro do checkout principal (por exemplo
`~/projects/ai-memory/.ai-memory.toml`), copie-o ou comite-o em cada
worktree fora da árvore, ou coloque um marcador compartilhado acima do diretório
pai das worktrees, como mostrado aqui.

Sem `project_strategy = "repo-root"`, um remoto válido ainda atribui a cada
worktree o nome padrão canônico do caminho do repositório. Apenas caminhos sem remoto
resolvem pelo nome base de seu diretório atual.

A resolução ocorre no lado do host: hooks de ciclo de vida e plugins
gerados em TypeScript seguem o ponteiro commondir da worktree (`git rev-parse
--git-common-dir`, ou o mesmo utilitário Rust/libgit2 para hooks nativos) até
o repositório principal e enviam o nome resolvido como um `project` explícito.
Isso significa que funciona mesmo quando o diretório da worktree estiver **fora**
da árvore do repositório principal (algumas ferramentas mantêm worktrees em um diretório separado,
de modo que a worktree não possui um ancestral `.ai-memory.toml` próprio) e mesmo
quando o servidor executa em um contêiner que não pode ver o checkout do host.
Coloque o marcador em qualquer lugar no caminho ascendente a partir da worktree (comumente
um único `~/.ai-memory.toml`) para selecionar a estratégia.

### Identidade do repositório

O nome padrão de um projeto com suporte a remoto agora provém de seu caminho de repositório
normalizado, enquanto pastas sem remoto ainda usam seu nome base. Nomes de pastas colidem:
dois repositórios não relacionados, ambos clonados como `api/`, de outro modo compartilhariam um único
projeto. Em um servidor com concessões por projeto (#708) isso significa uma concessão, portanto
todo cliente resolve uma **identidade de repositório** para o checkout e a envia
com cada evento. O primeiro nível que fornecer uma prevalece:

1. `identity = "…"` no marcador;
2. `project = "…"` no marcador;
3. o remoto git `upstream`, caso contrário `origin`, normalizado
   (`git@github.com:Acme/API.git` → `github.com/acme/api`);
4. o nome da pasta.

Apenas os níveis 1 e 3 alteram o roteamento. Um `project` declarado roteia por nome como
sempre fez, porque uma declaração tem precedência sobre o remoto: um fork cujo marcador nomeia
seu próprio projeto nunca é arquivado sob o repositório do qual foi originado. Um nome de pasta
puro roteia exatamente como antes. O que roteia por identidade é um
checkout não declarado com um remoto, ou um checkout com uma
`identity` explícita:

- O projeto que já possui a identidade prevalece, qualquer que seja seu nome:
  `~/work/api` e `~/dev/acme-api`, ambos clonados de `github.com/acme/api`,
  são um único projeto.
- Um projeto existente com aquele nome e sem identidade ainda é **reivindicado no
  lugar** pela primeira captura que possa gravar nele, de modo que a atualização não move
  nenhuma memória. Uma worktree vinculada (`git worktree add`) reivindica o projeto nomeado
  conforme a pasta de seu checkout principal, de modo que não importa qual checkout
  captura primeiro. O servidor localiza o checkout principal a partir dos metadados git
  da worktree, portanto isso necessita de um servidor capaz de ler o checkout.
- Se o nome já pertencer a uma identidade diferente, o novo repositório
  recebe seu próprio projeto, nomeado a partir de seu proprietário (`github.com/orgb/api` →
  `orgb-api`, depois `orgb-api-2`, …).

O remoto é normalizado no host, portanto credenciais embutidas em uma URL
remota nunca saem da máquina. Outros nomes de remotos (`fork`, `mine`) são
ignorados intencionalmente: eles variam por pessoa e dariam a um mesmo repositório uma
identidade diferente para cada uma delas.

#### Nomear projetos pelo caminho do repositório (`identity_style`)

Por padrão, um checkout não declarado com um remoto `upstream` ou `origin` válido é
nomeado a partir de todo o caminho do repositório no remoto sem seu host (#1033):

```toml
identity_style = "host_path"   # explicit pre-v3 compatibility opt-out
```

Com o estilo padrão `path`, um projeto criado a partir de um remoto é nomeado a partir do caminho
completo do repositório sem o host, com `/` grafado como `-`
(`git@github.com:acme/api.git` → `acme-api`,
`https://gitlab.com/acme/group/api.git` → `acme-group-api`), de modo que toda
worktree e clone concordem quanto ao nome. Apenas o nome muda: capturas ainda
são roteadas pela identidade completa (`github.com/acme/api`), e o estilo é transmitido
apenas com uma identidade remota. Um `project` ou `identity` declarado mantém seu
próprio nome.

- **Projetos existentes mantêm seu UUID.** Leituras pelo nome de caminho canônico ou
  pelo nome base v2 destinam-se apenas à localização e nunca renomeiam. Uma captura autorizada
  no estilo padrão ou gravação explícita pelo nome de caminho canônico promove apenas
  `projects.name` em uma única transação; páginas, sessões, concessões, handoffs e
  todas as outras linhas dependentes permanecem anexadas ao mesmo UUID. O nome base v2
  continua legível durante toda a v3.
- **Outra forja nunca é mesclada.** Quando o nome do caminho já estiver ocupado por
  um repositório diferente (normalmente o mesmo caminho em outra forja,
  `gitlab.com/acme/api` depois de `github.com/acme/api`), o recém-chegado recorre
  ao nome que receberia sem o estilo.
- Aliases locais de marcadores e rotas de identidade/caminho da home do operador mantêm sua
  precedência documentada sobre o padrão e permanecem disponíveis para migração ou
  remapeamento deliberado.
- Clientes estáticos de CLI/MCP continuam passando apenas `workspace` + `project`. A CLI leve
  deriva o nome de caminho canônico quando nenhuma rota de marcador/home a sobrepõe; outros
  clientes estáticos devem usar o mesmo nome a partir de `upstream`, depois `origin`. Um remoto
  válido prevalece sobre `project_strategy = "repo-root"`; a estratégia aplica-se apenas quando
  nenhum remoto válido existir. Eles podem usar
  o nome de caminho canônico (`acme-api`) ou o nome base v2 (`api`) enquanto for
  inequívoco. Uma chave sem host compartilhada por múltiplas forjas falha de forma fechada.
- `ai-memory doctor` combina o marcador local efetivo e evidências normalizadas do
  repositório com o diagnóstico de coordenadas somente leitura do servidor. Ele
  informa compatibilidade exata/canônica/legada, status ausente ou ambíguo,
  elegibilidade para renomeação e motivos de colisão. Ele analisa o marcador de roteamento uma vez,
  honra flags de escopo da CLI antes dos nomes do marcador e relata apenas uma classe segura de
  status/origem do marcador; TOML de marcador inválido é ignorado sem ecoar seu
  conteúdo, caminhos brutos do marcador nunca são renderizados e credenciais remotas nunca
  saem do cliente. Um resultado ambíguo pode nomear o UUID/nome atual do candidato com suporte
  a identidade preferencial para contexto, mas nunca é uma resolução única ou
  resultado elegível para renomeação.
- O `_meta.md` do projeto indexado por UUID persiste os campos opcionais `identity`,
  `identity_source`, `canonical_name` e `legacy_name`. Um `reindex`
  limpo restaura o UUID/nome original e recalcula as chaves indexadas
  a partir da identidade validada. Manifestos mais antigos sem esses campos permanecem
  válidos e reconstroem um projeto sem identidade.

### Único espaço de trabalho, sem sobreposições por repositório

```
~/.ai-memory.toml → workspace = "home"
```

Todo cwd sob `$HOME` recai no espaço de trabalho `home`; um remoto válido usa seu
nome de caminho canônico, enquanto um repositório sem remoto usa `project = basename(cwd)`.
Útil quando você simplesmente deseja sair do
agrupamento `default` por completo.

## Migrar projetos existentes

Projetos já criados sob o espaço de trabalho `default` permanecem lá. Mova um para um
espaço de trabalho diferente com a CLI:

```sh
ai-memory move-project \
    --from-workspace default --project foo \
    --to-workspace movvia --confirm
```

## Padrão para toda a instalação (sem marcador)

`project_strategy = "repo-root"` normalmente reside em um marcador, o que significa
adicionar um `.ai-memory.toml` em (ou acima de) cada repositório. Para obter a mesma
resolução por raiz de repositório para toda uma instalação **sem** um marcador por repositório, incorpore-a
aos hooks gerados no momento da instalação:

```sh
ai-memory install-hooks --apply --agent claude-code --project-strategy repo-root
```

Toda sessão para essa instalação então resolve seu projeto a partir da raiz principal
do repositório git, de modo que um agente que execute `mkdir sub && cd sub` e permaneça lá não
mais bifurque o restante da sessão para um projeto fantasma chamado `sub`.

Esta é uma **configuração em tempo de instalação**, gravada no comando de hook do agente (e
nos plugins gerados do OpenCode / OMP / Pi / OpenClaw), com o mesmo status que a
`AI_MEMORY_AUTH_TOKEN` / `AI_MEMORY_HOOK_URL` ao lado da qual se posiciona, e *não* uma sobreposição
em tempo de execução definida pelo usuário (o que foi deliberadamente rejeitado na issue #16). A flag aceita
`basename` (o padrão de novas instalações, que não embute nada) ou `repo-root`. Um comando posterior
`install-hooks --apply` sem a flag preserva o valor já embutido nos
hooks do ai-memory; passe `--project-strategy basename` explicitamente para removê-lo.

A precedência permanece inalterada: o `project_strategy` ou `project` explícito de um marcador
continua prevalecendo sobre o padrão da instalação.

## Navegação no meio da sessão: `[routing] mid_session`

Tudo acima decide onde uma sessão *começa*. Uma sessão longa também se desloca:
um agente executa `cd` para um diretório temporário, ou para um checkout irmão para
pesquisar uma referência. `[routing] mid_session` no `config.toml` decide como esses
eventos do meio da sessão são atribuídos. Trata-se de uma configuração de tempo de execução no lado do servidor, e não
uma chave de marcador.

```toml
[routing]
mid_session = "follow-cwd"   # default
# mid_session = "sticky"
```

- **`follow-cwd`** (padrão, comportamento histórico) resolve novamente cada
  evento do meio da sessão a partir de seu próprio cwd. Um `cd` para um checkout irmão registra
  essas observações no projeto daquele checkout, de modo que o registro bruto de uma única sessão
  fica dividido entre dois projetos, enquanto sua linha de sessão e página compilada permanecem
  no primeiro.
- **`sticky`** mantém o projeto da sessão onde quer que o agente navegue. Isso
  corresponde ao modelo que o restante do sistema já adota: `sessions.project_id`
  contém exatamente um valor, e a consolidação lê por `session_id` e grava
  uma única página no projeto da sessão. Escolha este modo quando uma sessão de agente significar
  um único projeto.

Duas garantias valem em **ambos** os modos:

- **Um marcador ainda prevalece.** Um `.ai-memory.toml` que nomeia um projeto é uma
  mudança deliberada de escopo, não desvio acidental, portanto nunca é anulado. O hook informa ao
  servidor qual tipo de sobreposição enviou (`project_src=marker` versus
  `project_src=repo-root`), que é o que permite ao modo `sticky` anular um nome
  derivado enquanto honra um nome declarado. Um cliente anterior à v1.27 não envia
  proveniência, e suas sobreposições permanecem autoritativas.
- **Âncoras amplas nunca se tornam fixas (sticky).** Uma sessão originada em `/` ou em `$HOME` não é
  uma âncora significativa, portanto nunca captura eventos abaixo dela. Caso contrário, uma
  sessão perdida iniciada em `$HOME` agruparia todos os projetos em um único
  balde.

Eventos de criação de sessão não são afetados em ambos os modos: abrir uma sessão em uma
pasta comum fora do git ainda nomeia o projeto com base nessa pasta.

Alguns harnesses relatam um cwd diferente de onde uma ferramenta foi executada: um subagente mantém o
cwd de sua sessão pai enquanto usa uma ferramenta de arquivo em outro checkout, e o Hermes
Agent sempre relata o diretório de seu próprio processo (normalmente `~`) enquanto cada chamada de ferramenta
nomeia seu repositório por meio de `workdir` ou `path`. Comandos nativos `ai-memory hook`
compensam isso antes de aplicar `follow-cwd` ou `sticky`: o local absoluto de uma chamada de ferramenta
(os caminhos de destino de uma ferramenta de arquivo comprovada por fixture, o `path` explícito de uma ferramenta
de busca/listagem, ou o `workdir` estruturado de um comando de shell) roteia o
evento quando todos os caminhos comprovarem o mesmo repositório ou limite de marcador. O
marcador daquele destino decide a admissão na lista de permissões, e sua política de captura e
perfil de `server` se aplicam, de modo que uma chamada entre projetos não pode usar as credenciais
do repositório de origem. As exclusões da origem ainda se aplicam: se o próprio `ignore_paths`
do cwd do payload descartasse o evento (um padrão pode nomear um caminho
fora de seu checkout, e um comando pode nomear um arquivo de origem enquanto seu `workdir`
aponta para outro lugar), o evento não é rerroteado e essa exclusão o descarta.
Caminhos relativos e workdirs, chamadas de projetos mistos, esquemas desconhecidos e caminhos
absolutos fora de um repositório/marcador reconhecido mantêm o cwd do payload. Texto livre de
comando de shell nunca é reinterpretado como uma instrução de roteamento de projeto;
apenas o campo estruturado `workdir` conta. Isso altera apenas a atribuição da
observação bruta; a linha da sessão e sua página compilada de sessão permanecem no
projeto onde a sessão começou.

Antes dessa consulta, um evento do Hermes adota `TERMINAL_CWD` (o `terminal.cwd`
da sessão) no lugar do diretório do processo. Um gateway Hermes multiplexador
exporta apenas o `terminal.cwd` de seu perfil padrão, de modo que um evento de gateway
de um perfil nomeado roteia, em vez disso, por um marcador na própria home Hermes daquele perfil,
`~/.hermes/profiles/<name>/.ai-memory.toml` (sob o `HERMES_HOME` do gateway
quando definido). É um marcador local normal: `workspace`, `project`,
`[capture]` e `server` se aplicam. Sem ele, o evento mantém o diretório
do gateway. Veja [install.md](install.md#hermes-agent-nous-research).

Independentemente desta configuração, sob `project_strategy = "repo-root"` um
evento no meio da sessão cujo cwd esteja fora de qualquer repositório git *e* de qualquer marcador (diretórios
temporários do agente, `/tmp`, pastas de dados) já herda o projeto da sessão
em vez de criar um projeto fantasma chamado `scratchpad` ou `data`.
O hook do host resolve repo-root por conta própria, portanto uma sobreposição ausente já comprova
que o cwd resolveu para nada.

### Reinformar em uma mudança de projeto: `[routing] brief_on_project_change`

`mid_session` decide onde as observações de uma sessão errante são *registradas*,
mas o resumo da sessão — páginas fixadas, `_rules/`, `_slots/` — é injetado apenas
uma vez, no início da sessão, para o projeto ao qual o cwd inicial resolveu. Uma
sessão que começa um nível acima (digamos `~/projects`, que não tem marcador) e
depois trabalha dentro de `~/projects/acme/site` (que tem seu próprio marcador)
nunca vê as regras desse projeto.

`[routing] brief_on_project_change = true` (padrão `false`) fecha essa lacuna para
o Claude Code. Quando o cwd de um prompt resolve **através de um marcador** para um
projeto diferente do da própria sessão, e esse projeto ainda não recebeu resumo
nesta sessão, o servidor retorna seu resumo central — as mesmas páginas fixadas,
`_rules/` e `_slots/` que um início de sessão injeta, dentro do mesmo orçamento —
como contexto adicional naquele prompt.

```toml
[routing]
brief_on_project_change = false   # padrão; true para optar por ativar
```

É deliberadamente restrito:

- **Somente marcador.** Um cwd simples que resolve por raiz git ou nome de pasta
  nunca reinforma, a mesma regra que `mid_session` aplica para reescopar. Apenas
  um `.ai-memory.toml` que nomeia um projeto conta (`project_src=marker`).
- **Somente um projeto diferente.** Um prompt que permanece no próprio projeto da
  sessão não pede nada.
- **Uma vez por projeto por sessão.** O servidor registra quais projetos uma
  sessão já recebeu resumo (limitado e em memória, com um TTL de 12 horas);
  perder esse registro só pode repetir um resumo, nunca pular o primeiro.
- **Somente Claude Code.** Ele acompanha o `UserPromptSubmit` do Claude Code; um
  payload do Cursor, que executa as mesmas configurações de hook, é excluído.

A opção de ativação, o teste de diferença em relação à sessão e o registro de uma
vez por projeto vivem todos no servidor. O hook só pergunta quando o roteamento
por marcador de um prompt mudou desde o último sobre o qual perguntou, de modo que
uma sessão que fica parada não envia requisição extra.

## Quem lê o marcador

Ambos os pontos de entrada, a partir da v1.20:

- **Hooks de ciclo de vida** encaminham os campos do marcador para o servidor a cada
  evento, de modo que as capturas de sessão fiquem no escopo declarado.
- **Comandos de CLI do cliente** resolvem `(workspace, project)` localmente antes
  de chamar o servidor: `run`, `bootstrap`, `search`, `read-page`,
  `write-page`, `lint`, `curator`, `embed`, `pending-writes`,
  `forget-sweep`, `auto-improve`, `purge-project`, `rename-project`,
  `move-project` e `move-session --from-project` (lado de origem), e similares.

Antes da v1.20, apenas os hooks o liam. Um checkout que declarasse
`workspace = "acme"`, portanto, tinha suas capturas alocadas em `acme`, enquanto todo
comando da CLI resolvia para `default`: o mesmo repositório dividido entre dois
escopos, com o workstream gerenciado do `ai-memory run` no lado errado da
divisão.

Cada campo é resolvido independentemente:

1. A flag explícita (`--workspace` / `--project`).
2. O marcador mais próximo: `workspace`/`project`/`identity` explícitos; caso contrário, o nome
   canônico de caminho de um remoto `upstream`/`origin` válido prevalece, mesmo quando
   `project_strategy = "repo-root"` estiver definido. A estratégia seleciona a raiz do repositório
   principal apenas quando nenhum remoto válido existir.
3. Os fallbacks: espaço de trabalho `default` e o nome base do cwd atual. Um subdiretório
   git não implica repo-root sem uma estratégia explícita.

Quando o nível 2 decide um campo, o comando imprime uma linha no stderr indicando
o escopo resolvido, qual metade (ou metades) o marcador decidiu, e o
marcador que o decidiu:

```console
$ ai-memory search "scope resolver"
ai-memory: scope acme/api (workspace + project from /Users/dev/projects/acme/.ai-memory.toml)
```

`AI_MEMORY_IGNORE_MARKER=1` ignora o nível 2 para uma invocação, restaurando a
resolução anterior à v1.20 sem editar ou sair da árvore do marcador. Isso
se aplica **apenas a comandos do cliente**. Os hooks de ciclo de vida ainda encaminham os
campos do marcador em cada evento, de modo que uma invocação executada com essa variável definida resolve
em um escopo diferente das capturas de sessão ao seu redor. Use isso para
leituras pontuais, não como forma de realocar a memória de um repositório.

`ai-memory serve` é deliberadamente excluído: o servidor não possui um cwd de chamador a partir
do qual subir na árvore, e seus `--workspace` / `--project` são o fallback embutido para
eventos de hook que cheguem sem um cwd utilizável.

## O que o arquivo marcador NÃO faz

- Não usa padrões glob. Busca ascendente apenas por ancestralidade literal.
- Não mescla marcadores ancestrais. O mais próximo prevalece. (`server` é a única chave
  herdada do marcador mais próximo que a declarar; veja acima.)
- Não faz migração automática de projetos do espaço de trabalho `default`.
- Não faz colapso automático para repo-root. Worktrees e subdiretórios apenas
  compartilham um projeto quando `project_strategy = "repo-root"` for definido explicitamente
  (por marcador, ou embutido para toda a instalação; veja acima).
- Não aceita URL ou token no marcador. `server = "<name>"` seleciona um perfil
  de servidor registrado localmente com `ai-memory server add`; não pode introduzir um
  novo destino nem transportar uma credencial. Caso contrário, use as variáveis de ambiente existentes
  (`AI_MEMORY_AUTH_TOKEN`, `AI_MEMORY_HOOK_URL`). (Um *padrão* repo-root
  ainda pode ser embutido em uma instalação sem marcador via
  `install-hooks --project-strategy repo-root`, mas isso é configuração em tempo de
  instalação, não uma sobreposição em tempo de execução que o usuário define em seu shell.)
- Não ultrapassa o limite de confiança. A busca para em `$HOME`; um
  checkout fora dele para na raiz daquele checkout e necessita de um marcador dentro
  do checkout. Um diretório fora do git e fora de `$HOME` precisa de um marcador em seu
  cwd exato.

## Solução de problemas

**Meu marcador não está sendo identificado.** Verifique:

1. O arquivo chama-se exatamente `.ai-memory.toml` (observe o ponto inicial).
2. O arquivo está em um **ancestral** do cwd, não em um irmão ou
   descendente.
3. Não há um marcador mais próximo o sobrepondo. Execute
   `find ~/projects -maxdepth 5 -name '.ai-memory.toml'` para ver todos os
   marcadores em sua árvore.
4. Os valores de workspace / project correspondem à regex acima (alfanuméricos
   minúsculos, pontos, traços, sublinhados).
5. Se você usa `project_strategy`, seu valor é exatamente `repo-root`.

Scripts de hook são executados de forma assíncrona ("dispare e esqueça") por design, portanto não registram logs em caso
de sucesso. Para ver o que está sendo realmente enviado, execute um script de hook
manualmente:

```sh
printf '{"cwd":"%s"}' "$PWD" \
  | sh ~/.local/share/ai-memory/hooks/claude-code/post-tool-use.sh
```

Se o marcador estiver sendo lido, a linha do curl (visível com `set -x`
ou nos logs do servidor) incluirá `&workspace=...` na URL.
