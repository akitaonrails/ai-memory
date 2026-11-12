# Workstreams gerenciados entre harnesses

`ai-memory run` é um inicializador opcional que permite que uma sessão do agente de código
se mova entre Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro
CLI v2/v3, OMP, Grok Build CLI, Antigravity CLI e GitHub Copilot CLI. Inicializações diretas de agentes
mantêm seu comportamento existente no ai-memory. Não há alternância global de modo nem
comando `switch`: usar `run` seleciona o workstream atual e cria ou retoma de forma
transparente a sessão nativa correta para o harness solicitado.

No Linux, uma sessão com sandbox ai-jail do OpenCode usa um servidor `--standalone` privado por
padrão. Isso mantém os processos do seu servidor e ferramentas dentro do namespace de PID do
ai-jail, de forma que não possam sobreviver à jail. Um `--server` nativo explícito
ainda tem precedência, pois seleciona deliberadamente um serviço externo. Os nomes legados
`opencode2`, `opencode-v2` e `open-code2` forçam o adaptador V2 para scripts
antigos; eles são aliases de compatibilidade, não um segundo harness OpenCode.

Quando o servidor executa com `--enable-web`, o root pode inspecionar todos os ledgers gerenciados
em `/web/workstreams`. A página somente leitura mostra o estado de ciclo de vida confiável, o
harness/execução mais recente, links do harness atual, contagens de eventos e horários de atividade. Ela
não infere "aguardando entrada" a partir da prosa capturada e nunca exibe
caminhos de checkout, fingerprints, strings do proprietário da concessão (lease), IDs de sessão nativa, IDs de
registro de origem ou conteúdo de transcrição. Essa visualização de operador entre projetos não é uma
API de descoberta: o `ai-memory resume` e a seleção normal de execução permanecem restritos aos
fingerprints exatos de repositório/worktree descritos abaixo.

`ai-memory run` é a forma preferida de iniciar um harness: em caso de dúvida, execute
com o ai-memory. Além da continuidade da sessão, na primeira vez que ele inicia um determinado
harness, ele instala automaticamente os hooks e MCP do ai-memory para esse harness se eles
ainda não estiverem conectados, de modo que a captura e a recuperação funcionem sem uma etapa separada de `install-hooks`
/ `install-mcp` (uma armadilha comum: `ai-memory run kimi` costumava capturar
nada se os hooks do Kimi nunca tivessem sido instalados). A configuração automática é idempotente e
ocorre uma única vez por harness, versão do binário e local de instalação (um segundo diretório de configuração,
como outro `CLAUDE_CONFIG_DIR`, tem sua própria primeira inicialização; `ai-memory
uninstall` de hooks ou MCP limpa esse registro, de modo que a próxima inicialização configura novamente).
Ela preserva configurações não relacionadas do usuário, é executada antes do harness iniciar para que ele obtenha
os hooks novos e é de melhor esforço: se a instalação falhar, emite um aviso e ainda assim
inicia. Harnesses sem suporte
de instalador (Crush) são ignorados; o Pi conecta os hooks, mas não tem cliente MCP para gravar. Desative
isso com `ai-memory run --no-autowire`, `AI_MEMORY_RUN_AUTOWIRE=false` ou
`run_autowire = false` na configuração; `install-hooks` / `install-mcp` manuais continuam
disponíveis para harnesses que você nunca inicia via `run`.

O inicializador resolve o nome do seu executável diretamente através do `PATH`. Ele
não passa por um shell interativo, portanto um `claude` definido apenas como um
`alias` de shell em `.bashrc`/`.zshrc` fica invisível para ele. Se você alternar contas
do Claude via alias, coloque um script ou shim com o mesmo nome antes no `PATH`
(ou passe `--executable PATH`, que também resolve um nome simples através do
`PATH`), garantindo que o processo `claude` resolvido seja de fato o pretendido.

Perfis de inicialização nomeados persistem substituições repetidas de variáveis de ambiente por conta
no mesmo `config.toml` que o cliente carrega:

```toml
[run.profiles.work.env]
CLAUDE_CONFIG_DIR = "/home/me/.claude-work"
ANTHROPIC_BASE_URL = "https://api.anthropic.com"
```

```bash
ai-memory run --profile work claude --model opus
```

Perfis são projetados para lidar apenas com variáveis de ambiente; caminhos de executáveis, argumentos nativos, `--yolo`
e escolhas de auto-wire permanecem explícitos na linha de comando. O processo filho herda
o ambiente de processo normal, os valores do perfil o substituem,
`--env-file` substitui o perfil e entradas repetidas de `--env` têm precedência por último.
Os valores resolvidos também orientam a descoberta de sessões nativas e o auto-wire, para que todos
os três usem a mesma conta/diretório de configuração. Um perfil desconhecido ou inválido falha
antes que o ai-memory obtenha uma concessão de workstream ou inicie um harness. Nomes de perfil
usam até 64 letras ASCII, dígitos, `.`, `_` ou `-`; cada perfil pode conter
até 128 entradas de variáveis de ambiente. Como os valores são literais e podem incluir
credenciais, proteja o `config.toml` como qualquer outra configuração local com segredos.

`--profile` pertence ao wrapper somente antes do nome do harness. Uma flag posterior permanece
como argv nativo, o que mantém o seletor de perfil próprio do OMP sem ambiguidades:

```bash
ai-memory run --profile work omp --profile omp-work
```

**Múltiplas contas do Claude (ex.: corporativa e pessoal).** Qualquer nome de harness
iniciado por `claude` é aceito (`claude-corp`, `claude-personal`, ...)
e sempre seleciona o harness Claude. A grafia exata nunca altera
o tipo de agente, o armazenamento de sessões ou a importação de transcrições.
Um nome `claude*` diferente de `claude` e `claude-code` padrão também escolhe o
binário: quando nenhum `--executable` for informado e um executável com exatamente esse
nome estiver no `PATH` de inicialização (incluindo um `PATH` definido com `--env`), o `run`
o executa. Scripts de wrapper por conta, portanto, funcionam sem que você precise
se repetir:

```bash
# ~/bin/claude-corp and ~/bin/claude-personal are wrapper scripts (earlier on
# PATH than the bare `claude`) that each exec the real claude binary with
# that account's config/credentials directory.
ai-memory run claude-corp --model opus
ai-memory run claude-personal
```

Se nenhum executável com esse nome existir, o `run` imprime um aviso no stderr e
inicia o `claude` padrão, para que um wrapper com erro de digitação ou ausente não coloque você
silenciosamente na conta errada. Um `--executable` explícito sempre tem precedência, e `claude` e
`claude-code` sempre significam o binário `claude` padrão. O nome deve ser um nome de comando
simples encontrado através do `PATH` (um alias ou função de shell é invisível
aqui); para usar um binário em outro local, passe `--executable <path>`.

```bash
cd /path/to/project

ai-memory run claude
# quit Claude Code, then continue the same logical workstream in Codex
ai-memory run codex --yolo
# return to Claude Code later; ai-memory supplies Claude's native --resume
ai-memory run claude --model opus
# any `claude*` name is accepted (see "Multiple Claude accounts" above)
ai-memory run claude-corp
# Kimi Code installs `kimi`; `kimi-cli` is accepted as a launcher alias
ai-memory run kimi-cli
# Command Code uses `command-code` on Unix and `cmdc` on native Windows
ai-memory run command-code
# Kiro defaults to v2; select its incompatible v3 engine explicitly once
ai-memory run kiro
ai-memory run kiro --v3
# or omit the harness and continue the newest usable session automatically
ai-memory run
```

Quando esse segundo comando for uma troca real de harness, o servidor retorna apenas o
harness anterior e a identidade da sessão nativa. O inicializador verifica essa
sessão em relação a este checkout exato, lê-a através do adaptador existente de
somente leitura e grava seu histórico normalizado visível em um arquivo Markdown
temporário privado. O processo filho recebe apenas esta variável local:

```text
AI_MEMORY_PREVIOUS_SESSION_HISTORY=/private/temp/.../history.md
```

O pacote de inicialização instrui o agente receptor a ler o arquivo quando o usuário
pedir para continuar ou quando um detalhe exato estiver ausente do pacote portátil limitado.
Essa diretiva criada pelo servidor fica fora da cerca de histórico não confiável do pacote;
apenas as evidências importadas da transcrição ficam dentro da cerca. O bloco de
roteamento canônico do ai-memory fornece o mesmo fallback: quando todas as três
variáveis pertencentes ao wrapper `AI_MEMORY_RUN_ID`, `AI_MEMORY_WORKSTREAM_ID` e
`AI_MEMORY_PREVIOUS_SESSION_HISTORY` estiverem presentes, "continuar", "retomar" ou
"continuar de onde paramos" significa ler o arquivo indicado antes de inferir a
tarefa inacabada. Isso cobre esgotamento de cota e outras saídas abruptas em que o
harness encerrado não consegue preparar um handoff.

Fora de uma execução gerenciada não há dossiê, mas o servidor ainda vê as observações
capturadas da sessão interrompida. Quando o trabalho mais recente no projeto pertencer a
outra sessão do mesmo operador que nunca terminou e não deixou handoff após
esse trabalho,
`GET /handoff` adiciona um aviso apenas com metadados (tipo de agente, ID da sessão, idade) que
informa ao agente para ler essa sessão com `memory_read_session_observations`,
da mais recente para a mais antiga. `memory_briefing` (`recent_sessions`) e `memory_handoff_list`
(`newer_unended_session`) expõem o mesmo ponteiro a clientes cujo início de sessão
não injeta nada.
Passe `recent_sessions_limit` para `memory_briefing` para aumentar o padrão de cinco
entradas (limitado a 1–50) ao verificar harnesses ativos em um projeto movimentado.

O dossiê é evidência, não instruções: ele é cercado como histórico não confiável,
não contém raciocínio oculto nem registros privados/incompatíveis e desaparece
quando o filho sai. O servidor remoto do ai-memory nunca recebe seu caminho ou
corpo. A retomada no mesmo harness não precisa de dossiê porque esse harness retoma sua
própria sessão nativa. Se o armazenamento anterior estiver ausente, ilegível, em outra
máquina ou acima dos limites de segurança do adaptador, a inicialização continua com o
ledger e handoff portáteis e emite um aviso.

Se uma troca de harness na mesma máquina não contiver
`AI_MEMORY_PREVIOUS_SESSION_HISTORY`, verifique o executável usado pelo shell exato
que o iniciou:

```bash
type -a ai-memory
ai-memory --version
```

A continuidade de histórico nativo requer o cliente de host 3.1.0 ou mais recente. Um binário
desatualizado do Cargo ou do gerenciador de pacotes anterior no `PATH` pode ocultar um wrapper Docker
atualizado e seu executor nativo com verificação de checksum. Atualize ou remova o
executável concorrente e reinicie a execução gerenciada; um processo filho já em execução
mantém o ambiente herdado de seu inicializador antigo.

Após a saída bem-sucedida de uma sessão gerenciada interativa, `ai-memory run` oferece
os harnesses instalados, uma opção para executar o harness atual novamente e sair.
Escolher outro harness mantém o mesmo workstream selecionado, de modo que seu contexto
salvo permaneça disponível para a próxima execução. O prompt tem como padrão sair. Ele
aparece apenas quando stdin, stdout e stderr são terminais, a inicialização gerenciada
foi uma sessão e `--executable` não foi usado; comandos utilitários, saídas com falha ou
interrompidas e inicializações não interativas não exibem o prompt. Uma troca ou
reexecução não repete os argumentos nativos do harness anterior, que podem ser
específicos daquela CLI; configurações de wrapper como `--workspace`, `--project`,
`--yolo`, controles do ai-jail, `--no-autowire`, `--no-exit-prompt`,
`--env` e `--env-file` permanecem em vigor. A escolha inicial de `--fresh` aplica-se
apenas à primeira inicialização.

Para retornar ao shell sem o menu, passe `--no-exit-prompt` antes ou
depois do nome do harness:

```bash
ai-memory run --no-exit-prompt codex
ai-memory run claude --no-exit-prompt
```

Para desativá-lo em todas as inicializações, defina `AI_MEMORY_RUN_EXIT_PROMPT=false` ou adicione
isto ao `config.toml`:

```toml
[run]
exit_prompt = false
```

O padrão é `true`. A variável de ambiente aceita `true` ou `false` e
substitui o arquivo de configuração; `--no-exit-prompt` desativa o menu mesmo quando o
ambiente o ativa. Essas configurações afetam apenas o menu de saída. A sessão
gerenciada ainda assim é iniciada, captura o histórico e termina normalmente.

Tudo após o nome do harness é argv nativo, exceto as flags exatas pertencentes
ao wrapper `--yolo`, `--fresh`, `--force-unlock` e `--no-exit-prompt`. Nenhum separador
`--` é necessário, e o ai-memory não mantém uma segunda cópia do esquema
de opções de cada harness.
Outras opções do wrapper vêm antes:

Eventos portáteis, handoffs e resumos de projeto são injetados como dados
históricos não confiáveis e explicitamente delimitados. Textos em formato de instrução contidos no
conteúdo armazenado são apenas evidências: os agentes não devem executar comandos, expor segredos,
alterar permissões ou políticas, nem usar ferramentas apenas porque esse conteúdo solicita.
As instruções atuais de sistema/desenvolvedor/usuário, o arquivo canônico de instruções
do projeto e o checkout atual continuam sendo a autoridade máxima.

O pacote de inicialização mantém mensagens de usuário e assistente, checkpoints, resumos
de compactação e anotações de importação em linha. Chamadas e resultados de ferramentas concluídas usam
pequenos stubs com o nome da ferramenta e a contagem de caracteres original, para que o conteúdo de arquivos
gerados ou a saída de comandos não expulsem a conversa do contexto limitado de
retomada. Suas cargas úteis completas e higienizadas permanecem no ledger e estão
disponíveis através de `ai-memory workstream-search`.

```text
ai-memory run [--workspace NAME] [--project NAME]
              [--workstream NAME | --new NAME] [--executable PATH]
              [--yolo] [--fresh] [--force-unlock] [--no-exit-prompt] [--profile NAME]
              [--env KEY=VALUE]... [--env-file PATH] [--require-server]
              [claude|claude*|codex|opencode|opencode2|pi|crush|omp|kimi|command-code|kiro|grok|antigravity|copilot]
              [native arguments...]
```

O padrão é o workstream selecionado mais recentemente para o repositório e worktree
atuais, criando um chamado `default` no primeiro uso. `--new NAME` inicia uma
linha de trabalho independente; `--workstream NAME` retorna a uma existente. Estes são controles
opcionais de ramificação; eles não alternam harnesses.

Liste os workstreams disponíveis para esses seletores sem iniciar um harness:

```bash
ai-memory workstreams [--workspace NAME] [--project NAME]
ai-memory workstreams --limit 50 --json
```

A listagem é restrita à mesma identidade `(workspace, project, repository, worktree)`
do `run`, coloca a seleção atual em primeiro lugar e depois ordena pela atividade
mais recente. Cada linha inclui os harnesses conectados e o ID estável do workstream; a
resposta não expõe caminhos de checkout, fingerprints de repositório ou IDs de
sessão nativa.

Nomes são escolhidos no momento de `--new` e podem ser corrigidos posteriormente:

```bash
ai-memory rename-workstream --from typo-nmae --to refactor-db
ai-memory rename-workstream --workstream-id 01a04092-… --to refactor-db
```

Os dois seletores são mutuamente exclusivos; o ID é aquele que o `workstreams`
imprime. A renomeação é apenas de metadados. Nomes são exclusivos por checkout, portanto um
destino que outro workstream já possua é recusado em vez de mesclado,
e o destino é validado exatamente como um nome em `--new`. Como o
ledger, harnesses conectados e execuções gerenciadas usam o ID do workstream como chave em vez
de seu nome, nada mais é movido. Isso inclui qual workstream uma execução simples de
`ai-memory run` retoma e a ordem da listagem, sendo que ambos permanecem intactos porque
uma renomeação deliberadamente não toca em `selected_at` nem em `updated_at`. Uma execução
já ativa continua exibindo o nome com o qual foi iniciada até que seja encerrada.

## Você precisa disso?

Provavelmente não a princípio. Os hooks por si sós já garantem a maior parte da continuidade.

- Ignore-o se tudo o que você deseja é um handoff: você fecha o Claude Code
  no meio da tarefa, abre o Codex no mesmo diretório e a próxima sessão
  começa com "de onde você parou, o que falhou, o que está aberto". Isso
  funciona apenas com `install-hooks`; sem necessidade de `ai-memory run`.
- Use-o quando quiser que a retomada nativa do próprio harness (`claude
  --resume` / o seletor) sobreviva a uma troca para outro harness. O
  ledger gerenciado registra o fluxo de eventos visíveis de forma portátil, enquanto uma
  troca de harness na mesma máquina expõe o dossiê maior de histórico visível local
  da sessão nativa anterior sob demanda. O `ai-memory continue` pode, portanto,
  reabrir o mesmo workstream em um agente diferente com mais do que apenas um resumo,
  sem fazer upload da transcrição nativa para o servidor.

Se você nunca troca de harness no meio de um workstream, o fluxo padrão é
mais simples e você não perde nada continuando nele.

## Recuperando um projeto ambíguo

Instalações mais antigas podem conter dois projetos populados que respondem pelo
mesmo nome, por exemplo `widget` e `acme-widget` com a chave legada `widget`.
A resolução de nomes recusa deliberadamente essa colisão. Um caminho exclusivo de checkout
não torna o seletor de nomes do servidor inequívoco.

`run` pode selecionar um projeto existente pelo seu ID estável dentro de um espaço de trabalho:

```sh
ai-memory run --workspace default --project-id <project-uuid> claude
ai-memory run --workspace default --project-id <project-uuid> codex
ai-memory workstreams --workspace default --project-id <project-uuid> --json
```

Com um terminal, um `run` ambíguo oferece uma escolha numerada de projetos correspondentes
que o chamador tem permissão de gravar. Enter cancela sem abrir uma execução. Scripts
recebem um erro acionável contendo IDs autorizados e devem passar
`--project-id`; eles nunca selecionam a primeira correspondência automaticamente. A consulta
de candidatos examina um conjunto limitado de correspondências e relata quando houver mais. Um
ID explícito permanece disponível quando o projeto pretendido não é exibido.

Após uma preparação bem-sucedida, o cliente lembra o ID resolvido em
`<data-dir>/client-projects.json`. Uma escolha autorizada explícita durante a descoberta
de workstream também é lembrada após a leitura ser bem-sucedida. As vinculações
incluem a origem e montagem normalizadas do servidor, espaço de trabalho, projeto solicitado,
caminho canônico de checkout e fingerprints de repositório/worktree. Remotes alterados,
outros checkouts, servidores e escopos não reutilizam a vinculação. O
servidor verifica a participação no espaço de trabalho e as permissões atuais em cada requisição;
o registro local não concede acesso. Um registro local ilegível é mantido inalterado
e recorre à resolução de nomes, que ainda recusa nomes ambíguos. `resume` usa a mesma vinculação ao listar
e iniciar um workstream. Um `--project` explícito substitui um ID em cache;
`--project-id` substitui a escolha do checkout após uma preparação bem-sucedida.
Nenhuma das operações move, mescla ou exclui o histórico do projeto.

Um workstream vazio selecionado por ID inicia uma nova sessão nativa em vez de
adotar automaticamente uma sessão não vinculada encontrada no mesmo caminho. Uma sessão
nativa já capturada em outro projeto ou operador é recusada; sessões
nativas já vinculadas ao workstream selecionado continuam podendo ser retomadas.

Execuções selecionadas por ID exigem um servidor acessível. O contexto SessionStart e os
hooks de captura usam o projeto fixado da execução após validar o operador
iniciador, a concessão (lease) ativa, o harness e a sessão nativa. A inicialização e a vinculação inicial
também exigem o checkout original. Uma execução fixada desatualizada, expirada ou externa é
recusada em vez de roteada pelo nome ambíguo. Execuções existentes mantêm seu
roteamento original por marcador/nome; a flag aditiva de esquema tem como padrão desativada.

As requisições na rede para `/workstream/runs` e `/workstream/recent` aceitam um
`project_id` opcional, mutuamente exclusivo com um `project` não vazio. Clientes
enviam um `project` vazio para seleção por ID para que servidores mais antigos o rejeitem antes
de criar qualquer coisa. As respostas de preparação incluem coordenadas `resolved_project`
opcionais; um cliente que solicita um ID se recusa a iniciar o harness a menos que
o servidor confirme esse ID e espaço de trabalho exatos. Atualize o servidor antes
de usar a seleção por ID. Clientes que usam apenas nomes continuam funcionando.

`POST /workstream/project-candidates` aceita nomes de `workspace` e `project`
e retorna `candidates` além de `truncated`. Ele funciona sem a interface web, usa o
middleware normal de autenticação, inclui apenas correspondências graváveis e retorna
IDs e nomes sem expor caminhos do sistema de arquivos do servidor.

## Inicializador com foco no projeto

`ai-memory show` inverte o fluxo habitual de `cd` e depois `run`: escolha um
checkout local, escolha um harness gerenciado instalado e inicie a partir desse checkout.

```bash
cd ~/Projects
ai-memory show

# Structured discovery only; never launches a harness.
ai-memory show --json
ai-memory show --json --no-scan
```

Uma preparação gerenciada bem-sucedida atualiza `<data_dir>/client-projects.json`, um
registro privado local do cliente indexado por uma URL de servidor normalizada e sem credenciais
e `(workspace, project)`. A resposta do servidor em `/api/v1/projects` fornece
apenas metadados do projeto; ela nunca expõe ou escolhe um caminho de checkout no host do servidor.
Isso permite que um laptop e um desktop mapeiem o mesmo projeto de um servidor doméstico remoto para
diferentes diretórios locais sem sincronizar precedência de caminhos ou conflitos.

Por padrão, o seletor combina links salvos válidos com uma varredura limitada de profundidade 1 no
diretório atual. A varredura reconhece marcadores de projeto comuns, ignora
links simbólicos além de diretórios de dependências/build e resolve cada candidato através
das mesmas regras de marcador e repositório do `run`. `--no-scan` usa apenas links
salvos, enquanto `--workspace NAME` filtra ambas as fontes. Links desatualizados, redirecionados
ou com incompatibilidade de escopo são ignorados e um `run` posterior bem-sucedido repara a
entrada. Se o servidor estiver temporariamente indisponível, links salvos e resultados da varredura
permanecem selecionáveis e uma inicialização a partir do seletor degrada exatamente como qualquer outro
`run` (consulte [Inicializações offline degradadas](#inicializações-offline-degradadas)) em vez
de falhar fechado; passe `--require-server` para recusar a inicialização.

O modo interativo começa com `+ New project`. O inicializador aceita um nome portátil de
diretório em ASCII minúsculo, cria o marcador, roteamento de instruções e
Agent Skills em um diretório temporário oculto de preparação, e o renomeia para o local definitivo apenas quando
toda a configuração for bem-sucedida. `--yolo`, `--fresh` e argumentos nativos finais aplicam-se
ao harness selecionado. Chamadores não interativos (sem terminal) devem usar `--json`; JSON não pode ser
combinado com argumentos de inicialização.

## Continuando de qualquer lugar

O `ai-memory run` simples continua o checkout atual, mas sua busca de workstream
é indexada por `(workspace, project, repo fingerprint, worktree fingerprint)`, portanto
o chamador já deve estar no projeto. `ai-memory continue` fornece a
etapa que faltava e não necessita de `cd`:

```bash
ai-memory continue
ai-memory continue --workspace work
```

O checkout é escolhido inteiramente no cliente, a partir do registro de data e hora `linked_at` que
toda preparação gerenciada bem-sucedida grava em `client-projects.json`. O servidor
nunca é consultado sobre qual diretório usar: ele não expõe caminhos do host, e um
link só pode ser considerado confiável após este host revalidá-lo.

Antes de iniciar, o link mais recente é verificado novamente duas vezes: o caminho registrado deve
ainda ser canônico em relação a si mesmo (rejeitando um diretório que foi movido ou substituído
por um link simbólico), e ainda deve resolver para o mesmo `(workspace, project)`
(rejeitando um checkout que arquivaria a memória desta sessão sob um escopo
diferente). Um link que falhar em qualquer uma das verificações é indicado no stderr e ignorado, sendo
tentado o próximo link mais recente. Um carimbo de data/hora corrompido em `linked_at` também é relatado e
nunca considerado inicializável. O fallback nunca é silencioso: o projeto
e o caminho selecionados são sempre impressos antes do harness iniciar.

Uma vez selecionado um checkout, a inicialização é exatamente o `ai-memory run` simples naquele
diretório, incluindo a seleção automática de harness. `continue`, portanto, aceita
`--workspace`, `--yolo` e `--fresh`, mas não argumentos nativos do harness nem
`--executable`, cujo significado depende de um harness que o usuário não especificou.

## Escolhendo um workstream

`ai-memory resume` apresenta todos os workstreams gerenciados pertencentes ao **checkout
atual**, e então inicia o workstream nomeado selecionado lá. Outros repositórios
e worktrees Git irmãos não são incluídos, mesmo quando compartilham um espaço de trabalho,
nome de projeto ou URL remota. Executar a partir de um subdiretório de um checkout usa os
fingerprints Git daquele checkout e o marcador de escopo mais próximo. Para retomar o checkout vinculado
mais recente a partir de qualquer diretório, use `ai-memory continue` em vez disso.

```bash
ai-memory resume
ai-memory resume --search auth
ai-memory resume --workspace work --limit 50
ai-memory resume --all        # every linked checkout, current one first
```

`--all` restaura a visualização entre checkouts: percorre todos os checkouts vinculados a
este servidor no registro local do cliente (um por projeto), o checkout atual
primeiro, com a consulta totalmente paginada e com escopo de fingerprint própria de cada checkout;
`--workspace` filtra o registro antes de qualquer requisição, e um checkout que não possa
mais ser resolvido ou consultado é ignorado com uma nota.
A listagem lê os fingerprints estáveis de repositório/worktree sem executar
`git status`, de modo que uma varredura da árvore de trabalho ou hook de monitoramento do sistema de arquivos não trave
o seletor. A inicialização selecionada ainda captura seu checkpoint normal do Git.

Use Cima/Baixo para mover entre workstreams e Esquerda/Direita para alternar o
harness de inicialização da linha destacada. Cada linha lembra sua escolha enquanto você
navega. `auto` é a escolha inicial e preserva a descoberta do `ai-memory run` simples
da sessão utilizável mais recente; as opções restantes são executáveis de harness
aceitos detectados no `PATH` do host. Enter inicia a combinação exibida de
workstream/harness. Escape limpa uma pesquisa não vazia e cancela quando a
pesquisa estiver vazia; Ctrl-C sempre cancela.

Basta digitar para filtrar os nomes de workstream com uma pesquisa que ignora maiúsculas/minúsculas;
não há modo de busca nem tecla de prefixo. Os resultados são atualizados imediatamente e cada
caractere imprimível (incluindo `j`, `k`, `q` e `/`) é texto de pesquisa literal.
Backspace remove um caractere e Ctrl-U limpa a consulta. Enter inicia o
resultado destacado diretamente. As teclas de seta navegam e trocam de harness sem
sair da pesquisa.
Um resultado vazio não pode ser selecionado; edite ou limpe a pesquisa para recuperar.

Não há limite padrão de corte: a CLI busca páginas sucessivas e limitadas do servidor,
e o seletor rola com Cima/Baixo, PageUp/PageDown, Home e End. `--search`
define a consulta inicial. Um `--limit N` explícito limita os resultados **após** essa
pesquisa inicial; limpe ou altere a pesquisa sem o limite para navegar por todas as linhas.
`--workspace` verifica o espaço de trabalho resolvido do checkout atual, em vez
de alternar para outro checkout.

A seleção atual lidera o seletor, seguida pela atividade
recente. Cada linha identifica seu espaço de trabalho/projeto, idade da atividade, harness
de inicialização selecionado e harnesses já vinculados. Escolher um harness diferente é
como um workstream existente pode ser continuado em outro agente. `--yolo` e
`--fresh` são encaminhados para a eventual inicialização gerenciada.

O seletor não lê o registro privado `client-projects.json` nem recorre
a outro checkout quando este não tiver workstreams. Ele revalida o caminho canônico
e o escopo resolvido antes de iniciar a seleção. O servidor
recebe apenas os fingerprints de repositório/worktree necessários para a listagem local existente
do checkout, nunca um caminho do host. Ele requer um terminal interativo;
scripts podem continuar a usar `ai-memory workstreams --json` após selecionarem um
checkout por conta própria. A paginação exige um servidor atualizado; se um servidor mais antigo
ignorar o deslocamento de página, a CLI relata que uma atualização é necessária em vez de
entrar em loop infinito ou apresentar silenciosamente uma lista truncada.

## Seleção automática de harness

Sem nenhum nome de harness, o `ai-memory run` inspeciona sessões locais do checkout para
Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code e ambos os motores do Kiro
CLI. Antes de verificar as sessões do OpenCode, ele investiga o executável exato `opencode`
uma vez e usa essa versão principal resolvida para descoberta, inicialização, auto-wire
e importação de transcrição. A busca do executável, a verificação da versão e o início do processo filho
compartilham o mesmo ambiente capturado com sobreposições de `--env` / `--env-file`, incluindo a
substituição que não diferencia maiúsculas de minúsculas de `PATH`/`Path` no Windows. Uma verificação do OpenCode
ausente, malformada, que expirou o tempo limite ou não compatível
ignora apenas o OpenCode; a descoberta continua com todos os outros harnesses disponíveis.
Para um workstream vazio, ele retoma
a sessão mais recente automaticamente. Para um workstream estabelecido, o estado do servidor
tem precedência: o ai-memory retoma o harness vinculado mais recentemente que ainda
tenha uma sessão local utilizável. Ele nunca escolhe uma sessão mais nova, mas obsoleta, de
outro harness simplesmente porque esse arquivo tem um carimbo de data/hora posterior. Os candidatos v2 e
v3 do Kiro compartilham a mesma identidade de agente no servidor, mas a variante exata
do motor nativo selecionado permanece preservada. OMP, Grok, Antigravity e Copilot continuam disponíveis explicitamente,
mas não fazem parte do pool automático. A versão principal resolvida do OpenCode contribui com um
adaptador para o pool, de modo que o armazenamento compartilhado V1/V2 nunca duplica um candidato.

Sessões sem sandbox ai-jail do OpenCode V2 normalmente são executadas em um serviço em segundo plano compartilhado,
de forma que seu plugin não consegue ver o ambiente da execução gerenciada como plugins em processo
veem. Etapas gerenciadas do OpenCode V2 iniciam com os seletores de retomada corretos, capturam
via hooks e importam a transcrição V2 (tudo verificado ao vivo), mas a
confirmação do delta do ledger (`context_delivered`) não é acionada, pois o
caminho de aceite requer o ID da execução no hook session-start. Quando isso falha, o
intervalo é entregue novamente, nunca perdido: uma etapa posterior para outro harness pode receber o intervalo
novamente. Até que o serviço compartilhado forneça contexto de plugin por invocação, a continuidade
entre harnesses para o OpenCode V2 chega através do fluxo comum de handoff em vez
do delta do ledger.

O modo sem argumentos aceita opções do wrapper, mas não argumentos nativos do harness nem
`--executable`, porque seu significado depende do harness selecionado. Em um novo
diretório sem nenhuma sessão no pool automático, ele sai sem criar um
workstream e sugere os comandos explícitos `ai-memory run <harness>`.

## Primeira inicialização gerenciada

Um workstream que de outra forma estaria vazio pode adotar uma das sessões nativas existentes
do harness solicitado. Em uma inicialização interativa, o ai-memory inspeciona o armazenamento desse
harness sem modificá-lo e lista até oito sessões recentes cujo diretório de trabalho
registrado corresponda ao checkout atual. Escolha uma para retomá-la, pressione
Enter para aceitar o candidato mais recente, ou escolha `0` para iniciar uma nova sessão.
Sessões de outro checkout nunca são oferecidas.

A adoção é apenas uma operação de bootstrap. Uma vez que qualquer harness tenha vinculado uma sessão
nativa ou contribuído com histórico portátil de mensagens/ferramentas/compactação, o workstream
está estabelecido. Se o Claude o estabeleceu e o Codex ainda não se juntou a ele, por
exemplo, `ai-memory run codex` cria uma nova sessão do Codex e injeta o
histórico do workstream do Claude. Ele não inspeciona nem seleciona uma sessão mais antiga e não relacionada
do Codex. Retornar ao Codex mais tarde retoma a sessão do Codex já vinculada
a esse workstream.

Seletores nativos explícitos sempre têm precedência. `--new NAME` sempre cria uma nova
sessão nativa para o novo workstream. Invocações por script/não interativas e
inicializações sem entrada de terminal ignoram o seletor e iniciam uma nova sessão. Uma inicialização que
sai antes de produzir uma sessão nativa ou histórico portátil não
consome a oportunidade posterior de adoção.

Antes de adicionar um seletor de retomada pertencente ao ai-memory, o inicializador verifica o ID exato
vinculado no armazenamento nativo do harness sem modificá-lo. Se a transcrição
foi excluída, limpa ou perdida com uma sobreposição de sandbox, o ai-memory inicia uma nova
sessão nativa e reaponta o mesmo workstream quando essa sessão for observada.
Um armazenamento ilegível ou malformado é relatado, mas não é confundido com uma sessão
ausente. O Codex compacta um rollout que não é modificado há uma semana para
`rollout-*.jsonl.zst` e restaura o arquivo simples quando o `codex resume` o reabre.
A cópia compactada conta como a sessão vinculada, portanto a inicialização a retoma
e a importação de transcrição a lê como o arquivo simples. Use
`ai-memory run --fresh <harness>` para pular deliberadamente a sessão
vinculada e o seletor de adoção. `--fresh` não pode ser combinado com um seletor nativo
de resume, continue, session ou fork. Uma nova inicialização interativa ainda
exporta a sessão vinculada como `AI_MEMORY_PREVIOUS_SESSION_HISTORY` quando ela estiver
disponível localmente. Isso é importante quando a nova sessão do Claude usa um diretório de
configuração diferente: o harness pode ler a sessão antiga limitada e validada pelo checkout
sem que o ai-memory precise copiá-la para o servidor.

O comando `/new` na sessão do Codex não expõe uma relação confiável de
thread pai. O ai-memory, portanto, nunca presume que a nova thread é a proprietária do
workstream gerenciado. Se a thread vinculada sair sem confirmar seu
contexto após `/new`, o wrapper explica que a thread pretendida deve ser
selecionada explicitamente na próxima execução gerenciada.

Quando a sessão vinculada estiver ausente e o checkout ainda contiver outra
sessão desse harness, uma inicialização interativa que nomeia seu harness ainda
inicia uma nova, mas também indica a mais recente e o seletor nativo que
a retoma, como `resume <id>` para o Codex. Executar o mesmo comando novamente
com esse seletor logo após o nome do harness atua como um seletor explícito, de modo que
essa sessão é vinculada. O inicializador nunca a escolhe por conta própria e
não pode dizer se essa sessão está aberta em outro terminal ou vinculada a
outro workstream; a adoção continua sendo uma operação de bootstrap.

## O que acontece em cada execução

1. O cliente do host resolve o escopo normal de espaço de trabalho/projeto e um fingerprint
   estável de repositório e worktree. Quando o projeto é o nome de caminho canônico
   que o remote git do checkout deriva, o cliente também envia essa identidade
   remota, e o servidor abre a execução no projeto que a captura de hooks usa para
   o checkout: aquele que já carrega a identidade, ou o projeto cujo caminho de
   repositório contém o cwd, o qual um escritor reivindica no local. Um projeto
   criado a partir de um nome de pasta diferente antes de as identidades serem registradas é,
   portanto, reutilizado em vez de duplicado sob o nome derivado (#1182); um nome
   vindo de `--project` ou de um marcador roteia pelo nome como antes. Se um
   projeto baseado em identidade ainda carregar seu nome base legado, essa gravação autorizada promove o mesmo UUID para
   seu nome de caminho canônico e atualiza o manifesto de escopo do wiki antes de abrir
   a concessão (lease); uma falha no manifesto pós-commit é retornada como
   `manifest_warning` em caso de sucesso ou de erro posterior de preparação, retendo o
   primeiro aviso entre tentativas ocupadas e aguardo da concessão, impresso uma vez pela CLI
   e deixado para reparo na inicialização. A validação pura da requisição, incluindo seletores
   mutuamente exclusivos de workstream, é executada antes da promoção. Ela abre uma concessão (lease)
   renovável de 90 segundos. Apenas um escritor pode possuir um workstream de cada vez, para que
   dois terminais não disputem silenciosamente seus ponteiros de sessão nativa
   ou cursores de entrega.
2. O modo sem argumentos resolve o harness correto disponível. Para um workstream vazio,
   um adaptador interativo explícito pode oferecer sessões locais correspondentes para
   adoção única. Caso contrário, o adaptador repassa argumentos nativos na
   ordem e adiciona um seletor de criação/retomada apenas quando o usuário não tiver fornecido um.
3. `AI_MEMORY_RUN_ID` marca os hooks de ciclo de vida como gerenciados. Quando a preparação
   seleciona um harness diferente do mais recentemente vinculado, a execução fixa a
   identidade prévia de harness/sessão antes que o link de destino possa alterar a
   recência. O host a revalida contra o checkout e, quando disponível,
   expõe um dossiê privado de histórico visível local através de
   `AI_MEMORY_PREVIOUS_SESSION_HISTORY`; nenhum caminho ou corpo do dossiê é enviado ao
   servidor. O SessionStart então vincula a
   sessão nativa real e injeta apenas os eventos portáteis que essa sessão ainda não
   viu. O Crush, que não tem hook SessionStart, recebe o mesmo pacote
   limitado através de uma entrada temporária em `options.global_context_paths`. O Kimi Code
   dispara SessionStart, mas descarta seu stdout, de modo que o hook
   SessionStart do adaptador do kimi apenas captura o evento; ele não busca nem vincula.
   O hook UserPromptSubmit emite o GET `/handoff` com o `session_id` nativo
   na query; o servidor vincula a sessão e renderiza o pacote
   atomicamente, e o Kimi Code injeta o stdout do hook como mensagem do
   usuário antes do turno. Um handoff pendente de uso único permanece aditivo:
   ele é colocado antes do pacote gerenciado, e ambas as reivindicações de entrega são confirmadas
   juntas somente após a resposta completa de handoff/pacote/resumo ser montada.
   Inicializações diretas continuam usando o mesmo caminho de handoff sem um
   pacote gerenciado.
4. Quando o processo filho sai, o ai-memory lê o armazenamento nativo da transcrição sem
   modificá-lo. Uma sessão nomeada na linha de comando (ou escolhida antes do
   spawn) é aquela lida. Caso contrário, uma sessão vinculada durante a execução sob
   seu `AI_MEMORY_RUN_ID`, mesmo a atual do workstream, é lida quando o
   armazenamento nativo a contém para este checkout; somente sem tal vínculo ele
   procura pela sessão mais recente no checkout, que uma inicialização concorrente
   poderia possuir. Mensagens visíveis de usuário/assistente, chamadas/resultados de ferramentas concluídos,
   resumos de compactação e um checkpoint Git não mutante entram em um ledger de
   workstream append-only. Raciocínio oculto e registros privados/incompatíveis são
   excluídos e registrados como anotações de perda de extração. O mesmo ocorre com um registro acima
   do limite de 1 MiB por registro, geralmente uma imagem no resultado de uma ferramenta: ele é ignorado
   em vez de falhar a importação, e uma nota marca a lacuna. Como o
   registro ignorado pode ter sido uma chamada de ferramenta, um resultado posterior nesse turno que
   não possua ID de chamada é tratado como tendo uma chamada desconhecida. As regras de
   `[capture] ignore_paths` do checkout aplicam-se antes do upload, com a tabela de
   decisão do hook ativo: uma chamada de ferramenta excluída e seu resultado nunca entram no
   ledger, uma que a política não possa provar ser segura mantém apenas sua família de ferramenta, e um
   resultado cuja chamada foi importada por uma execução anterior é descartado sob
   uma política. Mensagens não são afetadas. No modo de captura por lista de permissões, um checkout
   sem marcador não envia eventos, pois o hook ativo não emite nenhum
   lá; a execução ainda assim é concluída.

   O processo do Claude pode gravar uma execução em várias transcrições nativas quando
   `/clear` inicia uma nova conversa. O inicializador enumera essas
   continuações, importa cada transcrição abandonada da mais antiga para a mais recente como um
   segmento não final do ledger e só então finaliza a execução na sessão mais
   recente. Assim, os turnos anteriores ao clear mantêm sua identidade original de sessão nativa,
   enquanto a próxima inicialização gerenciada ainda retoma a conversa após o último
   `/clear`.

   Para execuções selecionadas por ID, o `SessionStart` do Claude com `source: "clear"` também
   atualiza a vinculação do hook ativo, de modo que a captura continua antes que o inicializador
   saia. A transição requer o mesmo operador, harness e checkout,
   uma concessão (lease) ativa e acesso atual de Gravação. Eventos comuns não podem alterar
   a vinculação; um `clear` reproduzido para uma sessão nativa abandonada é
   recusado. A transcrição anterior permanece no histórico de sessões nativas
   do workstream, de modo que um `/resume` em sessão de volta para ela
   (`SessionStart` com `source: "resume"`) move a vinculação de volta sob as
   mesmas verificações. Um `resume` é aceito apenas para uma sessão nativa que este
   workstream já possuía e que nenhuma outra execução ativa detenha no momento.

   Cada pacote de workstream entregue começa com um marcador de origem versionado. Se o Claude Code
   persistir esse pacote e sua ferramenta `Read` o retornar, o normalizador de transcrições
   do Claude exclui o resultado marcado em vez de alimentar novamente o histórico entregue
   no ledger. Ele também reconhece o cabeçalho de pacote pré-marcador para
   compatibilidade com sessões nativas existentes.
5. As importações usam IDs de evento determinísticos, cursores de origem incrementais, segmentos
   JSONL higienizados imutáveis e lotes limitados. Uma nova tentativa não pode duplicar
   o histórico. O código de saída do processo nativo é preservado.
6. Command Code, Kiro CLI e Antigravity CLI não têm hook nativo de término de sessão,
   portanto, para eles, a própria execução finaliza a sessão (novamente após um
   resume), como `ai-memory finalize-session --reopen` faria: resumo, handoff
   e consolidação opcional em SessionEnd. Ele faz isso apenas para uma sessão nomeada
   na linha de comando, escolhida antes do spawn, ou vinculada durante a execução,
   nunca para uma que ele tenha precisado procurar. Se a finalização falhar, ele imprime o comando
   exato `finalize-session` para ser executado.

O próximo harness recebe um delta recente limitado porque nenhuma janela de contexto de agente
pode absorver com segurança uma transcrição ilimitada. O ledger visível completo permanece
pesquisável de dentro de um processo de agente gerenciado:

```bash
ai-memory workstream-search "scope resolver decision"
ai-memory workstream-search --limit 50 --json "failed migration"
```

`AI_MEMORY_WORKSTREAM_ID` fornece o ID automaticamente dentro do processo filho. A partir
de outro shell, passe `--workstream-id <uuid>` explicitamente. Os resultados da pesquisa preservam
o harness de origem, papel, sequência de eventos e conteúdo. A atividade histórica de ferramentas
é rotulada como evidência concluída e nunca deve ser reproduzida como uma chamada pendente.

Falhas de ferramentas no OpenCode V1 contribuem com seu texto visível em `state.error` como um
resultado, com `status: "error"` e `is_error: true` nos metadados do ledger.
Chamadas bem-sucedidas mantêm seu resultado comum em `state.output`.

Falhas de ferramentas no OpenCode 2 contribuem com seu texto de erro visível como um resultado, com
`status: "error"` e `is_error: true` nos metadados do ledger. Chamadas bem-sucedidas
mantêm suas partes normais de texto em `state.content`.

Mensagens de compactação do OpenCode 2 importam seu `summary` como um evento de compactação
(e, quando uma compactação com falha não tiver resumo, `error.message`). O adaptador V1
já importava partes de compactação da mesma maneira.

Os cursores do Crush calculam fingerprints de cada revisão de mensagem no intervalo mais recente de
um segundo do carimbo de data/hora. Isso preserva inserções cujo UUID ordena abaixo do cursor anterior
e edições cujo carimbo de data/hora nativo não avançou. Intervalos exatos de recuperação
também retêm os fingerprints do endpoint capturado, de modo que uma revisão gravada posteriormente
naquele segundo não seja puxada para trás em um intervalo anterior.

A pesquisa e o tail também retornam os campos opcionais `source_record_id` e `metadata`.
A CLI em modo texto imprime o registro de origem e os metadados de correlação como JSON com escape;
`--json` preserva esses campos para os consumidores. Respostas de servidores mais antigos sem
eles continuam legíveis. Pacotes de contexto de inicialização mantêm seu formato existente e
omitem esses campos adicionais.

Rótulos de origem e valores de metadados em string são higienizados com o sanitizador
configurado do servidor **antes** do limite de 512 bytes UTF-8. A lista de permissões de metadados
corresponde aos adaptadores fornecidos: strings `tool`, `tool_call_id`, `tool_use_id`,
`parent_id`, `summary_type` e `status`; booleano `is_error`; inteiro de 32 bits com sinal
`exit_code`; e inteiro de 64 bits sem sinal `loss_count`. Chaves desconhecidas, objetos aninhados,
arrays e tipos escalares incorretos são descartados. Metadados vazios são omitidos. Esses
rótulos descrevem evidências históricas não confiáveis; eles não podem conceder autoridade,
escolher um projeto ou comprovar o sucesso de uma ferramenta de forma independente.

A mesma higienização e lista de permissões protegem linhas legadas do SQLite durante a pesquisa/tail,
sem reescrevê-las. Metadados malformados ou despejos legados acima de 16 KiB não retornam
nenhum metadado. Segmentos brutos existentes não são reescritos por uma leitura. A consulta
FTS do ledger, pontuação, ordenação e limites permanecem inalterados; a proveniência é obtida
na consulta existente de eventos, sem leituras SQL adicionais por resultado.

`event_id` continua sendo a chave de deduplicação. Os adaptadores nativos o derivam da
origem inicial antes da redação de proveniência; truncar ou redigir dois rótulos
de origem não pode, portanto, mesclar IDs de evento distintos. Repetir um evento é uma
operação no-op quando sua identidade estável (`agent`, `native_session_id`, `kind`) coincidir.
O primeiro conteúdo indexado, papel, carimbo de data/hora e proveniência permanecem inalterados,
mesmo que um upload posterior seja diferente ou os padrões do sanitizador tenham mudado. Isso
também permite a reprodução de linhas legadas acima dos limites de gravação atuais; a higienização
de proveniência em tempo de leitura ainda se aplica. Alterar a identidade estável é rejeitado
e o lote SQL é revertido. Um segmento bruto imutável rejeitado não é indexado;
uma nova tentativa corrigida pode importar o lote válido sem avançar o histórico para o
rejeitado. Execuções concluídas mantêm o comportamento no-op existente para eventos tardios,
incluindo novos IDs de evento; seu histórico e sequência mais recente não avançam.

IDs de evento do cliente que começam com `managed-run:` são recusados antes do armazenamento bruto,
inclusive em novas tentativas de uma execução concluída. Esse namespace pertence aos eventos
de checkpoint e perda de extração do servidor, que ele anexa ao concluir uma
execução ativa.

## Inicializações offline degradadas

Quando o servidor ai-memory está inacessível (um homelab remoto fora do ar para
manutenção, uma VPN que não está ativa), o `ai-memory run` não aborta. Ele investiga
o servidor primeiro (qualquer resposta HTTP conta como acessível, portanto uma versão antiga
sem `/healthz` ainda passa), imprime um aviso enfático citando a URL do servidor
e o que significa a execução degradada, e inicia o harness mesmo assim:

- **Indisponível durante a execução**: sem concessão (lease) de workstream ou pacote de contexto
  entre harnesses, importação imediata de transcrição ou entrega de handoff. Nada que necessite
  do servidor é executado: sem prepare, lease, link, heartbeat, busca de contexto, verificação
  de status, finish ou importação.
- **Recuperado posteriormente**: a recuperação requer que tanto o cliente `ai-memory run`
  quanto o servidor sejam atualizados para a versão que fornece `ai-memory recover`
  e confirmações modernas de hook por item. Os hooks de ciclo de vida do ai-memory do harness
  (se instalados) enfileiram eventos localmente (spool). Após o término do filho, o inicializador
  descobre sua sessão nativa e anexa apenas metadados limitados (nunca
  conteúdo de transcrição ou segredos) a um diário local de recuperação restrito ao proprietário. Execute
  `ai-memory recover` quando o servidor retornar: sob o bloqueio exclusivo de drenagem
  do spool, ele primeiro coloca em quarentena entradas correlacionadas a cada sessão do diário,
  drena entradas não relacionadas, reexporta a transcrição nativa exata e
  reproduz início de sessão, conteúdo e término de sessão através do caminho de preenchimento
  higienizado existente de `/hook/batch`. Uma reprodução bem-sucedida exclui a
  quarentena; uma falha a restaura. O armazenamento nativo do Antigravity não tem exportador de
  transcrição aceito, portanto sua inicialização degradada ainda é registrada explicitamente como
  somente-spool e a recuperação nunca invoca seu exportador. Suas entradas correlacionadas
  são colocadas em quarentena e o diário é limpo como `recovered-via-spool` apenas após
  a confirmação de sua entrega durável.
  Um spool vazio, um descarte correlacionado anterior ou uma inicialização sem evidências de hook
  permanecem no diário com status de reparo manual. Chaves estáveis de ingestão de recuperação mantêm
  a sessão exata, agente, tipo de evento e identidade de evento higienizada até a
  exclusão da sessão/projeto; um escritor comum não pode reivindicar previamente uma para outro
  evento. Chaves de hook comuns ainda expiram após 30 dias. Um registro existente de MCP
  ainda degrada para ausência de recuperação durante a indisponibilidade, em vez de
  bloquear a sessão.
- **Sessões**: como não há concessão (lease), não há exclusão mútua contra
  outro inicializador no mesmo checkout, de modo que uma inicialização degradada nunca adota ou
  retoma uma sessão implicitamente. Um seletor explícito de sessão nativa ainda
  retoma a sessão indicada; antes de tal retomada, o inicializador persiste o
  cursor exato pré-inicialização do adaptador nativo, e a recuperação exporta apenas registros
  posteriores a ele. Se esse cursor não puder ser comprovado, a inicialização permanece utilizável, mas
  não é registrada no diário para reprodução automática. Caso contrário, o inicializador captura
  candidatos locais do checkout antes do spawn e registra no diário apenas quando exatamente uma nova
  sessão aparecer. Inicializações concorrentes ambíguas não são registradas no diário e imprimem
  a ação de recuperação manual `backfill --force --session <id>`.
- **Auto-wire**: a instalação de hooks na primeira inicialização ainda acontece (é local e
  idempotente, e a captura deve enfileirar no spool offline), mas nenhuma entrada MCP é registrada
  para o servidor agora inacessível e nenhuma marca de conclusão é gravada, de modo que a
  próxima inicialização online conclui a configuração. Registros existentes nunca são
  afetados.
- **Atribuição**: o processo filho é iniciado sem `AI_MEMORY_RUN_ID` /
  `AI_MEMORY_WORKSTREAM_ID`, portanto seu hook SessionEnd não pode atribuir a
  sessão a uma execução do servidor que nunca ocorreu.
- **Código de saída**: o próprio código de saída do filho é retornado; a inicialização em si
  não é um erro.

Quando o processo filho sai, o inicializador imprime que a execução não foi registrada no
servidor, quantos eventos de hook continuam enfileirados localmente e que a execução foi
registrada no diário para `ai-memory recover`. O diário é limitado a 256 entradas;
uma vez cheio, ele recusa novas entradas com um aviso acionável em vez de descartar
trabalho não recuperado. Ele é gravado atomicamente como estado local exclusivo do proprietário; aberturas de trava no Unix usam
`O_NOFOLLOW` e a substituição atômica verifica novamente o destino imediatamente
antes de renomear. O Windows aplica uma ACL exclusiva do proprietário e rejeita links simbólicos observados,
mas a proteção contra condições de corrida em reparse-points ainda requer a esteira de CI `windows` do PR antes
do merge. Eventos enfileirados permanecem limitados
separadamente: são descartados após o número configurado de tentativas falhas de drenagem
(8 por padrão), 7 dias de idade, ou o limite de 10.000 arquivos no spool. Para uma
interrupção planejada, defina `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0` para desativar apenas os
descartes por contagem de tentativas (consulte `docs/install.md`).

Se o servidor falhar *durante* uma execução, após a concessão (lease) ter sido adquirida, o código de
saída ainda é preservado. O diário registra o ID da execução original, o localizador
da sessão nativa, o caminho absoluto do armazenamento e o código de saída; o branch do repositório e o estado
do caminho não são persistidos. Novas tentativas de recuperação validam o intervalo semântico ordenado
exato da transcrição e a identidade nativa, e omitem um checkpoint regenerado, para que
alterações no repositório durante a inatividade não sejam reivindicadas como estado da execução falha e
uma finalização bem-sucedida não crie um segundo evento de checkpoint.
`ai-memory recover` usa a rota `/recover/finish` autorizada pelo proprietário para retomar
uma execução original expirada apenas quando nenhuma execução mais nova a tiver substituído; a ordenação é a
tupla explícita `(started_at, SQLite insertion order)`, de modo que empates no carimbo de data/hora não possam
reviver uma execução mais antiga. Outro operador, uma execução substituída e qualquer payload
de execução concluída que não seja uma reprodução exata dos eventos/cursor/estado de saída já indexados
permanecem recusados.
Entradas bem-sucedidas são removidas, enquanto entradas com falha permanecem com um motivo higienizado
e limitado. Cada requisição do diário e toda a passagem do diário possuem orçamentos
de tempo. Uma passagem de spool que descarte algo é relatada como incompleta e mantém todas
as entradas do diário; entradas correlacionadas são colocadas em quarentena antes dessa passagem e só
podem ser excluídas após uma passagem limpa posterior e reprodução exata da transcrição. Se o
servidor ainda estiver inacessível, o `recover` sai com sucesso
e imprime as contagens restantes do spool/diário e orientações de inicialização.

Para restaurar o comportamento estrito (falhar com o diagnóstico habitual de "não foi possível alcançar …"
e nunca iniciar o agente), passe `--require-server`, defina
`run.require_server = true` no `config.toml`, ou exporte
`AI_MEMORY_RUN_REQUIRE_SERVER=true`.

## Privacidade da identidade nativa

Novas vinculações gerenciadas aceitam a identidade nativa original exata apenas quando for
não vazia, no máximo 512 bytes UTF-8, livre de caracteres de controle e formatação invisível
insegura, e inalterada pelo higienizador de privacidade. IDs nunca são truncados,
normalizados ou substituídos por um espaço reservado de redação para fazer uma
vinculação ser bem-sucedida. IDs existentes de fornecedores e UUIDs gerados para novas sessões continuam
sendo aceitos. Cada adaptador mantém suas próprias restrições de caminho e seletor.

O inicializador valida IDs preparados e vinculados de servidores mais antigos antes da retomada
ou do uso do armazenamento nativo, e valida identidades de finalização antes da serialização.
Regras de privacidade configuradas também se aplicam ao status pós-inicialização e IDs descobertos
antes da exportação da transcrição e transporte de finalização.
O ingresso HTTP aplica as regras de privacidade configuradas antes das gravações de segmentos; o
escritor também valida identidades antes do SQL de vinculação/importação, após as verificações existentes
de proprietário e de Gravação atual na finalização. Um auto-link de hook recusado ainda deixa a
captura compartilhada regular e higienizada de observações e o roteamento legado de sessão por UUID/v5
intactos. Uma execução sem ID nativo mantém seu comportamento existente de checkpoint desvinculado.

`workstream-search --json` mantém seu campo String existente: um `native_session_id` histórico
inválido é retornado como `""` (UNKNOWN), o qual não pode ser usado para uma nova
vinculação gerenciada. Identidades válidas mantêm seus bytes exatos. O valor armazenado,
conteúdo de evento, linhas, cursor de origem e histórico compartilhado não são reescritos. A
CLI repete a projeção de privacidade para respostas de servidores mais antigos.

Esse limite estabelece apenas a representação da identidade e a privacidade. Ele não
prova que um arquivo nativo existe, autentica um ID de origem, correlaciona uma execução a um
evento ou estabelece o resultado de uma ferramenta. A recusa em SQL permanece transacional; esta
alteração não adiciona uma vedação atômica em disco/SQL nem altera a política de concessão/tentativas.

## Comportamento do adaptador nativo

| Harness | Nova sessão nativa | Sessão nativa recorrente | Fonte somente leitura |
|---|---|---|---|
| Claude Code | `--session-id` gerado | `--resume <id>` | `~/.claude/projects/**/*.jsonl` |
| Codex | criação nativa padrão | `resume <id>` | `~/.codex/sessions/**/rollout-*.jsonl`, ou a cópia `rollout-*.jsonl.zst` que o Codex mantém de um rollout ocioso por uma semana (o arquivo sem compactação tem precedência se ambos existirem) |
| OpenCode V1 | criação nativa padrão | `--session <id>` | `~/.local/share/opencode/opencode.db` aberto em modo somente leitura; selecionado quando o executável exato de inicialização informa versão principal 1 |
| OpenCode V2 | criação nativa padrão | `--session <id>` | mesmo nome de arquivo `opencode.db`, usando as tabelas `session_v2`/`session_message`; selecionado quando o executável exato de inicialização informa versão principal 2 |
| Pi | `--session-id` gerado | `--session <id>` | `~/.pi/agent/sessions/**/*.jsonl` |
| Crush | criação nativa padrão | `--session <id>` | `<data dir>/crush.db` aberto em modo somente leitura: `options.data_directory` dos arquivos JSON de configuração do Crush, senão o `.crush` mais próximo até a raiz do worktree git, senão `<cwd>/.crush` |
| Kimi Code | criação nativa padrão | `--session <id>` | `$KIMI_CODE_HOME/sessions/*/*/agents/main/wire.jsonl` |
| Command Code | criação nativa padrão | `--session <uuid>` | `~/.commandcode/projects/*/<uuid>.jsonl` |
| Kiro CLI v2 | criação nativa padrão | `--resume-id <uuid>` | `$KIRO_HOME/sessions/cli/<uuid>.jsonl` (+ metadados adjacentes em `<uuid>.json`) |
| Kiro CLI v3 | criação nativa padrão com `--v3` | `--v3 --resume-id <sess_uuid>` | `$KIRO_HOME/sessions/<checkout-bucket>/<sess_uuid>/messages.jsonl` (+ metadados adjacentes em `session.json`) |
| OMP | criação nativa padrão | `--resume=<id>` | `<agent dir>/sessions/**/*.jsonl`, ou o diretório de sessões XDG descrito abaixo |
| Grok Build CLI | `--session-id` gerado | `--resume <id>` | `$GROK_HOME/sessions/*/*/chat_history.jsonl` |
| Antigravity CLI | criação padrão nativa | `--conversation <id>` | metadados em `~/.gemini/antigravity-cli/conversations/<id>.db`; prompts do usuário de `history.jsonl`, com fallback para `brain/<id>/.system_generated/logs/transcript{,_full}.jsonl` limitado dessa conversa para o modo de impressão (etapas do assistente e ferramentas vêm da captura por hook de ciclo de vida) |
| GitHub Copilot CLI | `--session-id` gerado | `--resume=<id>` | `$COPILOT_HOME/session-state/<uuid>/events.jsonl` (`COPILOT_HOME` tem como padrão `~/.copilot`) |

O diretório de agente do OMP é `~/.omp/agent` para o perfil padrão e
`~/.omp/profiles/<name>/agent` para um perfil nomeado. `PI_CONFIG_DIR` altera a
raiz `.omp` em relação à pasta pessoal do usuário. O perfil padrão também pode usar
`PI_CODING_AGENT_DIR`. No Linux e macOS, as sessões vão para
`$XDG_DATA_HOME/omp/sessions`, ou `$XDG_DATA_HOME/omp/profiles/<name>/sessions` para
um perfil nomeado, quando esse diretório do OMP existir e o diretório do agente
não tiver sido realocado.

As transcrições do Command Code v3 são autodescritivas e somente de acréscimo. O adaptador
exige que o nome de arquivo UUID, o ID do cabeçalho e o `cwd` canônico do cabeçalho coincidam
antes da descoberta ou retomada. Sua lista de permissões da versão 1.14.1 foi verificada contra o
pacote publicado com integridade confirmada e um fixture real saneado. Ele importa
mensagens visíveis, compactações e resumos de branches, retém `parentId` como
procedência do branch, e exclui raciocínio oculto, imagens, mensagens injetadas pelo
harness, metadados de provedor/modelo, registros personalizados/Mod e todo sidecar. Uma
versão desconhecida de transcrição falha fechada até ser auditada.
O executável padrão é `command-code` no Unix e `cmdc` no Windows nativo;
`commandcode`, `cmdc` e `cmd` são aliases de inicialização aceitos. As escolhas exatas do
usuário de `--session`, `--resume`, `--continue` e bifurcações permanecem soberanas.
A API experimental de Mod sem sandbox não é utilizada.

Um seletor nativo explícito como `--resume` do Claude, `--session` do OpenCode,
`resume` do Codex, `--conversation` / `--continue` do Antigravity, ou
`--resume` / `--continue` / `--session-id` / `--connect` do Copilot tem prioridade.
O ai-memory vincula a sessão nativa selecionada e redefine um cursor de adaptador não
relacionado em vez de assumir que ele pertence à sessão antiga.

Quando a sessão vinculada do Claude é uma sessão em segundo plano do Claude Code que ainda
está em execução no daemon, o Claude recusa `--resume <id>` se outra flag vier
junto com ele (observado com `--model`, `--effort` e
`--dangerously-skip-permissions` no Claude Code 2.1.287; um `--resume` isolado
se acopla) e aponta para `claude attach <id>`. Apenas quando a
transcrição mostra que a sessão rodou em segundo plano (`sessionKind: "bg"`)
o ai-memory consulta `claude agents --json --cwd <checkout>`. Quando isso lista a
sessão como ativa em segundo plano neste checkout, ele inicia
`claude attach <id>` usando o ID curto da listagem. Argumentos nativos não
são passados ao cliente de acoplamento. Na validação de ciclo de vida do Claude Code 2.1.288,
nenhum novo `SessionStart` foi observado do cliente acoplado; os hooks do worker
em segundo plano transportaram o ID da sessão em segundo plano. A entrega de um pacote pendente
de contexto de workstream via attach permanece não verificada: essa validação
não exercitou um pacote pendente. O inicializador relata essa limitação e
suprime o aviso de confirmação ausente para o attach. (#1052)

Quando o cliente de acoplamento se desacopla ou sai prematuramente, a execução gerenciada termina com
o status de saída do cliente e importa a transcrição vinculada disponível na finalização.
Ela não interrompe o daemon nem espera a conclusão do turno em segundo plano;
eventos gravados após essa importação permanecem de fora. Na mesma validação,
Ctrl-Z desacoplou com saída 0, e encerrar apenas o cliente acoplado produziu
saída 1 enquanto o daemon continuou e emitiu `Stop` após a conclusão da execução.

Uma listagem de `claude agents` que falhe, esteja indisponível ou seja inesperada mantém a retomada
nativa e suas flags originais. Esse fallback ainda pode encontrar a recusa do
Claude em retomar com flags uma sessão ativa em segundo plano. O teste de ciclo de vida
injetou uma listagem com saída 127 e um objeto JSON incompatível, exercitando então
a retomada e o backend reais; um binário do Claude mais antigo sem `agents` não foi
testado. Essas observações estão registradas na
[validação de ciclo de vida do PR #1067](https://github.com/akitaonrails/ai-memory/pull/1067#issuecomment-5962340148).

Sessões em segundo plano do Claude Code rodam no daemon do Claude Code, separadamente
do cliente de acoplamento. Seus hooks usam o ambiente herdado do worker do
daemon: a validação da versão 2.1.288 observou um `AI_MEMORY_RUN_ID` de uma inicialização
anterior, diferente das execuções de acoplamento atuais. Um hook do daemon, portanto,
não identifica por si só a execução que está se acoplando no momento. Quando uma sessão gerenciada
do Claude se acopla a uma (`/resume` em uma sessão indicada como
"running in the background"), a conversa prossegue na transcrição da sessão
em segundo plano. Ao final da execução, o ai-memory busca uma transcrição
gravada durante a execução cujos registros `sessionKind: "bg"` indiquem a própria sessão
da execução como o cliente acoplado e este checkout como `cwd`, e finaliza a execução
nessa sessão em segundo plano, para que a próxima inicialização a retome em vez da sessão vazia
em primeiro plano. Uma sessão em segundo plano acoplada por outra execução nunca é
adotada. (#1050)

O Crush não tem hooks para vincular sua sessão: uma inicialização nova do Crush assume a única sessão de nível superior
criada enquanto executava (suas sessões de título e subagentes não contam), e
não importa nada, emitindo um aviso, quando outra execução no mesmo armazenamento também
criou uma; retome essa sessão com `--session <id>` para vinculá-la. `--continue`
assume a única sessão que tocou sob os mesmos termos. Quando o diretório de dados
fica fora do projeto, como faz um `data_directory` global, as sessões de outros projetos
o compartilham e o Crush não registra nenhum diretório por sessão, de modo que uma execução assume
apenas uma sessão que editou um arquivo neste projeto (edições de seus subagentes
contam), e apenas se ela for a única; caso contrário, não importa nada, emitindo o
mesmo aviso. O local real do armazenamento decide, logo um `.crush` com symlink para um
diretório compartilhado é compartilhado. Uma sessão de outro projeto que editou um arquivo aqui
ainda conta como pertencente a este projeto.
Os valores de `--session-dir` do Pi e do OMP e de `--data-dir` do Crush são repassados
sem alterações e usados como raiz de importação somente leitura. Sem `--data-dir`,
o ai-memory localiza o diretório de dados do Crush como o próprio Crush faz, exceto que não
executa um `crushrc` para ler um conjunto apenas nele; passe `--data-dir` nesse caso. Substituições
de ambiente de armazenamento nativo também são respeitadas:
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `XDG_DATA_HOME`,
`PI_CODING_AGENT_SESSION_DIR`, `PI_CODING_AGENT_DIR`, `KIMI_CODE_HOME`,
`KIRO_HOME` e `GROK_HOME`, além de `PI_CONFIG_DIR` do OMP.
A seleção de perfil do OMP segue um `--profile` inicial, depois `OMP_PROFILE`, e então
`PI_PROFILE`. Um perfil nomeado ignora `PI_CODING_AGENT_DIR`. Um `--profile`
posterior na linha de comando é deixado para o OMP; use `OMP_PROFILE` ou
`--env OMP_PROFILE=<name>` para que o ai-memory possa resolver o mesmo perfil.

Substituições de diretório em branco (valores vazios ou apenas espaços) contam como não definidos.
A importação de sessão, hooks e MCP usam seus caminhos de fallback, e `ai-memory run`
remove esses valores do ambiente filho. Variáveis de perfil seguem uma
regra diferente: um `OMP_PROFILE` vazio seleciona o perfil padrão e ainda assim
tem precedência sobre `PI_PROFILE`.
Exporte essas variáveis no ambiente que o próprio `ai-memory run` vê, ou passe-as com
`--env` (abaixo), não apenas dentro de um script wrapper do harness. `ai-memory run`
resolve o diretório da sessão nativa, e conecta hooks e MCP automaticamente, a partir desse
ambiente; se o harness gravar sua transcrição sob um `CLAUDE_CONFIG_DIR`
personalizado que o `ai-memory run` não consiga ver, os dois divergirão e a
importação da transcrição nativa falhará. Ao usar diretórios de configuração
por conta, defina a variável antes de invocar `ai-memory run` (ou no
mesmo wrapper que também o executa), de forma que a instalação de hooks e a resolução
da sessão nativa concordem.

Um comando repetível `ai-memory run --env KEY=VALUE <harness>` (e `--env-file
<path>`, uma `KEY=VALUE` por linha, ignorando linhas em branco e comentários `#`) é
a alternativa de primeira classe ao padrão de alias de wrapper `env KEY=VAL harness`
acima: é uma flag pertencente ao wrapper, devendo portanto preceder o nome
do harness, e o ambiente resolvido alcança o processo do harness gerado,
a própria resolução de sessão nativa do `ai-memory run` *e* a conexão automática da primeira execução.
Hooks, MCP e importação de transcrição seguem então a mesma raiz de configuração, atendendo ao
requisito de concordância de `CLAUDE_CONFIG_DIR` descrito acima, sem precisar exportar a
variável previamente na shell chamadora. Um `--env` posterior substitui uma entrada de mesma chave
em `--env-file`; nenhum deles expande ou interpreta o valor, portanto deixe a shell
expandir `$HOME` na linha de comando e utilize caminhos absolutos em um
`--env-file`. Os comandos manuais `install-hooks` / `install-mcp` não aceitam `--env`; eles
leem seu próprio ambiente.

A seleção automática de harness (`run`, `continue` e `resume` isolados) examina o
armazenamento que a inicialização resolve a partir desse mesmo ambiente, de forma que um checkout cujas
sessões fiquem sob um `CLAUDE_CONFIG_DIR` personalizado seja encontrado quando a variável estiver
definida. Sempre que o cliente vincula uma sessão, na inicialização ou no encerramento da execução,
ele também registra o armazenamento dessa sessão no `client-projects.json` local do cliente.
Se uma inicialização posterior não conseguir encontrar a sessão vinculada no armazenamento resolvido e
o armazenamento registrado for um diretório diferente, a execução para com um erro
indicando ambos os diretórios em vez de começar do zero e redirecionar o workstream
para longe de uma sessão que ainda existe. Reinicie com a mesma variável (ou
flag de armazenamento) para retomá-la, ou passe `--fresh` para iniciar uma nova sessão. Sessões vinculadas antes de este registro
existir, bem como uma sessão ausente em seu próprio armazenamento registrado, continuam iniciando do zero
como antes.

O adaptador da família Pi
também reconhece um arquivo de escrita atômica `.jsonl.<nonce>.tmp` completo quando um processo
nativo é encerrado antes de renomeá-lo; registros JSONL finais incompletos nunca são
importados. Subcomandos de ajuda, versão e utilitários conhecidos passam sem
flags de sessão. O modo de impressão do Claude/Pi/OMP, `exec` do Codex, `run` do OpenCode/Crush,
entrada redirecionada e outras inicializações não interativas nunca abrem o seletor
de adoção.

`ai-memory run --yolo <harness>` e `ai-memory run <harness> --yolo` usam
o modo perigoso nativo do harness. A tradução é Claude Code
`--dangerously-skip-permissions`, Codex
`--dangerously-bypass-approvals-and-sandbox`, OpenCode `--auto`, Pi `--approve`,
Crush `--yolo`, Kimi Code `--yolo`, Command Code `--yolo`, Kiro CLI v2
`--trust-all-tools`, Grok Build CLI `--yolo` (equivalente à sua
opção `--always-approve`), Antigravity CLI
`--dangerously-skip-permissions` e GitHub Copilot CLI `--yolo` (equivalente
à sua opção `--allow-all`). O Kiro v3 substituiu a flag trust-all por
`permissions.yaml`, portanto o ai-memory imprime um aviso e não adiciona nenhuma flag não verificada.
O OMP atualmente não precisa de flag adicional. O ai-memory não adiciona duplicata quando a
flag nativa traduzida já estiver presente.

O suporte gerenciado é intencionalmente mais restrito do que a matriz geral de integração.
IBM Bob, Gemini CLI, Devin CLI, Cursor e outros agentes podem
ter suporte a MCP ou hook de ciclo de vida sem retomada nativa gerenciada. Contribuidores
que adicionarem outro harness gerenciado devem seguir o [protocolo de contribuição
de harness gerenciado](managed-harness-contributions.md), incluindo extração somente leitura,
entrega de contexto pré-turno, invariantes de migração, testes determinísticos e uma
etapa de aceitação opcional com o harness real.

### Recuperação do daemon compartilhado do Codex (#987)

Versões recentes do Codex executam sessões através de um daemon compartilhado app-server
em segundo plano (`codex agents` o lista). O daemon mantém o ambiente com o qual foi iniciado,
e os hooks de ciclo de vida que ele inicia herdam esse ambiente, incluindo
o `AI_MEMORY_RUN_ID` de qualquer execução gerenciada que o iniciou automaticamente. Quando um SessionStart
gerenciado posterior do Codex relata esse ID finalizado, o ai-memory o ignora como
autoridade e pesquisa apenas o repositório já resolvido da requisição, o cwd exato
do checkout e o bucket do operador. O servidor adota uma substituição apenas quando
exatamente uma execução ativa e não entregue do Codex estiver aguardando lá, e ele seleciona e
vincula essa execução em uma única transação de escrita. Múltiplos candidatos, um ID ativo
cujo escopo, checkout ou proprietário não coincida, e uma execução já vinculada por
outra sessão falham fechados.

Execuções gerenciadas comuns do Codex funcionam, portanto, com o daemon compartilhado. Para
diagnóstico, ou quando um par intencionalmente concorrente de workstreams nomeados tornar
a recuperação ambígua, argumentos nativos após o harness ainda são repassados:

```bash
ai-memory run codex --no-daemon
```

`--no-daemon` faz essa sessão específica rodar sem o servidor em segundo plano compartilhado,
mesmo que um já esteja em execução; está disponível nos comandos interativo e
`resume` do Codex (verificado no Codex 0.156). Ele continua útil para isolamento,
mas a continuidade normal do workstream não o exige.

## Instalação e recuperação

Execuções gerenciadas precisam de hooks de ciclo de vida do ai-memory atualizados para que SessionStart receba
o delta portátil. Atualize-os após o upgrade:

```bash
ai-memory install-hooks --agent claude-code --apply
ai-memory install-hooks --agent codex --apply
ai-memory install-hooks --agent opencode --apply
ai-memory install-hooks --agent pi --apply
ai-memory install-hooks --agent omp --apply
ai-memory install-hooks --agent kimi-code --apply
ai-memory install-hooks --agent kiro-cli --apply
```

Hooks do Kimi Code instalados como comandos nativos `ai-memory hook` obtêm
automaticamente o comportamento de entrega atual quando o binário é atualizado. Uma
instalação com fallback por script deve executar novamente o comando `install-hooks` do Kimi Code
após o upgrade para que seus scripts preparados sejam atualizados. Hooks atuais entregam
handoffs em `UserPromptSubmit`; Kimi descarta o stdout de `SessionStart`.

Limitações conhecidas do adaptador do Kimi Code: transcrições de subagentes
(`agents/<id>/wire.jsonl` exceto `main`) não são importadas na v1 e são
registradas como uma anotação de perda de extração; o nome do diretório do bucket de sessão
é um hash unidirecional do diretório de trabalho, de modo que a descoberta sempre lê
o campo `cwd` atual de `state.json` ou o alias legado `workDir` e nunca analisa
o nome do bucket. Aliases conflitantes ou um ID persistido que discorde do
diretório da sessão são rejeitados. IDs de evento derivam
do SHA-256 da linha bruta de wire.jsonl, portanto duas linhas idênticas em bytes
colapsam em um único evento de ledger. Isso só pode acontecer com conteúdo
idêntico no mesmo milissegundo, porque o Kimi Code carimba cada registro com
`time`.
O cursor incremental armazena tanto o deslocamento de bytes do registro completo quanto um
SHA-256 desse prefixo importado. Anexos normais são retomados no deslocamento salvo;
se o Kimi reescrever o `wire.jsonl` no local, o ai-memory reinicia do início e
reproduz o arquivo, com IDs de evento estáveis deduplicando registros já presentes no
workstream.
Sessões legadas que mantêm `wire.jsonl` diretamente no diretório da sessão
(a estrutura pré-`agents/` que o armazenamento de sessão do kimi ainda lê via fallback
de stat) não são descobertas nem importadas na v1. O contrato
nativo foi reverificado contra o Kimi Code v0.34.0. O inicializador gerenciado aceita
`kimi`, `kimi-code` e `kimi-cli`; todos os três resolvem o executável instalado
do `kimi`.

O adaptador com detecção de versão do Kiro foi testado ao vivo com o Kiro CLI
2.16.2 autenticado em ambos os motores. V2 usa IDs de sessão em UUID e o armazenamento
simples `$KIRO_HOME/sessions/cli/<uuid>.json` mais `<uuid>.jsonl` com eventos v1 de
`Prompt`, `AssistantMessage` e `ToolResults`. V3 usa IDs incompatíveis
`sess_<uuid>` e armazenamento aninhado em
`$KIRO_HOME/sessions/<checkout-bucket>/<sess_uuid>/session.json` mais
`messages.jsonl`; metadados aceitos são limitados a `schemaVersion = 1.0.0`,
`dataModelVersion = 1`, uma correspondência exata de diretório/ID e uma entrada de `workspacePaths`
resolvendo para o checkout atual. A lista de permissões de eventos visíveis da v3 é
texto do usuário, saída `Say` do assistente, chamadas de ferramentas e resultados de ferramentas. Controle
de sessão, registros de hooks, resumos de uso, limites de turno, operações privadas do assistente,
registros malformados e versões desconhecidas de esquema não são importados.

Os motores nunca podem retomar cruzado: metadados exatos de armazenamento são verificados antes que um
`--resume-id` vinculado seja injetado, e a variante incompatível do motor também é
armazenada no cursor incremental opaco. `--v3` explícito, `--mode` exclusivo da v3,
ou `--agent-engine v3` seleciona v3; `--agent-engine v2` explícito seleciona v2; um
valor de motor desconhecido permanece como passthrough em vez de ser adivinhado. Uma vez vinculado a uma
sessão v3, um `ai-memory run kiro` simples posterior recupera esse motor
transparentemente. O Kiro CLI 2.16.2 gravava sessões v3 abaixo do padrão
`~/.kiro/sessions` mesmo quando `KIRO_HOME` redirecionava outro estado, portanto o ai-memory
verifica a raiz v3 configurada primeiro e essa raiz padrão como fallback de
compatibilidade. Se uma sessão vinculada existir apenas no fallback, o ai-memory remove
`KIRO_HOME` para essa única retomada para que o Kiro possa encontrar a sessão; o Kiro consequentemente
usa suas configurações/hooks v3 do diretório padrão para esse processo, e o auto-wire de primeira
inicialização configura esse diretório padrão em vez de `KIRO_HOME`. Novas inicializações e
versões que armazenam a sessão abaixo da raiz configurada mantêm `KIRO_HOME`
inalterado. Todo candidato ainda precisa de ID exato, esquema e metadados de checkout.
A tradução de `--yolo` na v2 é `--trust-all-tools`; uma escolha mais estrita
`--trust-tools` nunca é ampliada. A V3 não documenta nenhuma flag equivalente de CLI.
Consulte as referências atuais do Kiro de
[gerenciamento de sessões](https://kiro.dev/docs/cli/chat/session-management/) e
[compatibilidade v3](https://kiro.dev/docs/cli/v3/).

O Grok também não precisa de instalação de hooks do ai-memory para entrega gerenciada. O Grok
ignora o stdout de `SessionStart` e seu hook `UserPromptSubmit` é passivo, de modo que
o inicializador busca o pacote de contexto limitado no servidor e o repassa
através da flag nativa `--rules` do Grok, que anexa o texto ao prompt de sistema
dessa sessão. A entrega é confirmada apenas após o spawn do filho. Como
`--rules` é de uso único no analisador de argumentos do Grok, um `--rules`/`--append-system-prompt`
fornecido nativamente vence e o pacote permanece não entregue até
que uma execução gerenciada posterior possa aceitá-lo. O Grok pode reescrever `chat_history.jsonl` no
local ao retroceder, de modo que o cursor de importação armazena um hash de prefixo e reproduz a partir
do início quando ele não coincidir mais, com IDs de evento por hash de conteúdo
deduplicando registros já presentes no workstream. Arquivos irmãos de sessão
(`events.jsonl`, `updates.jsonl`, `rewind_points.jsonl`) contêm dados internos
do harness e nunca são lidos como transcrições; a descoberta lê
o `info.cwd` de `summary.json` e nunca analisa o nome codificado do bucket na URL. O
inicializador gerenciado aceita `grok` e `grok-build`. O contrato nativo foi
verificado contra o Grok Build CLI v0.2.111.

O Antigravity mantém um banco de dados SQLite por conversa em
`~/.gemini/antigravity-cli/conversations/<conversation-id>.db`, portanto o ID é o
nome do arquivo e nenhuma varredura é necessária para localizá-lo. O espaço de trabalho onde uma conversa foi
aberta vem de `trajectory_metadata_blob`, uma mensagem protobuf cujo primeiro
campo contém uma mensagem aninhada cujo primeiro campo é a URI `file://` do espaço de trabalho;
apenas esses dois campos são lidos. Um banco de dados que não os contenha (de uma versão
mais antiga ou mais nova do `agy`) é ignorado em vez de falhar a listagem. Observe que o espaço de trabalho
registrado é o diretório de onde o `agy` foi iniciado, não uma raiz de checkout, de modo que uma
conversa iniciada um nível acima não é oferecida dentro de um subdiretório.

`agy` não aceita ID escolhido pelo chamador para uma nova conversa, portanto uma inicialização nova
não injeta seletor e o ID é vinculado pelos hooks ou descoberto após a saída; uma
retomada vinculada passa `--conversation <id>`. `--continue` / `-c` é tratado como uma
escolha explícita do usuário e nunca é substituído. `--yolo` mapeia para
`--dangerously-skip-permissions`. Cargas úteis de etapas são blobs protobuf não documentados
e sem versão, de modo que o ai-memory não decodifica o texto da conversa: o ledger de eventos visíveis
para este harness vem da captura por hook de ciclo de vida, e a exportação de transcrição
falha com uma mensagem informando isso. O inicializador gerenciado aceita `antigravity`,
`antigravity-cli` e `agy`. O contrato nativo foi verificado contra o
Antigravity CLI v1.1.7. Antigravity não faz parte do conjunto de detecção automática
sem argumentos; informe-o explicitamente.

O GitHub Copilot CLI mantém cada sessão em
`$COPILOT_HOME/session-state/<session-id>/` (`COPILOT_HOME` tem como padrão
`~/.copilot`). O Copilot aceita oficialmente um UUID escolhido pelo chamador através de
`--session-id`, portanto uma inicialização nova gera um; uma retomada vinculada passa
`--resume=<id>`, na forma `=`, pois `--resume` aceita um valor opcional e
caso contrário consumiria o argumento seguinte. Um `--resume`, `-r`,
`--continue`, `--session-id` ou `--connect` fornecido pelo usuário é uma escolha explícita e nunca é
substituído; uma retomada por nome de sessão ou prefixo de ID é resolvida pelo Copilot e
vinculada a posteriori. A descoberta lê o primeiro registro de `events.jsonl`,
`session.start`, e requer que seu `sessionId` coincida com o nome do diretório UUID
e seu `context.cwd` coincida com o checkout. A exportação importa o texto tipado
de `user.message` (não a cópia `transformedContent` carregando data/hora
e contexto injetados), texto visível de `assistant.message` e `toolRequests`,
a saída visível para o modelo de cada `tool.execution_complete` (ou a mensagem
de erro de uma chamada com falha) e resumos bem-sucedidos de `session.compaction_complete`.
Prompts de sistema, raciocínio oculto (`reasoningText`,
`reasoningOpaque`, `reasoningBlocks`), instantâneos de mensagens do modelo, saída de hooks
(onde o próprio pacote de inicialização do ai-memory é registrado), prompts de permissão e
telemetria de sessão são excluídos. O diário permaneceu append-only durante retomadas
e compactações nos testes, mas isso não é documentado, portanto o cursor armazena um
hash de prefixo e reproduz com IDs de evento por hash de conteúdo se o arquivo for reescrito.
O contexto é entregue pelo hook SessionStart existente do Copilot através do
seu `additionalContext` de nível superior. `--yolo` mapeia para o `--yolo` nativo do Copilot;
um `--yolo` ou `--allow-all` existente não é duplicado. O inicializador gerenciado
aceita `copilot` e `copilot-cli`: `run` inicia apenas harnesses de linha
de comando, portanto o nome simples aqui significa a CLI, enquanto `install-mcp --client
copilot` continua significando o Copilot do VS Code. O contrato nativo foi verificado
contra o GitHub Copilot CLI 1.0.92. O Copilot não faz parte do conjunto de
detecção automática sem argumentos; informe-o explicitamente.

O Crush não precisa de instalação de hooks do ai-memory para o modo gerenciado. O inicializador lê
seu contexto único no servidor, copia o JSON global do Crush que a inicialização
leria (`$CRUSH_GLOBAL_CONFIG/crush.json`, senão
`$XDG_CONFIG_HOME/crush/crush.json`, senão `~/.config/crush/crush.json`, com
entradas de `--env` primeiro e um valor em branco contando como não definido) para um diretório
temporário privado, anexa um caminho efêmero de contexto e aponta o processo filho para
esse diretório com `CRUSH_GLOBAL_CONFIG`. Quando esse JSON não lista nenhum
`global_context_paths`, o inicializador primeiro adiciona o `CRUSH.md` e o `AGENTS.md` que o
Crush teria carregado por padrão, de modo que o pacote não os substitui. Um
`crushrc` global adjacente a esse JSON é mantido por um `crushrc` gerado no
diretório temporário que o inclui a partir de seu próprio diretório, onde o Crush
o executa. A entrega é confirmada
apenas após o início do filho, para que uma falha no spawn não perca o pacote. A
configuração original não é modificada. O ai-memory abre o banco de dados do projeto em modo somente leitura;
o processo do Crush iniciado continua suas gravações normais de sessão nativa.

O wrapper de shell do Docker para Linux/macOS não pode inspecionar projetos do host nem executar um
agente do host de dentro de seu contêiner auxiliar. Para `run`, `show`, `continue`,
`resume` e `workstreams`, ele baixa a versão nativa correspondente em
`${XDG_DATA_HOME:-~/.local/share}/ai-memory/native-runner`, verifica o
checksum SHA-256 publicado e executa esse cliente no host. O pacote
`hooks/` da versão é mantido ao seu lado para que o auto-wire possa preparar scripts de hook em um host
onde o `install-hooks` nunca foi executado. O cliente reside com os dados do ai-memory no host
em vez de ficar sob `~/.cache`, porque a configuração de hooks conectados via auto-wire o executa
diretamente: uma limpeza de cache não pode interromper a captura. Um cliente baixado por um wrapper
mais antigo permanece em `~/.cache/ai-memory/native-runner`, e hooks configurados via auto-wire a partir dele
mantêm esse caminho até que uma versão mais recente do cliente configure novamente via auto-wire. Para movê-los agora,
execute novamente `ai-memory install-hooks --agent <agent> --apply` e exclua o diretório
antigo. Defina `AI_MEMORY_NATIVE_BIN=/path/to/ai-memory` para usar uma
versão nativa específica. Instalações nativas por pacote, release e código-fonte não precisam de
shim. No Windows nativo, use o `ai-memory.exe` publicado ou compile a partir do código-fonte.

O wrapper intercepta todos os cinco comandos antes do Docker e preserva as variáveis de ambiente
`PATH`, `AI_MEMORY_SERVER_URL` e de autenticação do host. O log de inicialização do
cliente nativo mostra `server_url` bem como seus caminhos de configuração locais; `data_dir` e
`bind` descrevem padrões locais e não substituem um servidor remoto configurado.
Se os logs mostrarem
`data_dir=/data` seguido por `starting managed ... No such file or directory`,
o wrapper instalado está desatualizado e enviou o comando para o contêiner auxiliar.
Execute `ai-memory upgrade` na máquina cliente. Um servidor remoto/homelab deve ser
atualizado separadamente.

Em uma saída normal, o ai-memory importa a transcrição e encerra a concessão (lease) antes
de retornar. Falhas tratadas de preparação, inicialização ou importação cancelam a concessão
imediatamente. Uma nova inicialização tenta novamente um conflito de workstream ativo brevemente para que o
inicializador anterior possa terminar; se outro harness estiver genuinamente em execução,
o conflito permanece e escritores concorrentes continuam sendo rejeitados.

### Recuperação de concessão (lease)

Um inicializador que encerra sem liberar sua concessão (morto, seu terminal
fechado, ou uma sandbox como o ai-jail desfeita) deixa o workstream retido
até que essa concessão expire. Uma reinicialização interativa (stdin e stderr são
terminais) não falha mais por causa disso: o conflito relata a expiração da concessão, de modo que
o ai-memory informa quem a detém e espera que ela expire (no máximo uma concessão,
~90 segundos; `Ctrl+C` aborta), e então inicia normalmente. Se o detentor renovar a
concessão enquanto você espera, trata-se de um inicializador ainda em execução, e você recebe um
erro. Encerre-o, ou passe `--new <name>` para um workstream separado. A
verificação de ocupado do servidor continua sendo o único árbitro: o inicializador em espera nunca expulsa
outra execução. Inicializações não interativas (scripts, hooks, CI) mantêm a janela curta de
nova tentativa e falham rapidamente em vez de travar. Interrupções
de terminal continuam alcançando o processo filho enquanto o pai permanece vivo para finalizar
ou cancelar a execução.

Quando você souber que o inicializador anterior não existe mais e não quiser esperar pela concessão,
force sua expiração explicitamente:

```bash
ai-memory run --force-unlock codex
# The exact wrapper flag is also accepted after the harness name.
ai-memory run codex --force-unlock
```

A substituição é atômica e limitada ao mesmo operador autenticado durável;
em operação de usuário único ou sem atribuição de identidade, ambas as execuções devem ser
não atribuídas. A execução ativa de um operador diferente continua sendo recusada. O comando
expira apenas a concessão gerenciada; ele não envia sinal nem encerra um processo nativo.
Se o inicializador anterior estiver de fato vivo, seus heartbeats posteriores e finalização serão
rejeitados, e o final de sua transcrição pode não ser importado. Use
`--force-unlock` apenas após verificar que o inicializador parou. Servidores mais antigos
não atendem ao pedido e retornam uma recusa, portanto atualize o servidor bem como
o cliente antes de confiar nesse caminho de recuperação.

Antes que o processo filho inicie, `Ctrl+C` no seletor de sessão nativa cancela a
execução adquirida e sai sem exigir Enter ou adotar a sessão selecionada.
O inicializador aguarda a resposta de cancelamento do servidor. Um erro de requisição é
relatado e deixa a concessão expirar dentro de sua janela normal de 90 segundos;
um servidor que aceita a requisição mas nunca responde ainda pode manter o inicializador
aguardando. Os heartbeats cessaram, portanto a concessão em si ainda expira.

Enquanto o seletor de harness ou de sessão nativa estiver aberto, uma interrupção temporária do servidor
produz um aviso curto em vez de imprimir cada heartbeat com falha. O
inicializador continua sondando a cada 30 segundos com um timeout de requisição de 10 segundos para que a
concessão de 90 segundos permaneça segura durante reinicializações comuns do servidor. Falhas repetidas
são silenciosas; quando o servidor responder novamente, um aviso de recuperação confirma que os
heartbeats foram retomados. O harness nativo permanece utilizável durante toda a interrupção.
Se a interrupção exceder uma janela de concessão, o inicializador original pode renovar sua execução
apenas enquanto nenhum inicializador mais novo tiver reivindicado o workstream. Uma preparação substituta,
cancelamento, finalização ou operação destrutiva continua sendo terminal para a execução antiga.

A redação de interrupção é reservada para um servidor que o inicializador não conseguiu alcançar. Quando
o servidor responde a um heartbeat e o rejeita, o aviso exibe o próprio motivo
do servidor. Um carimbo de data/hora expirado isoladamente não é esse caso: o `heartbeat`
renova qualquer execução cujo estado ainda seja `active`, de modo que o inicializador original recupera
sua própria concessão após uma interrupção ou suspensão do host. A rejeição chega quando a
execução deixa de estar ativa, por ter sido finalizada, cancelada ou expirada por uma
preparação posterior. Esse último é o caminho comum em uma estação de trabalho, já que a
primeira instrução de toda preparação expira cada execução ativa cuja concessão já tenha
passado. Um inicializador que ficou suspenso além de sua concessão continua renovando
até que qualquer outro `run` prepare um workstream, e então lê `managed run lease
is not active` de um servidor que está acessível e ocioso. Relatar isso como uma
interrupção levaria o operador a procurar por um servidor que nunca ficou fora do ar.

Se o cliente for encerrado sem limpeza, como por exemplo via `kill -9`, sua concessão
expira em 90 segundos. Uma execução gerenciada posterior inicia do último cursor confirmado
do adaptador, de modo que sessões nativas já vinculadas possam importar a cauda ausente
sem duplicar eventos anteriores. Uma falha de servidor ou autenticação antes da inicialização
do processo é fatal; o ai-memory não inicia silenciosamente um agente não gerenciado.

## Limites de privacidade e armazenamento

Importações de finalização, incluindo novas tentativas de execuções concluídas, usam a identidade resolvida pela
autenticação HTTP. Um chamador deve ser o proprietário da execução (ou acessar uma execução compartilhada
sem proprietário/NULL). Usuários do banco de dados devem ter acesso atual de Gravação ao seu
espaço de trabalho/projeto real. Outro escritor no mesmo projeto não pode finalizar uma execução
atribuída a um operador
diferente. O root sem um ator acessa apenas execuções compartilhadas; a autenticação root
não ignora a propriedade. Operadores autenticados atrás do proxy confiável configurado
mantêm sua política de acesso ao projeto existente sem exigir um usuário do banco de dados.
Cabeçalhos de ator sem autenticação de proxy e metadados de transcrição não podem
fornecer essa autoridade. Desativar o login humano de um usuário não revoga uma chave de API
ativa; a autenticação HTTP verifica a chave, e a finalização verifica se o
usuário do banco de dados ainda existe e tem acesso atual ao projeto.

Com a autenticação desativada, mas usuários do banco de dados ainda presentes, requisições de finalização anônimas
para projetos restritos são recusadas. O prepare mantém seu caminho de
compatibilidade existente e ainda pode ser bem-sucedido. Concluir a execução requer
autenticação e um chamador que satisfaça as verificações de propriedade e Gravação acima.

O servidor verifica o acesso antes de ler o status da execução ou gravar um segmento bruto,
e verifica novamente na transação do escritor SQLite antes de qualquer importação, cursor,
vínculo ou atualização da execução. A transação lê os usuários atuais, modo do projeto, criador
e concessões emparelhadas; falhas de SQL e escopo malformado ou ausente falham fechados.
Essa resolução mais estrita aplica-se apenas a importações de finalização. Uma execução ativa ainda pode
terminar após o carimbo de data/hora de sua concessão (lease) ter passado, e uma nova tentativa autorizada
de execução concluída importa zero eventos. Execuções canceladas ou substituídas permanecem expiradas e recusadas.

Segmentos brutos são gravados antes da transação SQL. Uma revogação de Gravação após
o preflight pode deixar um segmento bruto higienizado sem um evento indexado. A
recusa do escritor deixa o SQL inalterado; a persistência de arquivo bruto e o SQL não são
uma operação atômica.


Os adaptadores gerenciados do ai-memory não gravam nos armazenamentos privados de Claude, Codex, OpenCode, Pi, Crush,
Kimi Code, Command Code, Kiro, OMP, Grok, Antigravity ou Copilot. O harness
iniciado mantém a posse normal de suas próprias gravações de sessão. Adaptadores leem apenas
formatos de sessão locais documentados ou observados. Credenciais de provedores, conteúdo criptografado,
registros de prompt de sistema/desenvolvedor e raciocínio oculto não são copiados. O
sanitizador do servidor é executado antes que tanto o ledger FTS do SQLite quanto os arquivos imutáveis sob
`<data_dir>/raw/workstreams/<workstream-id>/segments/` sejam gravados.

O ledger carrega a continuidade operacional e não substitui a wiki em
markdown. Decisões duráveis, regras, procedimentos e fatos do projeto ainda
pertencem às páginas da wiki através de consolidação ou gravações duráveis explícitas.

## Renomeações de projeto e de diretório

`ai-memory rename-project --from OLD --to NEW` altera apenas o nome do projeto no
servidor. Caminhos da wiki são indexados por UUID, portanto ela não move nenhum diretório do servidor, checkout
de origem ou sessão nativa de harness. Se o próprio caminho de checkout de origem for
renomeado, localizadores de sessão por caminho absoluto usados por Claude Code, Codex, OpenCode,
Pi, Kimi Code (`cwd` de `state.json` ou o legado `workDir`), Command Code (`cwd` do
cabeçalho v3), Kiro v2
(`cwd` de `<uuid>.json`), Kiro v3 (`workspacePaths` de `session.json`), OMP,
Antigravity e Copilot (`context.cwd` de `session.start`) ainda podem fazer referência
ao caminho antigo; o banco de dados `.crush` local do projeto do Crush
se move com o checkout.

Não há uma API portátil e aceita que reescreva o localizador de projeto privado de cada
harness. O ai-memory, portanto, não modifica esses armazenamentos nem equipara silenciosamente
um checkout renomeado a outro clone do mesmo remote. Seletores nativos explícitos
ainda têm precedência e podem recuperar uma sessão quando esse harness oferecer suporte a
retomada entre diretórios; o OpenCode também fornece seu próprio fluxo de exportação/importação. Para
um checkout renomeado, use um harness explícito e seu seletor documentado de sessão
para alimentar o novo workstream gerenciado. Mantenha o caminho de checkout antigo disponível até
que a recuperação seja verificada. A descoberta automática intencionalmente exige que o checkout
registrado corresponda exatamente.

### Um repositório que ganhou seu remote após o início da captura

O nome de um projeto vem do remote git, ou do nome da pasta quando o
checkout não tem remote. Sessões capturadas antes da adição do remote permanecem no
projeto nomeado pela pasta, enquanto o `ai-memory run` resolve aquele derivado do
remote, de modo que retomar uma dessas sessões não consegue vinculá-la à execução. O servidor
recusa o vínculo com o conflito `native_session_in_other_project`, que
cita ambos os projetos (apenas para o operador e harness que registraram a
sessão).

Quando o projeto nomeado pela pasta é o nome antigo do projeto derivado do remote e
o projeto derivado do remote ainda não tem sessões nem páginas, a reparação não perde
nada. O servidor move a identidade do repositório do projeto vazio para
o nomeado pela pasta, atribui a ele o nome derivado do remote e mantém o projeto vazio
como `<name>-split-<date>`. O ID do projeto é mantido, portanto sessões,
observações e páginas acompanham, e a captura baseada em identidade e inicializações posteriores
os alcançam. Os links de checkout deste cliente e a vinculação salva do projeto são
reapontados também.

Em um terminal, o `ai-memory run` explica isso e oferece o reparo (padrão
sim), e então inicia a sessão novamente. Fechar a entrada do prompt sem uma
resposta o cancela; pressionar Enter aceita o padrão. Fora de um terminal, ele para
antes do agente iniciar e imprime o comando:

```
ai-memory repair-split --workspace <ws> --project-id <folder project id> \
  --from-project-id <remote project id>
```

O mesmo comando repara um par que foi renomeado manualmente, onde a renomeação
isolada deixou a identidade no projeto arquivado. Ele recusa quando o
projeto derivado do remote possui suas próprias sessões ou páginas, ou uma execução gerenciada
ainda estiver ativa nele, porque unilos ocultaria dados. Compare os dois
projetos primeiro ([operações de ciclo de vida](lifecycle-ops.md)).

## Aceitação manual

O executor de aceitação opcional exercita casos extremos do inicializador e depois orquestra
as CLIs instaladas localmente do Claude, Codex, OpenCode, Pi, Crush, OMP, Kimi, Command
Code, Grok, Antigravity e Copilot através de um workstream real:

```bash
scripts/managed-workstream-acceptance.sh
```

Ele é deliberadamente separado do CI porque usa credenciais locais dos harnesses
e chamadas a modelos. Configurações de hooks, armazenamentos de sessões nativas, o servidor ai-memory e
o fixture do Git são isolados sob um diretório temporário. Claude, Codex e
OpenCode recebem apenas material de autenticação copiado; OMP recebe um diretório
de agente temporário com backups, consistentes para leitura, do banco de dados de credenciais/modelos e
configurações copiadas. Crush usa sua configuração global de provedores existente e um
banco de dados de projeto isolado. Kimi Code executa com um `$KIMI_CODE_HOME` isolado
preenchido com a configuração de provedor do operador. Command Code executa com um
`HOME` isolado contendo apenas `auth.json` e `config.json`. Antigravity executa
com um `HOME` isolado contendo apenas os arquivos de OAuth e configurações do operador.
Copilot executa com um `$COPILOT_HOME` isolado contendo apenas `config.json`. A
fase determinística também cobre adoção na primeira execução, seleção em modo sem argumentos e
falha em diretório vazio, `--yolo` do wrapper, exclusão de concessão, limpeza de contexto
do Crush, viagens de ida e volta (round trips) de armazenamento/retomada/importação em modo simulado (fake) de Kimi e Command Code, uma
viagem de ida e volta de hook/vínculo/retomada de Antigravity, uma viagem de ida e volta de
armazenamento/hook/retomada/importação em modo simulado do Copilot, uma viagem de ida e volta de
armazenamento/retomada/importação em modo simulado do Kiro v2,
a viagem equivalente em armazenamento aninhado do Kiro v3 com recuperação transparente de motor,
exclusão de trajetória privada e a
proteção de workstream estabelecido contra sessões obsoletas. A viagem simulada do Kimi
também exclui a sessão nativa vinculada e verifica a recuperação automática
de sessão nova e reapontamento.
Criação de sessão nativa, extração somente leitura, injeção entre harnesses e
caminhos de retorno por retomada são todos exercitados. A execução no host do wrapper Docker e a
preservação da URL remota são cobertas separadamente pelos testes de
empacotamento de `ai-memory-cli`.

O Kiro é intencionalmente ignorado no loop com modelo real via script. Seu
modo `--no-interactive` grava um armazenamento SQLite v1 diferente, enquanto ambos os adaptadores
gerenciados leem os diários interativos v2/v3. A aceitação do Kiro autenticado,
portanto, permanece interativa. Para a v2, execute `ai-memory run --new kiro-v2-accept
kiro`, insira um prompt exclusivo, saia normalmente, e depois execute `ai-memory run
--workstream kiro-v2-accept kiro-cli` e verifique se o mesmo UUID é retomado. Para a v3,
repita com um novo workstream e `kiro --v3`; a segunda inicialização simples do `kiro`
deve adicionar transparentemente `--v3 --resume-id <sess_uuid>`. Pesquise em ambos os ledgers
de workstream pelas respostas visíveis exclusivas do assistente. Registre a versão do Kiro e
higienize arquivos de metadados/eventos antes de alterar o esquema de qualquer um dos fixtures.

A fase de harness real trata o modelo como o sistema em transporte, não como
o oráculo do teste. Para cada perna, ela registra a sequência anterior do ledger e então
requer um evento de assistente recém-importado de harnesses com transcrições
nativas legíveis. Para o Antigravity, ela requer em vez disso o link exato da conversa
nativa e uma nova observação correlacionada do hook de inicialização, pois seu protobuf
de trajetória privada deliberadamente não é decodificado. Quando um delta de contexto é
esperado, ela primeiro verifica se o endpoint anterior do ledger é mais recente
do que o cursor de entrega daquele harness, e então requer que a execução gerenciada mais recente
relate esse endpoint exato como `sync_through` com `context_delivered = 1`. Ela
não exige que o modelo cite um sentinela anterior: o Claude Code pode
externalizar um resultado grande de hook para um arquivo, e a escolha do modelo em ler
esse arquivo não é um sinal determinístico de continuidade. Os fixtures determinísticos simulados
entre harnesses de Grok, Antigravity e Copilot exercitam o mesmo helper de asserção
sem credenciais ou chamadas ao modelo.

Defina
`AI_MEMORY_ACCEPTANCE_HARNESSES="command-code codex"` para selecionar uma
viagem de ida e volta Command-Code-para-Codex-para-Command-Code, ou
`AI_MEMORY_ACCEPTANCE_HARNESSES="antigravity codex"` para selecionar uma
viagem Antigravity-para-Codex-para-Antigravity (`agy` e `antigravity-cli` são
aliases aceitos), `AI_MEMORY_ACCEPTANCE_DETERMINISTIC_ONLY=1` para ignorar chamadas
ao modelo, ou
`AI_MEMORY_ACCEPTANCE_KEEP=1` para reter todos os logs e dados temporários.

## Executando dentro do Herdr

[Herdr](https://herdr.dev/) é um gerenciador de espaço de trabalho em terminal que acompanha qual
agente executa em cada painel. Ele identifica o agente a partir do processo em primeiro plano
do painel, recorrendo à correspondência da saída de tela do próprio agente contra
manifestos por agente.

`ai-memory run` fica em uma posição desconfortável entre os dois. O processo em primeiro plano é o
wrapper e o agente é seu filho (um grupo de processos cujo líder é
`ai-memory`), portanto a detecção de processos não encontra o agente. O painel se resolve
apenas quando o harness exibe um título que o Herdr reconhece, o que pode ocorrer bem depois
da inicialização e pode nunca acontecer para um harness cuja saída não corresponda a nenhum manifesto.
Até lá, o painel de agentes do Herdr não mostra nada para esse painel.

O ai-memory deliberadamente não tenta corrigir isso internamente. A dica do Herdr
para comandos wrapper, `HERDR_AGENT`, tem escopo restrito ao processo em primeiro plano
do painel, e um processo não pode alterar seu próprio ambiente após o exec, de modo que o
wrapper não tem como descrever a si mesmo para o Herdr quando já está em execução.
Definir a variável no agente que ele gera a colocaria em um local onde o Herdr
não a procura.

Duas opções funcionam hoje.

Indique o agente no comando, onde o Herdr verifica:

```bash
HERDR_AGENT=codex ai-memory run codex
```

Ou instale a integração de agente do próprio Herdr, que é a melhor opção:

```bash
herdr integration install codex
herdr integration install claude
```

Uma integração instalada relata a identidade do agente e o estado do ciclo de vida através do
socket do Herdr e é autoritativa, independentemente da detecção de processos, de modo que o
wrapper não importa mais. Ela também aprimora o que o Herdr pode mostrar: sinais reais
de `idle` / `working` / `blocked` em vez de inferir da tela,
a qual não consegue enxergar `blocked` com confiabilidade.

Esses são mecanismos separados gravando em arquivos separados: a integração do Herdr
instala seu próprio script de hook (`~/.claude/hooks/herdr-agent-state.sh` para o Claude
Code, `~/.codex/herdr-agent-state.sh` para o Codex), enquanto os hooks de ciclo de vida do
ai-memory residem na configuração própria do agente. O instalador do ai-memory preserva entradas
externas em vez de substituí-las, de modo que executar novamente `ai-memory install-hooks`
não removerá as do Herdr. Faça backup da configuração do agente e faça um diff após
instalar qualquer um deles se quiser ter certeza de que o outro sobreviveu.
