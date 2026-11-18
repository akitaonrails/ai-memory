# Uso no dia a dia

Esta página aborda o que acontece depois que o ai-memory é instalado: handoffs,
recuperação pós-compactação, consultas proativas de memória, a UI web e o
pacote de trecho de roteamento gerenciado + Agent Skills.

## Exclusões de captura

Para manter eventos reconhecidos de ferramentas de arquivos sob caminhos privados fora do ai-memory antes
que sejam enfileirados em spool ou enviados, configure `[capture] ignore_paths` no
`.ai-memory.toml` mais próximo. A gramática canônica, limitações, matriz de compatibilidade, requisitos de
atualização e o comando local seguro `--check-capture` estão
[na referência do arquivo marcador](marker-file.md#exclusões-de-captura). Comandos de shell
também são combinados por seus argumentos de caminho (`cat docs/adr/*.md`), lexicalmente. Este
não é um filtro DLP geral de prompt/saída.

## Handoff entre agentes

Você normalmente não cria handoffs manualmente. Com hooks de ciclo de vida
instalados, a captura de término de sessão grava o handoff e o próximo
hook de início de sessão o busca. Handoffs manuais abrangem todo o projeto e têm
precedência sobre handoffs automáticos de SessionEnd. Entre handoffs automáticos que
correspondem ao diretório receptor pelo limite de caminho, o mais novo é entregue;
criar um novo handoff automático expira handoffs automáticos abertos anteriores
daquele diretório exato, e a aceitação expira handoffs automáticos correspondentes mais
antigos sem perturbar handoffs manuais ou trabalhos pendentes de diretórios
irmãos.

```text
$ claude
> Working on the auth refactor. JWT rotation is broken; trying session cookies.
[work for an hour]
> /exit

$ codex   # in the same directory, later
[SessionStart hook fetches the handoff; Codex sees it before your prompt.]
> Picking up: you were investigating session cookies as an alternative...
```

Se um agente possui MCP, mas não possui interface de hook de ciclo de vida, peça a ele para chamar
`memory_handoff_begin` antes de sair. O próximo agente com hooks ainda pode
consumir esse handoff automaticamente. O Grok o mostra como `PostToolUse`
`additionalContext` após a primeira ferramenta. `install-hooks --agent grok --apply`
e `install-skills --scope global --agent grok` gravam
`$GROK_HOME/rules/ai-memory-handoff.md` para que o primeiro turno chame
`memory_handoff_list` (somente leitura; o PostToolUse então reivindica a linha). A regra
descreve a invocação do Grok — `use_tool` com a chave de catálogo qualificada
`ai-memory__memory_handoff_list` — porque o Grok acessa ferramentas MCP somente por meio
dessa metaferramenta, e coloca essa chamada antes de qualquer leitura de arquivo, corrigindo
`workspace`/`project` a partir de `./.ai-memory.toml` apenas quando a lista estiver vazia ou for um erro de escopo.
Até que essa
ferramenta seja executada, ou se a regra estiver ausente e a sessão nunca chamar uma ferramenta, chame
`memory_handoff_list` e depois `memory_handoff_accept` com o `handoff_id`
listado. O Zero deve listar e depois aceitar na retomada. A listagem não reivindica
a linha.

`memory_handoff_accept` informa por que não retornou nenhum handoff. Seu `status` é
`claimed` quando a chamada pegou um, `consumed_by_hook` quando o próprio
SessionStart da sessão chamadora já o fez (o handoff está no contexto dessa
sessão), e `none_pending` quando nada resta para reivindicar. Apenas um cliente que
encaminha seu id de sessão em chamadas MCP pode receber a informação `consumed_by_hook`: Claude
Code por meio de `install-mcp --session-aware`, ou OpenCode 2. Qualquer outro cliente
recebe `none_pending` depois que o hook consumiu o handoff, de modo que seu agente ainda
verifica seu contexto para o bloco entregue primeiro.

Em um servidor que distingue operadores, handoffs pertencem ao seu criador por
padrão: a próxima sessão daquele operador vê suas próprias linhas mais aquelas
deliberadamente compartilhadas, nunca as de um colega de equipe. Use `shared: true` em
`memory_handoff_begin` apenas quando o bastão for destinado a qualquer pessoa no
projeto. A recuperação autorizada como root pode passar `any_owner: true` para
`memory_handoff_accept` ou `memory_handoff_cancel`; chamadores normais não podem usar
esse modificador.

Handoffs são transferências para a próxima sessão, não um barramento de mensagens em tempo real entre agentes que
ainda estão em execução. Em particular, o Antigravity busca um handoff apenas uma vez por
conversa: no `SessionStart`, ou em builds sem ele, no `PreInvocation`
do primeiro turno com invocação zero. Seus turnos posteriores e chamadas de modelo nunca
buscam um, portanto, um handoff criado posteriormente nessa conversa permanece aberto em vez
de ser consumido pela próxima chamada de modelo de seu criador.

Se um agente criar um handoff por engano, cancele-o imediatamente com
`memory_handoff_cancel` e o `handoff_id` retornado por
`memory_handoff_begin`. O cancelamento marca o handoff como expirado, de modo que o próximo
hook de início de sessão não consumirá contexto desatualizado.

### Oferecer em vez de reivindicar no início da sessão

Por padrão, `SessionStart` reivindica um handoff pendente automaticamente, conforme
descrito acima. Com várias linhas de trabalho simultâneas no mesmo
projeto, uma sessão não relacionada (uma tarefa diferente, um harness diferente ou uma
inicialização não interativa) pode consumir um bastão destinado a uma sessão de acompanhamento
específica. Defina `claim_on_session_start = false` sob `[handoff]` no
`config.toml` para interromper isso:

```toml
[handoff]
claim_on_session_start = false
```

`SessionStart` então deixa o handoff aberto e renderiza um aviso não
consumível em vez disso, informando o `handoff_id` exato, o agente que o deixou e
sua idade. Nunca inclui o resumo armazenado, perguntas abertas ou próximas etapas (mesmo
padrão de segurança que o aviso da caixa de entrada: esse conteúdo é gravado por qualquer agente
ou operador que encerrou a sessão anterior, e um aviso injetado no contexto de
inicialização não pode ser deliberadamente ignorado como deixar de chamar `memory_handoff_accept`
pode). Pegue-o explicitamente:

```
> memory_handoff_accept handoff_id=<the id from the notice>
```

Isso abrange todo o servidor: todo operador no servidor recebe o mesmo comportamento.
Reinicie o servidor após alterar `config.toml`; a configuração é carregada uma vez
na inicialização. O padrão (`true`) permanece inalterado para todas as instalações existentes.

### Desativando a criação automática de handoffs no término da sessão

Por padrão, todo término de sessão não gerenciado cria um handoff aberto para a próxima sessão. Em uma configuração de operador único, onde sessões consecutivas fazem parte do mesmo fluxo contínuo de trabalho ou onde bastões não são desejáveis, a criação automática pode ser desativada, preservando a página de resumo da sessão e a consolidação:

```toml
[handoff]
create_on_session_end = false
```

Quando definido como `false`, `SessionEnd` grava `sessions/<id>.md` e enfileira a consolidação como de costume, mas ignora a criação de uma linha de handoff aberta (#1043). O ponto de verificação por turno do OpenCode, que substitui o término de sua sessão, da mesma forma atualiza a página de resumo sem criar ou atualizar um bastão. Bastões explícitos criados com `memory_handoff_begin` e execuções de workstream gerenciadas permanecem inalterados. Como `claim_on_session_start`, essa configuração abrange todo o servidor.

Para ignorar o handoff automático apenas para uma execução, como um turno de assistente headless (um revisor somente leitura, uma sondagem, uma etapa de um relay programado), defina `AI_MEMORY_AUTO_HANDOFF=off` no ambiente em que o harness e seus hooks são executados:

```bash
AI_MEMORY_AUTO_HANDOFF=off claude -p "Review the parser change"
```

O hook o encaminha como `auto_handoff=0` nos eventos dessa sessão, de modo que seu encerramento (ou ponto de verificação de turno do OpenCode/Antigravity) grava a página de resumo e enfileira a consolidação sem deixar um bastão. Outras sessões, incluindo aquelas em execução simultânea no mesmo projeto, mantêm o comportamento de `create_on_session_end` do servidor. `off`, `0`, `false` e `no` desativam; qualquer outro valor, ou deixá-lo indefinido, não altera nada. Ele pode apenas remover o bastão automático desta sessão, nunca criar um que o servidor não criaria. O binário nativo `ai-memory hook`, os pacotes de hooks shell e PowerShell e os plugins TypeScript gerados leem-no (#1951).

## Recuperação pós-compactação

Quando o Claude Code ou o Codex compactam seu contexto de trabalho, o
hook `PreCompact` dispara e o ai-memory grava uma página
`sessions/<id>.md` nova resumindo a sessão até o momento. A resposta
do hook retorna após essa página heurística; quando um provedor está
configurado, a reescrita por LLM é enfileirada e aplicada pelo mesmo worker
durável que o SessionEnd usa. Com um provedor, a gravação do ponto de verificação também passa
pela admissão de `consolidate`, de modo que uma política que recuse a consolidação
não deixa página e não enfileira nada. Uma reescrita ainda enfileirada quando a
sessão termina é executada uma vez sobre toda a sessão, mesmo com
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END` desativado, substituindo o resumo baseado
em regras; com a opção ativada, a tarefa de SessionEnd a substitui. Sem
a ativação, uma sessão que termina enquanto essa reescrita ainda está em execução
mantém a reescrita como a página mais recente, podendo deixar de fora o final da
sessão; o resumo de SessionEnd permanece acessível como a versão
substituída. Após a
compactação, o agente pode recuperar o resumo por meio de `memory_recent`, mesmo
que seu histórico bruto de chat tenha sido compactado.

Páginas de sessão geradas contêm `session_id` e `agent` no frontmatter. O
valor de `agent` é o harness de origem armazenado na sessão (por exemplo,
`claude-code` ou `codex`), não o cliente ou operador que solicitou posteriormente uma
consolidação. Pontos de verificação e versões substitutas, portanto, mantêm a mesma
origem. Chamadas manuais a `memory_write_page` e `ai-memory write-page` não
inferem um agente.

### Recarregar o contexto após uma compactação

Uma compactação substitui a conversa por um resumo escrito pelo modelo, e o
resumo pode perder ou suavizar o que você solicitou. O início da sessão vai
primeiro: a tarefa original, suas restrições, as primeiras decisões. Em uma sessão
longa, ou em um modelo local com uma janela de contexto em torno de 200k tokens, isso
acontece repetidamente. Todas as interfaces de instruções que o ai-memory instala (o bloco
`AGENTS.md`/`CLAUDE.md`, o handshake MCP e a habilidade de handoff) orientam
o agente a se refundamentar no ai-memory após uma compactação antes de continuar,
e a confiar em seus prompts gravados em vez do resumo. Alguns harnesses também têm
a evidência injetada automaticamente.

#### Automático: a âncora pós-compactação

Onde o harness possui um hook que é executado após uma compactação, o ai-memory injeta uma
**âncora pós-compactação** para a sessão, construída por
`GET /recap?session_id=<id>&mode=anchor`:

- um cabeçalho indicando o id da sessão e uma linha informando ao agente que o
  resumo pode ter perdido detalhes e que seus prompts abaixo são a fonte da
  verdade para a tarefa e suas restrições;
- os primeiros prompts da sessão, literais e os mais antigos primeiro, que contêm a
  tarefa original, e depois seus prompts mais recentes, os mais novos por último. A marcação do harness é
  omitida como na recapitulação. Os primeiros prompts recebem até cerca de 45% do orçamento;
  qualquer coisa cortada ou descartada traz uma nota visível;
- um rodapé apontando para o registro completo (`memory_read_session_observations`
  com o id da sessão e `kinds: ["user-prompt","stop"]`) e para
  `memory_query` para decisões.

O orçamento padrão é de 2.000 caracteres (limitado entre 800 e 4.000), pequeno o suficiente
para um contexto restrito. A âncora cobre toda a sessão, por isso ainda funciona
após a quinta compactação. Ela é somente leitura: uma compactação não reivindica nenhum handoff
e não reenvia o pacote de início de sessão, digest ou briefing. Quando o
servidor está lento ou inacessível, o hook não imprime nada e o harness continua
sem ele.

A âncora provém apenas do `ai-memory hook` nativo. Qualquer `SessionStart`
com origem `compact`, de qualquer harness, não reivindica nada: o hook nativo e
os hooks de fallback shell e PowerShell em `hooks/` ignoram a busca de
handoff e imprimem sua resposta vazia habitual. Os hooks de fallback nunca injetam uma
âncora, portanto, uma instalação do Claude Code, Codex ou Droid que os execute depende das
instruções instaladas, como os harnesses na última linha abaixo.

| Harness | Caminho pós-compactação |
|---|---|
| Claude Code | Automático (hook nativo): `SessionStart` com origem `compact`, por meio de `hookSpecificOutput.additionalContext` |
| Codex | Automático (hook nativo): `SessionStart` com origem `compact`, como contexto de desenvolvedor antes da próxima solicitação ao modelo |
| Factory Droid | Automático (hook nativo): `SessionStart` com origem `compact`, anexado ao novo contexto de sessão |
| OpenCode V1 | Automático: o hook `experimental.session.compacting` adiciona uma linha a `output.context` solicitando ao resumo que mantenha o id da sessão e aponte para o ai-memory, e `session.compacted` arma a âncora para a transformação de sistema até a próxima compactação |
| OpenCode V2 | Automático: o hook `compaction` adiciona a mesma linha como uma parte `system`, e `session.compaction.ended` arma a âncora para o hook `context` até a próxima compactação |
| Gemini CLI, Cursor, GitHub Copilot CLI, Kimi Code, Grok Build, Kiro CLI, Cline, Muse Code e os demais | Apenas instruções: esses harnesses não possuem hook que possa adicionar contexto após uma compactação (o Muse relata uma origem `compact`, mas seu contrato de saída não foi publicado, de modo que seu início por compactação apenas ignora a reivindicação de handoff), portanto o agente segue as instruções instaladas e lê os prompts de sua própria sessão |

Quando nenhuma âncora chega, o agente chama `memory_read_session_observations`
com `session_id: "current"` e `kinds: ["user-prompt","stop"]`. O servidor
resolve `"current"` apenas para um cliente com reconhecimento de sessão (o cabeçalho de ponte
com reconhecimento de sessão ou o id de sessão nativo do OpenCode 2). Um cliente estático é recusado e
passa o id de sessão mostrado na âncora pós-compactação. Sem âncora, ele
omite `session_id` e confirma se os prompts da sessão retornada correspondem à sua
própria conversa antes de confiar neles. O aviso de início de sessão indica uma
sessão diferente, portanto, seu id nunca é o fallback aqui.

#### Manual: `reload context`

Inicie um prompt com `reload context` para colocar o trecho que a última compactação
descartou de volta à frente do agente, em qualquer harness cujos hooks possam responder a um
prompt:

```
reload context after last compaction, then finish the screenshots for guide 12
```

Os hooks identificam a frase no início do prompt (qualquer combinação de maiúsculas/minúsculas; uma frase
que apenas a mencione não conta) e injetam uma **recapitulação de compactação** construída
a partir do que o ai-memory capturou para esta sessão entre sua compactação mais recente
e a anterior (ou o início da sessão):

- seus prompts nesse trecho, literais, os mais antigos primeiro; marcações de harness e
  notificações de tarefas são omitidas. Quando nem todos couberem, os mais novos permanecem
  integrais, o próximo mais antigo é cortado para o espaço restante e os mais antigos são
  descartados com uma observação;
- a última resposta do agente antes da compactação. Respostas são capturadas apenas
  com a opção dupla de captura do assistente (`install-hooks --capture-assistant`
  mais `capture_assistant` no servidor); sem ela, a recapitulação informa isso;
- os handoffs abertos do projeto, listados e nunca reivindicados, os desta
  própria sessão primeiro.

O restante do prompt é executado como de costume com a recapitulação no contexto. Servir a
recapitulação não grava nada: nenhum handoff é reivindicado e nenhuma página ou observação extra
é adicionada (o próprio prompt é capturado como qualquer outro).

Buscar a recapitulação retém o prompt até que ela chegue. Em um servidor local
íntegro, isso leva milissegundos. Quando o servidor está lento ou inacessível, o
hook primeiro gasta até 3 segundos enviando eventos enfileirados e depois aguarda até
3 segundos pela recapitulação, de modo que o prompt pode levar cerca de 6 segundos para chegar ao
agente, que então o executa sem uma recapitulação. Prompts que não começam com
`reload context` nunca esperam por isso.

| Harness | Quando o agente vê a recapitulação |
|---|---|
| Claude Code | Com aquele prompt (`UserPromptSubmit` `additionalContext`, até 8.000 caracteres) |
| Codex | Com aquele prompt (contexto de desenvolvedor, até 6.000 caracteres para ficar abaixo do limite de pré-visualização de saída do hook do Codex) |
| OpenCode V1 / V2 | Com aquele prompt e chamadas de modelo posteriores, até a próxima compactação |
| Grok Build | Após o primeiro resultado de ferramenta daquele turno: o Grok descarta a saída de `UserPromptSubmit`. A recapitulação compartilha esse campo com um handoff entregue ao mesmo tempo e é dimensionada para caber; quando o handoff deixa pouco espaço, ela vem com o próximo resultado de ferramenta. Uma solicitação sem chamada de ferramenta expira após 30 minutos ou no término da sessão |

Uma sessão que ainda não foi compactada responde à recapitulação manual com um
aviso de uma linha. O registro completo continua disponível por meio de
`memory_read_session_observations` com o id da sessão e
`kinds: ["user-prompt", "stop"]`.

## Consultas proativas de memória

Hooks cuidam da captura sem requisições explícitas. Consultas proativas dependem de
o agente saber qual ferramenta MCP chamar para cada situação. Instale o
pacote de roteamento gerenciado uma vez: um trecho compacto sempre carregado direciona os agentes
às Agent Skills gerenciadas do ai-memory que contêm o roteamento detalhado de ferramentas.

| Você diz | O agente chama | Efeito |
|---|---|---|
| "Já discutimos X?" / "busque na memória por Y" | `memory_query` | RRF de FTS5 + entidade/grafo/vetor sobre páginas wiki compiladas, além de um stream de observações (ativo por padrão) que eleva a página de uma sessão quando apenas seus turnos capturados coincidem, seguido por classificação delimitada por autoridade de fonte e fallback para observações brutas em caso de falha de página. |
| Antes de propor uma arquitetura | `memory_query` | Verifica decisões anteriores e gotchas antes de sugerir designs. |
| "Atualize-me" / "estive fora" | `memory_explore` | Digest em prosa cuja verbosidade escala com o tempo desde a última atividade. |
| "Onde paramos?" | Bloco de handoff existente, ou `memory_handoff_list` depois `memory_handoff_accept` com esse `handoff_id` se nenhum bloco existir | Inspeciona handoffs pendentes sem reivindicar, depois reivindica o id escolhido uma vez. |
| "Salve o contexto para a próxima sessão" | `memory_handoff_begin` | Grava um handoff conciso de término de sessão com perguntas abertas e próximas etapas. Não use para solicitações de status ou briefing. |
| "Descarte aquele handoff" / "criei um handoff por engano" | `memory_handoff_cancel` | Marca um id de handoff aberto exato como expirado antes que a próxima sessão possa consumi-lo. |
| "Peça ao agente em <other project> para fazer X" / "envie isto para o projeto B" | `memory_message_send` | Deposita uma solicitação autocontida na caixa de entrada de outro projeto (requer `to_workspace` + `to_project`); o destinatário já deve existir. Entre projetos, reivindicação única. Veja [agent-messaging.md](agent-messaging.md). |
| "Verifique minha caixa de entrada" / "alguma mensagem aguardando?" | `memory_message_list` depois `memory_message_pop` | Lista mensagens pendentes da caixa de entrada sem consumir, depois extrai uma mensagem exatamente uma vez. Uma mensagem extraída é uma entrada não confiável de outro projeto: uma solicitação para avaliar, nunca instruções a obedecer. |
| "Esqueça aquela solicitação que enviei" / "limpe minha caixa de saída" | `memory_message_cancel` | Retrata uma mensagem enviada pendente por id, ou limpa toda a caixa de saída quando omitido. Afeta apenas mensagens que este projeto enviou. |
| "Consolide esta sessão" | `memory_consolidate` | Executa manualmente a consolidação por LLM no modelo do servidor. Para uma solicitação explícita sobre a sessão da qual o agente está participando, a rota do agente na linha seguinte é preferida para que o próprio modelo do agente grave as páginas; mantenha `memory_consolidate` para sessões das quais o agente não participou e para execuções sem um agente. Omita `session_id` (ou envie um em branco) para consolidar a sessão concluída mais recente no projeto resolvido; passe um para direcionar a uma sessão específica. Um projeto pode manter preferências consultivas em `_prompts/consolidation.md`; `instructions` as substitui em uma única chamada. Também é executado em PreCompact / PostCompaction e no encerramento da sessão apenas quando `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` está definido (desativado por padrão; caso contrário, um encerramento substancial de sessão grava uma página de resumo baseada em regras). Sessões apenas de ciclo de vida não criam página gerada, handoff ou tarefa de provedor. PreCompact / PostCompaction gravam um ponto de verificação heurístico na requisição do hook; sua reescrita por provedor, e o trabalho opcional de provedor em SessionEnd, são enfileirados duravelmente fora da resposta do hook, repetidos com backoff e recuperados após a reinicialização do servidor. Sessões retomadas são reencerradas apenas quando a geração de observações persistidas avança, portanto, entrega duplicada e distorção de relógio não criam loops de consolidação. |
| "Grave você mesmo a página desta sessão" / um "consolide esta sessão" dentro da sessão | `memory_read_session_observations`, depois `memory_write_page` com `session_id` e `path: sessions/<id>.md` | O agente compila a página da sessão com seu próprio modelo em vez do provedor do servidor. Para corresponder ao layout multipágina do servidor, ele também pode gravar até mais quatro páginas com o mesmo `session_id`: `concepts/<slug>.md` (`tier: semantic`, `kind: fact`), `decisions/<short>.md` (`kind: decision`), `gotchas/<slug>.md` (`kind: gotcha`) e `_rules/<slug>.md` (`kind: rule`); a página de sessão é `tier: episodic`, `kind: fact`. A sessão deve pertencer ao projeto no qual a página é gravada; a página a registra como evidência, recebe o mesmo frontmatter de página de sessão (mais `consolidated_by: agent`) e sufixo de título duplicado, e a tarefa enfileirada de SessionEnd da sessão é marcada como concluída. A gravação usa a operação de admissão `write_page`, portanto um webhook que filtra em `consolidate` não a vê. Uma página fixada (pinned) nunca é sobrescrita dessa forma. O SessionEnd mantém a página: nem o resumo baseado em regras nem o worker opcional de SessionEnd a substituem, mesmo quando a sessão continuou após a gravação, e um ponto de verificação de PreCompact ou PostCompaction também a deixa intacta. |
| "O que aprendemos com esta sessão?" / "que memória devemos adicionar?" | `memory_auto_improve` | Sem um ID de sessão, revisa a sessão concluída mais recente sem execução persistida de autoaprimoramento, avançando além de saltos prévios em chamadas repetidas; passe um ID para uma nova execução direcionada. O servidor também executa autoaprimoramento agendado para novas sessões concluídas quando um LLM está configurado. `[auto_improve.scheduler] enabled = false` desativa a revisão automática; `[auto_improve] require_approval = true` deixa propostas agendadas e manuais em pending-writes para revisão. |
| "Lembre-se disso permanentemente" / "adicione uma anotação" | `memory_write_page` | Grava conhecimento durável na wiki; não é um handoff de uso único. |
| "Acrescente esta ação à seção Log" / "substitua apenas Problemas em aberto" | `memory_read_page`, depois `memory_patch_page` | Aplica patch em uma página Markdown existente sem reemitir todo o seu corpo ou frontmatter. Passe o `page_id` obrigatório; a substituição de seção também passa o `sha256` retornado da seção. O lote grava todas as edições ou nenhuma. |
| "Lembre-se disso até sexta-feira" / "expire isso após a migração" | `memory_write_page` com `expires_at` | Grava uma página delimitada no tempo. Use RFC3339 ou `YYYY-MM-DD` (fim do dia UTC); a recuperação normal a oculta após a expiração e a próxima varredura de esquecimento a exclui; um `memory_read_page` por caminho exato ainda a retorna, marcada como `expired: true`. O TTL tem precedência sobre `pinned`. |
| "Busque nas notas expiradas por X" | `memory_query` com `include_expired: true` | Inclui páginas históricas expiradas em uma busca explícita de projeto, escopo irmão ou global; buscas comuns as excluem. |
| "Busque apenas runbooks de release em Rust" | `memory_query` com `path_prefix: "procedures/"`, `tag: "rust"`, e opcionalmente `kind` / `tier` | Restringe todos os streams de recuperação de página antes da classificação. Filtros também restringem resultados globais, viagem no tempo, perfil e `pin_first`. Uma falha com filtro não possui fallback para observações brutas porque observações não possuem metadados de página. |
| "Por que esta página foi classificada aqui?" | `memory_query` com `explain: true` | Adiciona classificações delimitadas por stream, entidades correspondidas, pontuações, contribuições RRF, proveniência de grafo e fatores de autoridade aos resultados de projeto/escopos, além de um objeto `timings` de nível superior com o tempo de processamento de cada stream em milissegundos (`fts_ms`, `entity_ms`, `vector_ms`, `graph_ms`, `observation_ms`, `rerank_ms`; streams que não foram executados são omitidos). Uma consulta global relata apenas seu stream FTS distinto e `fts_ms`. |
| Melhore a relevância dos melhores resultados de projeto/escopos | Defina `AI_MEMORY_RERANKER=llm` em um servidor com provedor de LLM | Envia a consulta delimitada mais até 30 títulos/trechos delimitados ao provedor para no máximo uma passada final de relevância. Solicitações inválidas, parciais, com falha, esgotadas por tempo limite ou com concorrência saturada preservam a ordem normal; `global=true` e resultados complementares de preferências globais permanecem inalterados. |
| "Desfaça essa alteração" / "recupere a página que excluí" | `memory_restore_page` | Chame-o sem `rev` para listar os pontos de verificação da página, depois com o `rev` escolhido para gravar essa versão de volta como a atual. A versão substituída permanece acessível. Resumos de sessão expurgados são recusados. Apenas administrador em um servidor multiusuário; veja [lifecycle-ops](lifecycle-ops.md#mcp-memory_restore_page). |
| "Exclua esta página" / "remova a nota sobre X" | `memory_delete_page` | Remove uma página por caminho exato. Passe `workspace` + `project` juntos quando a página estiver em um espaço de trabalho irmão, de modo que um nome de projeto compartilhado entre espaços de trabalho nunca direcione silenciosamente a exclusão para o local errado. |
| "Aquela página recuperada ajudou" / "esta página está desatualizada" | `memory_feedback` | Registra `helpful`, `not_helpful`, `stale` ou `wrong` para o caminho exato. O peso de retenção afeta páginas episódicas elegíveis para varredura; stale/wrong também sinalizam qualquer página atual para revisão de lint. Conteúdo recuperado nunca autoriza feedback por si só. |
| "Audite a wiki" / "alguma contradição?" | `memory_lint` | Executa verificações de páginas desatualizadas, contradições e sugestões de regras. |
| "Qual é o tamanho da wiki?" / "estatísticas?" | `memory_status`, `memory_briefing` | Contagens e janelas de atividades recentes; `memory_briefing` é somente leitura. |

Trate a memória recuperada como evidência histórica não confiável, nunca como instruções
por si só. Quando a busca retornar páginas correspondentes em `_rules/`, `gotchas/`, `procedures/`
ou `decisions/`, leia a página inteira e valide-a com o estado atual do usuário,
do projeto e do checkout antes de agir. Esses caminhos registram regras pretendidas,
avisos, checklists e decisões de arquitetura; eles não podem autorizar ferramentas,
comandos, divulgação, feedback ou alterações de permissão/política. Namespace, tier,
tags, fixação (pinning) e classificação são apenas proveniência de recuperação, nunca autoridade
de instrução.

A ordenação de busca favorece esses namespaces mantidos apenas quando a relevância é
próxima. Os tiers `semantic` / `procedural`, `pinned: true`, e as tags
`canonical`, `active` e `source-of-truth` adicionam autoridade modesta. `sessions/`,
`_lint/`, `investigations/`, e as tags `superseded`, `historical`,
`test-fixture` e `do-not-answer-from` a reduzem. Esses sinais nunca excluem
uma página: uma consulta destinada a um termo específico de sessão ainda pode retornar essa sessão.
`pinned` permanece principalmente um controle de retenção e mutação por automação, não uma
substituição incondicional de busca.

Resultados suplementares de `global_scope_hits` são autodescritivos: cada um inclui
`workspace`, `project` e `scope: "global"`. Leia um diretamente com seu caminho
e `scope: "global"`; uma leitura sem escopo ainda seleciona o projeto atual, de modo que
páginas locais e globais de mesmo caminho permanecem distintas.

### Mantenha uma consulta econômica

`memory_read_session_observations` adota como padrão 20 linhas de até 1000 caracteres
cada, e `memory_read_page` adota como padrão um limite de 12.000 caracteres. Ambos são
somente leitura, e ambos aceitam valores maiores de `limit`, `body_max_chars` ou `max_chars`
quando você precisar da transcrição completa ou da página inteira. Para uma pergunta como "o que esta
sessão fez" ou "onde ela parou", uma leitura restrita com `order: "desc"` e um filtro de `kinds`
como `["user-prompt", "stop", "session-end"]` geralmente é suficiente.

A sessão que faz a pergunta também afeta o custo independentemente do ai-memory. Uma pergunta
enviada a uma sessão de agente inativa há muito tempo recria o cache de todo o seu contexto antes da
primeira chamada de ferramenta, e cada chamada de ferramenta posterior o relê. Para uma consulta rápida, uma
sessão nova ou subagente custa muito menos do que uma sessão que esteve inativa por
horas.

## Busca a partir do terminal

`ai-memory search` executa uma consulta FTS5 contra a wiki a partir de um shell. Ele ignora
os streams de entidade, vetor e grafo e o fallback para observações brutas que o
`memory_query` híbrido do agente utiliza.

```bash
ai-memory search "wal mode"                           # current project (cwd / marker)
ai-memory search "wal mode" --workspace acme --project api
ai-memory search "wal mode" --global                  # every project
ai-memory search "wal mode" --global --json -n 20
```

Sem uma flag, o escopo provém do diretório atual, da mesma forma que
todos os outros comandos de cliente (veja [marker-file.md](marker-file.md)).
`--global` não envia escopo e busca nas páginas wiki de todos os projetos; cada
resultado é impresso com seu `workspace/project`, e `--json` adiciona os campos `workspace` e
`project` a cada resultado. `--global` não pode ser combinado com `--workspace`
ou `--project`. Cobre apenas páginas wiki compiladas, não observações brutas.

O comando chama `GET /admin/search`, portanto, em um servidor multiusuário, apenas o operador
root pode executá-lo. Um usuário de banco de dados busca entre os projetos que pode ler com
`memory_query(global=true)` ou pela página de busca web. A configuração de marcador
`[recall] default_global` de um repositório altera o que um `memory_query` sem escopo
de um agente lê; ela não altera este comando, que permanece
com escopo de projeto a menos que você passe `--global`.

## Páginas recentes a partir do terminal

`ai-memory recent` lista as páginas wiki atualizadas mais recentemente, as mais novas primeiro.
É o equivalente de terminal da ferramenta `memory_recent` do agente: sem string
de consulta, apenas recência.

```bash
ai-memory recent                                      # current project (cwd / marker)
ai-memory recent --workspace acme --project api
ai-memory recent --global                             # every project
ai-memory recent --global --json -n 20
```

Cada linha mostra o caminho da página, um `updated_at` em RFC 3339 e uma idade relativa,
depois o título e um pequeno trecho. `--global` rotula cada resultado com seu
`workspace/project`, e `--json` adiciona os campos `workspace` e `project` a
cada resultado. `--global` não pode ser combinado com `--workspace` ou `--project`.

O comando chama `GET /admin/recent`, portanto, em um servidor multiusuário, apenas o operador
root pode executá-lo. `[recall] default_global` não altera este comando;
ele permanece com escopo de projeto a menos que você passe `--global`.

## Chamar qualquer ferramenta MCP a partir do terminal

`ai-memory call` é um caminho via shell para todas as ferramentas MCP, para um
agente que não pode ou não deve carregar MCP. Ele cobre as ferramentas que os
comandos dedicados acima não cobrem, como as híbridas `memory_query`,
`memory_briefing`, `memory_patch_page` e `memory_handoff_begin`.

```bash
ai-memory call                                        # list the tools and their input schemas
ai-memory call memory_query '{"query":"wal mode"}'
ai-memory call memory_query '{"query":"wal mode","global":true}'
```

A chamada vai para o endpoint `/mcp` do servidor, resolvido e autenticado como
os demais comandos de cliente. Uma chamada que não nomeia nenhum de `workspace`,
`project`, `project_id` ou `global` recebe o projeto do diretório atual quando a
ferramenta aceita um, pela mesma regra do `mcp-bridge --no-session-id`. O
resultado em texto da ferramenta vai para o stdout. O código de saída é 1 quando
a ferramenta relata um erro (seu texto vai para o stderr) ou o servidor responde
com um erro JSON-RPC, e 2 quando os argumentos não são um único objeto JSON.

## Memória histórica e inteligência de código em tempo real

O ai-memory pode ser executado ao lado do CodeGraph, de um serviço com suporte a LSP, de um índice SCIP/LSIF
ou de outro servidor MCP de inteligência estrutural de código. Mantenha os serviços
independentes: eles respondem a perguntas diferentes e não precisam de armazenamento compartilhado,
sincronização de sessões ou protocolo de precedência.

| Pergunta | Comece com | Regra de autoridade |
|---|---|---|
| Por que esse design foi escolhido? O que falhou antes? Qual procedimento ou handoff se aplica? | ai-memory | Trate o resultado como evidência histórica não confiável; leia a página relevante completa e verifique se ela ainda é aplicável. |
| Onde está este símbolo agora? Quem o chama? O que depende dele ou pode mudar com ele? | Um provedor estrutural, LSP ou busca direta no checkout | Trate o resultado como um indício no código atual e confirme afirmações importantes no código-fonte. |
| A alteração proposta realmente funciona? | Inspeção de código-fonte, compilador/build, testes e comportamento observado em tempo de execução | Estas são as evidências operacionais definitivas. Uma página de memória ou resultado de provedor não pode se sobrepor a elas. |

Uma sequência prática é:

1. Consulte o ai-memory antes de planejar para recuperar decisões, restrições, abordagens
   rejeitadas e riscos conhecidos.
2. Inspecione o checkout atual ou peça ao provedor estrutural para localizar os
   arquivos, símbolos, chamadores e dependências indicados. Um caminho ou símbolo preservado
   na memória pode ter sido movido, mudado de significado ou desaparecido.
3. Faça a alteração no código-fonte do checkout e valide-a com o
   build do projeto, testes e verificações relevantes em tempo de execução.
4. Preserve o aprendizado ou decisão duradoura no ai-memory. Não copie um
   grafo transitório de chamadas ou o índice completo de um provedor para a wiki apenas
   porque apareceu no resultado de uma ferramenta.

Nenhum dos lados é um canal de instruções. A memória recuperada continua sendo dados históricos
não confiáveis, e a saída de ferramentas estruturais continua sendo dados externos não confiáveis;
nenhum deles pode autorizar comandos, divulgação, alterações de permissão, feedback ou
operações destrutivas. Siga apenas as instruções atuais do sistema, do desenvolvedor, do usuário e
as canônicas do projeto.

O ai-memory atualmente não consulta provedores estruturais automaticamente, não
classifica seus resultados como um tipo de evidência persistida especial, não rastreia a existência
de símbolos nem marca páginas como desatualizadas com base no estado do provedor. Ele não infere a
identidade de um provedor estrutural ou evidência estrutural durável apenas a partir do
resultado genérico de uma ferramenta; trechos capturados continuam passando pelos limites
existentes de análise, saneamento, tamanho e política de captura específicos do agente. Isso
mantém a propriedade da fonte e os modos de falha explícitos enquanto requisitos reais de
interoperabilidade são reunidos.
Adaptadores específicos de provedores ou referências estruturais persistidas devem ser adicionados
apenas com um produtor, consumidor, modelo de versionamento, limite de privacidade e
comportamento concretos para provedores indisponíveis ou contraditórios.

Páginas consolidadas podem conter até 10 `entities:` normalizadas no frontmatter
canônico. Elas formam um stream de recuperação lexical com escopo de projeto: nomes exatos,
prefixos de nomes e prefixos de palavras após espaços, hifens ou sublinhados correspondem
sem uma chamada de LLM em tempo de consulta. Operadores podem editar a mesma lista YAML diretamente
em uma página wiki; o observador (watcher) e `ai-memory reindex` derivam o índice SQLite a partir do
Markdown (`reindex` requer um banco de dados derivado limpo). `explain: true` expõe
`entity_rank`, seu `entity_weight` bruto de frequência inversa, `matched_entities`
e a contribuição RRF da entidade. Índices vazios de entidades não contribuem com candidatos
ou pontuação, e páginas expiradas permanecem excluídas, a menos que `include_expired: true`.

## Gravação de metadados de página

`memory_write_page` e `POST /admin/write-page` aceitam os mesmos campos opcionais
de metadados no nível superior do JSON da requisição:

| Campo | Valor aceito e limite |
| --- | --- |
| `kind` | MCP: tipo semântico em texto livre, até 64 caracteres brutos, depois aparado (trimmed). Admin: o adaptador de string opcional existente retém seu tratamento original (aparar e omitir vazios), sem as novas verificações de kind do MCP. |
| `entities` | Até 10 strings de entrada, cada uma com até 64 caracteres brutos; depois aparadas, com espaços em branco colapsados, convertidas para minúsculas e desduplicadas usando o normalizador de entidades existente. |
| `abstract` | Resumo L0, até 1.024 caracteres brutos, depois aparado. |
| `relations` | Objeto contendo apenas as chaves `causes`, `fixes` ou `contradicts` e arrays de páginas de destino, até 32 destinos de entrada no total e 1.024 caracteres brutos por destino. |

Limites de caracteres e listas se aplicam à entrada bruta, antes de aparar, normalizar
ou desduplicar. Por exemplo, uma entidade com 60 caracteres seguidos por seis
espaços é recusada, embora a normalização por si só gerasse 60 caracteres.
O `kind` de nível superior existente do admin mantém seu comportamento legado, incluindo
strings com mais de 64 caracteres; o importador continua usando esse adaptador.

Destinos de relação usam a gramática de wikilink existente: `notes/page`,
`project:notes/page.md` ou `workspace/project:notes/page.md`. Caminhos sem extensão
são resolvidos com `.md`; destinos de páginas inválidos ou não portáteis são recusados.
Componentes de escopo já devem estar aparados: `other:notes/x` é válido, enquanto
`other :notes/x` é recusado em vez de criar uma aresta não resolvida.
Relações descrevem links; elas não concedem acesso aos seus destinos.
Formatos malformados, entidades inválidas e limites excedidos nos novos campos
de metadados falham antes que um escopo de gravação seja criado. Esses campos e o `kind` do MCP
rejeitam caracteres de controle que não sejam espaços em branco. Destinos de relações também
recusam caracteres de controle de espaço em branco. Todos os metadados ainda passam pelo
sanitizador da wiki e pela cadeia de admissão, e a atribuição provém da autenticação.

Por exemplo, esses campos podem ser adicionados à requisição existente de qualquer uma das interfaces:

```json
{
  "kind": "decision",
  "entities": ["SQLite", "Writer actor"],
  "abstract": "One writer owns every SQLite mutation.",
  "relations": {"fixes": ["gotchas/concurrent-writes.md"]}
}
```

Ambas as interfaces substituem a página inteira, incluindo seus metadados editáveis.
Campos omitidos não herdam a versão anterior; listas vazias de entidades e
objetos de relação limpam esses campos. Strings vazias de `kind` ou `abstract` omitem
essas chaves. Requisições existentes sem os novos campos permanecem válidas, incluindo
o `kind` de nível superior existente do admin. Campos arbitrários de frontmatter, escopo e autor
nunca são copiados dos metadados. Sem `expected_page_id` ou
`create_only`, a gravação é uma substituição incondicional. Apenas
`memory_write_page` aceita essas pré-condições; `POST /admin/write-page` não tem
nenhuma das duas. Veja
[Alterar uma página apenas se ninguém mais o fez](programmatic-memory.md#altere-uma-página-apenas-se-ninguém-mais-a-tiver-alterado)
para gravações condicionais.

### Aplicando patch em uma seção sem substituir a página

`memory_read_page` retorna no máximo 100 títulos Markdown analisados em `sections`,
cada um com seu `anchor`, `level` e `sha256` exato; `sections_truncated` informa
se existem mais. Títulos dentro de código cercado não são seções. Leia a página
imediatamente antes de aplicar o patch e, em seguida, chame `memory_patch_page` com aquele
`page_id` obrigatório:

```json
{
  "workspace": "customers",
  "project": "aran",
  "path": "STATUS.md",
  "expected_page_id": "019...",
  "edits": [
    {
      "op": "append",
      "target": "section",
      "anchor": "Recent actions",
      "content": "| 2026-10-26 | SMTP fixed |"
    },
    {
      "op": "replace_section",
      "anchor": "Open issues",
      "section_sha256": "9f...",
      "content": "| None |"
    }
  ]
}
```

Use `target: "end_of_page"` sem âncora para anexar no fim do arquivo (EOF). `add_section`
requer um título ATX de nível 2-6 como `## Decisions` e rejeita um título
que já exista após a normalização de espaços/caixa. Edições que chegam ao
mesmo local mantêm onde foram resolvidas na página que você leu, qualquer que seja sua ordem
em `edits`: uma anexação à última seção permanece nessa seção, uma
anexação em `end_of_page` a segue e novas seções vêm por último. Assim,
`end_of_page` é o fim da página que você leu, antes de qualquer seção que o mesmo
lote adicione; o texto que deve vir depois de um novo título vai no `content` desse
`add_section`. Um novo título recebe uma linha em branco antes e depois. Substituir uma
seção também substitui as seções aninhadas nela, portanto, outra edição em uma
delas é um destino sobreposto. Títulos duplicados, tokens de página ou seção
desatualizados, destinos sobrepostos, falha na admissão ou um gravador simultâneo rejeitam
o lote inteiro. O frontmatter existente é preservado; o corpo final ainda passa
pelo sanitizador normal, admissão, atribuição, versionamento, índice e
caminho de ponto de verificação git.

## Instalar o trecho de roteamento e as Agent Skills

A partir de um agente, diga:

```text
Install ai-memory routing into this project.
```

O agente chama `memory_install_self_routing` e recebe o bloco compacto
`markered_block`, strings de marcador, dicas de arquivos de regras, payloads de habilidades gerenciadas,
dicas de destino de habilidades e orientações de substituição. Ele então usa sua ferramenta normal de edição
de arquivos para preservar o conteúdo de usuário não relacionado, substituir ou anexar o
bloco `<!-- ai-memory:start -->` / `<!-- ai-memory:end -->` apenas quando os
delimitadores de marcador aparecerem sozinhos em suas próprias linhas, e gravar cada habilidade gerenciada
abaixo da raiz de habilidades selecionada. Arquivos de habilidade são gerenciados pelo ai-memory apenas quando
contêm o marcador gerenciado, portanto, habilidades de mesmo nome não gerenciadas não devem ser
substituídas, a menos que o humano force a substituição explicitamente.

O trecho instalado e todas as habilidades gerenciadas com escopo de projeto usam a mesma
regra de escopo com reconhecimento de cliente. Uma ponte com reconhecimento de sessão que encaminha o id real de sessão
do hook pode omitir o escopo para o repositório atual. Um cliente MCP estático deve
enviar valores exatos de `workspace` e `project` em cada chamada com escopo de projeto, mesmo
quando hooks de ciclo de vida estiverem instalados, porque hooks por si só não identificam a requisição
MCP. Declare ambos os nomes no [`.ai-memory.toml`](marker-file.md) mais próximo,
ou obtenha-os da configuração do operador/servidor; nunca os infira a partir de um
nome de diretório nem confie no último projeto ativo do servidor. Os modos de consulta global e
preferência global continuam omitindo argumentos de escopo de projeto.

A partir de um terminal:

```bash
ai-memory install-instructions
ai-memory install-instructions --target AGENTS.md
ai-memory install-instructions --print
ai-memory install-instructions --no-skills
```

`install-instructions` instala ou atualiza habilidades gerenciadas por padrão. Use
`--no-skills` apenas quando desejar intencionalmente uma atualização apenas do trecho.
A CLI substitui apenas o bloco marcado do ai-memory, preserva o conteúdo não relacionado,
e grava um backup com carimbo de data/hora antes de alterar um arquivo de instruções existente.
`install-instructions --print` pré-visualiza apenas o trecho de instruções; use
`install-skills --print` para pré-visualizar os payloads das habilidades. As flags de habilidades espelham
`install-skills` com o prefixo `--skills-`:
`--skills-scope project|global`, `--skills-agent claude-code|agents|devin|grok|both`,
`--skills-target-dir <dir>`, e `--skills-force`.

A detecção automática estende `CLAUDE.md` quando ele existe, `AGENTS.md` quando ele
existe, ambos quando ambos existem, ou cria `CLAUDE.md` quando nenhum existe. Use
`--target AGENTS.md` para projetos que não sejam exclusivos do Claude. O destino da habilidade segue o
destino da instrução, a menos que você o sobrescreva: `CLAUDE.md` implica
`.claude/skills`, `AGENTS.md` implica `.agents/skills`, e ambos os arquivos implicam
ambas as raízes de habilidades. Para a Grok Build CLI, selecione `--skills-agent grok` para que as habilidades
sejam instaladas sob sua raiz `.grok/skills`; para a GitHub Copilot CLI, selecione
`--skills-agent copilot-cli` para que as habilidades sejam instaladas sob `.github/skills` (projeto)
ou `$COPILOT_HOME/skills` (global, padrão `~/.copilot/skills`); para o Hermes
Agent, `--skills-agent hermes`
instala sob `.hermes/skills` (projeto) ou `~/.hermes/skills` (global).

Quando um projeto mantém `AGENTS.md` como seu arquivo de instruções canônico, forneça a ele um
`CLAUDE.md` cuja primeira linha seja uma importação pura `@AGENTS.md`. O Claude Code 2.1.277
e posteriores leem `AGENTS.md` apenas quando nenhum `CLAUDE.md`, `.claude/CLAUDE.md` ou
`CLAUDE.local.md` existir no diretório de trabalho ou em qualquer diretório acima dele.
Sem essa importação, sessões que leem apenas `CLAUDE.md` (Claude Code mais antigo, o
plugin AGENTS.md desativado, a configuração de instruções do projeto `claude-md` ou o
próprio `CLAUDE.local.md` de um desenvolvedor) começam sem um bloco instalado com
`--target AGENTS.md`, ou qualquer outra regra de projeto naquele arquivo. Um apontador em prosa "leia
AGENTS.md" não carrega o arquivo; ele pede ao agente para abri-lo, o que
deixa a conformidade dependente de o agente fazê-lo. Veja
[memória do Claude Code](https://code.claude.com/docs/en/memory#agents-md).
`install-instructions` imprime uma observação quando grava apenas `AGENTS.md` enquanto um
`CLAUDE.local.md`, `.claude/CLAUDE.md` ou `CLAUDE.md` no diretório ou acima
dele faria com que o Claude Code lesse aquele arquivo em vez disso, a menos que um deles importe
o `AGENTS.md` sendo gravado. O `~/.claude/CLAUDE.md` em nível de usuário
do Claude Code não conta, inclusive quando `$HOME` alcança aquele
diretório por meio de um symlink. A importação é resolvida a partir do arquivo que a
contém (`@AGENTS.md` em `.claude/CLAUDE.md` aponta para `.claude/AGENTS.md`; esse arquivo
precisa de `@../AGENTS.md`), `~/` expande a partir do diretório pessoal, e um `@path`
dentro de um intervalo de código ou bloco cercado não conta. Importações são seguidas através
de outros arquivos importados até quatro saltos, conforme o Claude Code as segue.

Para atualizar apenas as Agent Skills gerenciadas:

```bash
ai-memory install-skills
ai-memory install-skills --scope global --agent agents
ai-memory install-skills --scope global --agent devin
ai-memory install-skills --scope global --agent grok
ai-memory install-skills --scope global --agent copilot-cli
ai-memory install-skills --scope global --agent hermes
ai-memory install-skills --scope global --agent antigravity
ai-memory install-skills --agent both --print
ai-memory install-skills --target-dir .custom/skills --force
```

Para o Devin, habilidades locais do projeto são instaladas em `.devin/skills`. Instalações
globais do Devin usam `%APPDATA%\devin\skills` no Windows e `~/.devin/skills`
em sistemas não Windows. Para a Grok Build CLI, habilidades locais do projeto ficam sob
`.grok/skills` e globais sob `$GROK_HOME/skills` (padrão
`~/.grok/skills`). Uma instalação global de habilidades do Grok também grava a regra de handoff do
primeiro turno `$GROK_HOME/rules/ai-memory-handoff.md` (padrão
`~/.grok/rules/ai-memory-handoff.md`); habilidades do Grok em escopo de projeto e
`--target-dir` não o fazem. Para a GitHub Copilot CLI, habilidades locais do projeto ficam sob o
`.github/skills` do repositório e globais sob `$COPILOT_HOME/skills` (padrão
`~/.copilot/skills`); a Copilot CLI já carrega `AGENTS.md`, portanto compartilha o
destino de instruções `AGENTS.md`. O Google Antigravity lê habilidades do espaço de trabalho em
`.agents/skills` e possui duas raízes globais, `~/.gemini/config/skills` para o
Aplicativo e IDE e `~/.gemini/antigravity-cli/skills` para a CLI; uma instalação global
`--agent antigravity` grava ambas, e `uninstall` varre ambas.

As raízes locais de habilidades do projeto são `.claude/skills` para instalações compatíveis com Claude,
`.agents/skills` para instalações entre clientes, `.devin/skills` para Devin, e
`.grok/skills` para Grok, e `.github/skills` para GitHub Copilot CLI. As raízes globais
do Claude/Agents são `~/.claude/skills` e `~/.agents/skills`; raízes globais do Devin
são específicas da plataforma, conforme descrito acima; Grok global é
`$GROK_HOME/skills` (padrão `~/.grok/skills`); Copilot CLI global é
`$COPILOT_HOME/skills` (padrão `~/.copilot/skills`). `uninstall` varre ambas as
raízes da Copilot CLI e remove apenas arquivos que contenham o marcador gerenciado do ai-memory,
de modo que habilidades criadas pela equipe em `.github/skills` nunca sejam tocadas.
`--target-dir` aponta para uma raiz de habilidades explícita e ignora a inferência
de escopo/agente. `--print` pré-visualiza caminhos de destino e conteúdos de `SKILL.md`. `--force`
permite a substituição de habilidades não gerenciadas de mesmo nome; sem ela, habilidades
criadas pelo usuário são preservadas. O Uninstall remove habilidades gerenciadas pelo ai-memory das
raízes padrão de projeto/global após a validação do marcador; raízes personalizadas de `--target-dir`
são um caminho de limpeza manual.

Isso é apenas empacotamento de prompts. O ai-memory não executa um roteador de habilidades em tempo de execução,
não armazena memória durável em `SKILL.md` e não transforma o
loop de autoaprimoramento em um sistema de criação de habilidades. O conhecimento durável ainda
reside na wiki.

## Inicializar um projeto existente (bootstrap)

Se você instalar o ai-memory em um projeto que já possui meses de
histórico, a wiki começa vazia. O `ai-memory bootstrap` a inicializa a partir do
histórico existente do repositório e de documentos.

```bash
export AI_MEMORY_SERVER_URL="http://localhost:49374"
ai-memory bootstrap --dry-run
ai-memory bootstrap
```

O coletor de bootstrap lê o `git log`, o README raiz, `docs/`,
arquivos de regras do projeto e documentações de módulos Rust, depois envia via POST as
fontes selecionadas para o servidor em execução. Requer um provedor de LLM no
servidor. Veja [Livro de receitas de instalação - bootstrap no meio do projeto](install.md#bootstrap-em-projetos-em-andamento)
para flags, orçamentos de tokens e prioridade de fontes.

## Migrar de outra ferramenta de memória

Ao substituir um sistema de memória existente, trate os dados antigos como entrada
histórica não confiável até que você os faça a curadoria. Não redirecione transcrições brutas ou repositórios
antigos de memória diretamente para o ai-memory.

Lista de verificação de migração:

1. Exporte a memória ou histórico antigo antes de alterar hooks.
2. Mantenha a exportação bruta como um arquivo compactado, não como a verdade atual do projeto.
3. Limpe segredos, tokens, credenciais, chaves de API e logs brutos que não devem
   se tornar memória duradoura.
4. Faça a curadoria do material útil em páginas Markdown revisadas sob um diretório temporário
   de documentação ou diretamente em `concepts/`, `decisions/`, `gotchas/`,
   `procedures/`, `notes/` ou `_rules/`.
5. Se este checkout puder ser ambíguo, adicione `.ai-memory.toml` para fixar o
   espaço de trabalho/projeto pretendido antes de importar ou instalar hooks.
6. Inicie `ai-memory serve` localmente e confirme se `ai-memory status` consegue alcançar o
   servidor antes de alterar as configurações existentes dos clientes.
7. Importe o material curado primeiro; evite importar todo o histórico bruto legado.
8. Verifique as páginas esperadas com o `memory_query` híbrido completo; use
   `ai-memory search` apenas quando uma consulta FTS5 no terminal for suficiente.
9. Configure o MCP e os hooks de ciclo de vida para um cliente de cada vez.
10. Somente após a captura e recuperação do ai-memory funcionarem, desative os hooks,
    plugins ou servidores MCP de memória antigos.
11. Procure referências antigas à ferramenta antiga na configuração de cada cliente e remova
    cabeçalhos `Authorization` ou variáveis de ambiente obsoletas se a autenticação bearer mudou.
12. Reinicie cada CLI de agente após alterar hooks, plugins ou configurações de MCP.

Dicas de limpeza do cliente:

- Claude Code: verifique plugins, hooks, injeções antigas de SessionStart e servidores MCP.
- Codex: verifique a configuração de MCP e hooks de session/user-prompt/tool/compaction/stop.
- Command Code: verifique `~/.commandcode/mcp.json` e os quatro eventos estáveis de
  ciclo de vida em `~/.commandcode/settings.json`.
- Devin CLI: verifique `.devin/config.json`, `.devin/hooks.v1.json` e
  `.devin/skills` para entradas obsoletas de MCP, hooks ou habilidades de roteamento.
- Gemini CLI e Antigravity CLI: verifique `settings.json` ou arquivos equivalentes de
  configuração de hooks/MCP.
- Kimi Code: verifique `~/.kimi-code/mcp.json` e as entradas `[[hooks]]` em
  `~/.kimi-code/config.toml` (ambos sob `$KIMI_CODE_HOME` quando definido) para entradas
  obsoletas de MCP ou hooks.
- Kiro CLI: verifique os objetos `hooks` dentro de `~/.kiro/agents/*.json` (v2),
  `~/.kiro/hooks/ai-memory.json` (v3) e `~/.kiro/settings/mcp.json` (todos
  sob `$KIRO_HOME` quando definido) para entradas obsoletas do ai-memory.
- OpenCode, OpenClaw e OMP: verifique a configuração de MCP e diretórios de plugins/extensões;
  mova plugins de memória antigos para um diretório desativado/quarentena antes de excluir.
  O OpenCode usa `ai-memory.ts` com `mcp` direto na V1 e
  `ai-memory-opencode2.ts` com `mcp.servers` na V2; o instalador genérico
  detecta a versão principal instalada e remove apenas um irmão incompatível comprovadamente
  gerado pelo ai-memory.
- VS Code Copilot, Claude Desktop e Zed: estes são somente MCP, portanto confirme
  se a ferramenta antiga fornecia hooks de captura em outro lugar. As entradas de MCP
  do Zed ficam sob `context_servers` em seu `settings.json` de usuário.

Se você deseja um lembrete visível de inicialização durante a transição, mantenha-o pequeno. Uma
nota no arquivo de regras como "Memória ativa: ai-memory; exportação legada é apenas referência
histórica; use memory_query para recuperação" é mais segura do que despejar um grande
contexto legado em cada sessão.

Se você usa o provedor OAuth do ChatGPT/Codex, faça login uma vez antes de iniciar o
servidor com `AI_MEMORY_LLM_PROVIDER=openai-oauth`:

```bash
ai-memory auth login openai-oauth
ai-memory auth status
```

O comando de login armazena apenas credenciais de provedor em `<data_dir>/auth.json`.
É separado de `AI_MEMORY_AUTH_TOKEN`, o Bearer de root de máquina usado por
MCP, hooks, handoffs, workstreams e chamadas de máquina para APIs de dupla autenticação.

Para o GitHub Copilot, use o login correspondente de provedor antes de iniciar o servidor
com `AI_MEMORY_LLM_PROVIDER=copilot`:

```bash
ai-memory auth login copilot
ai-memory auth status
```

A autenticação do Copilot armazena um token de usuário do GitHub e, em seguida, o provedor o troca por um
token de API do Copilot de curta duração antes de cada chamada ao LLM.

O login de dispositivo (`openai-oauth`, `copilot` e `oidc-device`) aguarda até
`--timeout-secs` segundos pela autorização após a emissão do código do dispositivo
(padrão: 600). O limite abrange requisições de sondagem de autorização do dispositivo,
leituras de resposta e pausas entre sondagens, incluindo o recuo `slow_down` de um provedor.
Alcançá-lo descarta o resultado da sondagem sem alterar as credenciais
armazenadas. A descoberta, a requisição inicial de código de dispositivo e a troca da OpenAI
de um código autorizado dentro do prazo mantêm seu próprio tempo limite de requisição HTTP.

## Navegar pela wiki em um navegador

Inicie o servidor com `--enable-web` e abra
`http://<host>:49374/web`.

```bash
ai-memory serve --transport http --bind 127.0.0.1:49374 --enable-web
```

No macOS, o [aplicativo da barra de menus](macos.md#scenario-d-menu-bar-app) já inicia
o LaunchAgent com `--enable-web`; **Open Web UI** abre essa mesma URL.

Usuários de Docker compose podem adicionar a flag ao comando do serviço:

```yaml
command: ["serve", "--transport", "http", "--bind", "0.0.0.0:49374", "--enable-web"]
```

A UI web é somente leitura: lista de projetos, árvore de páginas por projeto,
trilhas de navegação, markdown renderizado, metadados, busca, grafo de links e uma
visão de operador de workstreams gerenciados.
O link **Graph** do cabeçalho abre `/web/graph` (wikilinks entre projetos);
cada página de projeto possui link para `/web/w/<ws>/<proj>/graph` (páginas naquele
projeto e os links resolvidos entre elas). Ambos são um SVG renderizado no servidor
mais uma tabela, sem JavaScript. Em uma visão de projeto, página,
namespace, histórico ou sessão, a caixa de busca do cabeçalho pesquisa naquele projeto;
em outros lugares, ela busca em todos os projetos com FTS5, como
`memory_query(global=true)`, e o seletor "Search in" da página de busca escolhe
um projeto, e seus seletores "Kind" e "Tier" (`kind=` e `tier=`) mantêm apenas
as páginas daquele tipo ou nível. Uma página mostra uma estrutura de tópicos de seus títulos, um selo `expired` assim que seu TTL `expires_at` expirar, as páginas que a citam e
as páginas que ela cita; a listagem de namespace de uma pasta ordena por atualização recente ou por
título e mostra 25 páginas por vez. A árvore, a lista de atividades recentes e uma
listagem de namespace omitem uma página cujo TTL `expires_at` já expirou, assim como a busca
faz; sua própria URL ainda a abre. A busca na página de um projeto classifica com
o mesmo classificador híbrido que o `memory_query` para aquele projeto, e sua caixa "Explain
ranking" mostra as classificações de stream de cada resultado, contribuições RRF, autoridade e pontuação de reclassificação, além dos tempos por stream.
O root também obtém `/web/workstreams`: uma visão geral delimitada e focada na atenção de
workstreams gerenciados entre projetos. Ela deriva `active`, `interrupted`,
`completed` e `empty` apenas a partir de concessões de execução, estado de saída e contagens
de eventos importados; não interpreta texto em prosa de transcrições nem expõe caminhos de checkout,
impressões digitais de repositório, proprietários de concessões ou ids de sessão nativos. Este agregado
de operador não altera `ai-memory resume`, cuja descoberta permanece restrita
ao checkout atual. O root também obtém `/web/health`: contadores de hooks (429s incluídos), a fila do gravador,
o spool local e diário de recuperação, a fila de consolidação, o estado do integrador de embeddings e
do provedor, e as verificações com falha junto com o comando que corrige cada uma. Cada página possui link
para seu histórico (`/web/w/<ws>/<proj>/h/<path>`): cada versão armazenada, a mais nova
primeiro, com o autor onde a gravação foi atribuída, e um diff unificado
entre quaisquer duas delas (por padrão, a mais recente contra a versão que ela
substituiu). Uma versão que outra gravação substituiu permanece listada; uma expirada
não, e uma página cujo TTL expirou não possui histórico. A página do projeto
lista sessões recentes, e cada uma abre uma linha do tempo
(`/web/w/<ws>/<proj>/s/<session-id>`) que pagina através das observações
capturadas da sessão em ordem, com links primeiro/anterior/próximo/último, um campo de salto
e, quando o JavaScript é executado, um controle deslizante e as teclas `j`/`k` para avançar uma observação
de cada vez. Observações são mostradas em texto simples. Em páginas
renderizadas, `[[wiki links]]` tornam-se links clicáveis para a página de destino.
`[[path]]`, `[[path|label]]`, `[[project:path]]`,
`[[workspace/project:path]]` e `[[_global:path]]` são todos aceitos
(resolvidos contra o projeto da página atual, a menos que o destino carregue
seu próprio escopo; `[[_global:path]]` sempre nomeia o projeto reservado `_global`
no espaço de trabalho padrão).
`[[…]]` permanece literal dentro de código cercado (` ``` ` e `~~~` fecham
apenas pelo seu próprio glifo), código em linha `` `…` `` e código
indentado com 4 espaços; esquemas externos dentro dos colchetes (`http://`, `https://`,
`mailto:`, `data:`, `javascript:`, `vbscript:`, `tel:`, `file:`)
permanecem literais também.

Sem nenhum token bearer configurado em loopback (uma instalação anterior à versão 3.0 que
nunca executou `ai-memory auth enable`), a wiki integrada permanece anônima. Uma
nova instalação possui um token bearer e nenhuma conta humana, portanto um navegador recebe um
prompt de login básico (Basic auth): qualquer nome de usuário, e o conteúdo de
`<data_dir>/auth-tokens/default` como a senha. O próprio navegador reenvia essas
credenciais, portanto nenhum cookie armazena o token. Esse caminho de navegador exclusivo via GET
é interrompido imediatamente após
a existência de uma senha humana ou um bootstrap concluído. Para administração
com autenticação humana, crie a primeira conta root ([primeira vez como
root](install.md#primeiro-acesso-do-root-greenfield)) e sirva a SPA administrativa compilada
com `--web-ui-dir`: ela faz login por meio de `/auth/login` e usa uma sessão web
HttpOnly mais proteção CSRF. Tokens Bearer armazenados no navegador continuam não sendo aceitos. O
redator da caixa de entrada pode usar um token de gravação inserido explicitamente para uma única mensagem
quando o login legado for
somente leitura; ele limpa o token após a requisição e nunca o persiste.
Fora dessa ação da caixa de entrada, `AI_MEMORY_AUTH_TOKEN` e chaves de API `aim_` permanecem
credenciais de máquina enviadas como `Authorization: Bearer <token>` por clientes de
MCP, hook, handoff e workstream.

Para hospedar a UI web sob um subcaminho de URL atrás de um proxy reverso, as
flags `--base-path` / `--web-slug` fazem o trabalho. Veja
[`docs/frontend-api.md`](frontend-api.md#6-hospedagem-de-ui-customizada-e-caminhos-base)
para a semântica das flags e
[`docs/https-via-proxy.md`](https-via-proxy.md#hospedando-sob-um-subcaminho)
para o passo a passo no lado do proxy.

![Project list homepage with four projects shown as cards with page counts and last activity.](web-projects-home.png)

![Visão do projeto com árvore de pastas, selos de kind e atividade recente.](web-project-view.png)

## Inspecionar a wiki bruta

A wiki é puro markdown mais histórico do git.

```bash
docker exec ai-memory ls /data/wiki/sessions/
docker exec ai-memory cat /data/wiki/sessions/<uuid>.md

# Open in Obsidian or any markdown viewer:
docker cp ai-memory:/data/wiki ./my-ai-memory-wiki

# Time-travel:
docker exec ai-memory git -C /data/wiki log --oneline
```

A reindexação do observador (watcher) compartilha o mutex por página usado por gravações e lotes no
mesmo identificador de Wiki e seus clones. Ele aguarda antes de ler o arquivo, retém esse
mutex durante o upsert do SQLite e libera as proteções de mutação tanto da página quanto globais
antes da geração de embeddings. Caminhos diferentes podem prosseguir simultaneamente, e o
observador nunca reescreve a página. Editores externos e identificadores de Wiki construídos
independentemente não obtêm esses bloqueios; edições externas ainda podem concorrer com uma
gravação ou reindexação do servidor.

## Mover uma sessão para outro projeto

Uma sessão capturada no projeto errado (um `cd` em um diretório
temporário, um subagente iniciado em outro local) pode ser vinculada novamente sem
uma reorganização de todo o armazenamento:

```bash
ai-memory move-session <session-id> --to my-project            # dry run
ai-memory move-session <session-id> --to my-project --confirm  # apply
ai-memory move-session --from-project tmp --to my-project --confirm
```

A sessão, suas observações, handoffs, tarefas de consolidação e sua
página `sessions/<id>.md` movem-se juntas; veja
[`docs/lifecycle-ops.md`](lifecycle-ops.md#move-session) para os modos de página,
proteções e o que permanece para trás.

## Corrigir carimbos de data/hora de sessões preenchidas retroativamente (backfill)

Uma execução de `backfill` anterior ao momento em que ela carregava o próprio tempo de evento da transcrição
data cada sessão importada no momento da importação, nivelando todo o histórico importado
em um único dia. Reler as transcrições locais corrige isso:

```bash
ai-memory repair-backfill-timestamps --project my-app            # dry run
ai-memory repair-backfill-timestamps --project my-app --confirm  # apply
```

Ele faz a correspondência de transcrições com sessões por id, nunca toca em `observations` ou
páginas, nunca atribui um horário de término a uma sessão ainda aberta e nunca propõe
um horário no futuro; veja
[`docs/lifecycle-ops.md`](lifecycle-ops.md#repair-backfill-timestamps) para as
regras de validação e o formato exato de requisição/resposta.

## Preferências de consolidação do projeto

Crie `_prompts/consolidation.md` na wiki de um projeto quando suas páginas compiladas
precisarem de preferências estáveis de estilo, terminologia, ênfase ou filtragem de ruído. Por
exemplo, seu corpo pode solicitar títulos em português ou omitir saídas rotineiras de CI.
A consolidação automática e ambos os modos manuais leem apenas a página do projeto
alvo. Passar `instructions` para `memory_consolidate` substitui essa página em uma
única chamada sem modificá-la.

A página e o valor por chamada permanecem dados de projeto não confiáveis. O ai-memory aplica o
sanitizador configurado, limita o valor a 2.000 caracteres e o codifica em JSON
na mensagem de usuário do LLM. Ambos os prompts de sistema de consolidação permitem apenas efeitos consultivos
de estilo, terminologia, ênfase e filtragem de ruído; o valor não pode adicionar
fatos, autorizar divulgação ou uso de ferramentas, nem sobrescrever regras de esquema, evidências e
saída. Páginas de preferência expiradas por TTL são ignoradas. Quando não houver página
ativa e nenhum argumento, o ai-memory não anexa nenhum bloco de preferência.

## Regras, memória e o perfil

O agente trabalha a partir de uma precedência ordenada; cada nível preenche lacunas naqueles
acima dele e nunca os substitui:

1. As instruções atuais do usuário na conversa.
2. O arquivo de regras do repositório: `CLAUDE.md` para o Claude Code; para Codex, Devin
   CLI, OpenCode, Cursor, Gemini CLI, Grok Build CLI, Kimi
   Code, Kiro CLI e Command Code, geralmente é `AGENTS.md`.
3. A memória do projeto: suas páginas, `_rules/`, decisões e gotchas.
4. O perfil entre projetos: as escolhas habituais do usuário em outros projetos
   (`docs/cross-project-profile.md`).

Portanto, uma regra rígida que deve valer em um repositório pertence ao seu arquivo de regras. Onde
nenhuma regra rígida existir, o agente recorre à memória em vez de perguntar novamente: as
páginas do próprio projeto primeiro, depois o perfil, que é o que permite a um novo projeto
começar com as escolhas habituais do usuário. Uma preferência que se aplica a todos os
projetos vai para o perfil (`memory_write_page` com `scope: "profile"`), e não
no arquivo de regras de cada repositório. A memória ainda nunca autoriza nada por
conta própria: ela define padrões, e o usuário e o arquivo de regras decidem.

Regras duradouras do projeto pertencem ao arquivo de regras do agente, não apenas na
wiki.

O consolidador classifica as observações compiladas como `decision`,
`fact`, `rule` ou `gotcha`. Páginas marcadas com tag de regra são roteadas para
`wiki/_rules/<slug>.md`, e `memory_lint` relata uma sugestão quando uma
regra parecer duradoura o suficiente para ser copiada para `CLAUDE.md` ou `AGENTS.md`.

O ai-memory nunca edita o arquivo de regras por conta própria. A sugestão do lint é
todo o fluxo de trabalho: copie a regra se ela deve se aplicar a cada turno, ignore-a
se foi apenas contexto temporário.

## Registros de decisão nativos do repositório (ADR)

Alguns projetos mantêm o raciocínio por trás do código no próprio repositório: um
diretório de ADR como `docs/adr/`, mantido manualmente ou por uma
ferramenta/servidor MCP dedicado a ADRs (por exemplo, [joshrotenberg/adrs](https://github.com/joshrotenberg/adrs)),
ou uma árvore `context/` do [Keep the Why](https://github.com/oliver-zehentleitner/keep-the-why)
(decisões, alternativas rejeitadas, restrições, revisadas em
pull requests). Três fatos enquadram como esse registro e o ai-memory interagem:

1. **O ai-memory nunca toca em arquivos do seu repositório.** Sua wiki reside
   no diretório de dados do servidor; as tarefas em segundo plano (consolidação,
   curadoria, decaimento de retenção, autoaprimoramento) leem e gravam apenas
   páginas wiki. Um diretório de registros de decisão no repositório está fora da
   superfície de gravação do ai-memory. Execute ambos lado a lado: o repositório detém o registro canônico, o ai-memory detém a
   recuperação entre sessões.

2. **Mantenha o diretório de registros fora da captura.** Um agente que lê o
   registro é capturado como qualquer outra leitura de arquivo, e a consolidação compila
   o que ele viu em páginas wiki, incluindo uma página em `decisions/` que diz
   "active" muito tempo depois de o repositório tê-la substituído, classificada em primeiro lugar pelo
   `memory_query` porque corresponde ao tópico. Liste o diretório na
   seção `[capture]` do marcador para que a cópia nunca seja feita:

   ```toml
   [capture]
   ignore_paths = ["docs/adr/**"]   # or ["context/**"] for Keep the Why
   ```

   O repositório detém esse registro; uma cópia compilada torna-se desatualizada no momento em que o
   repositório é alterado. Detalhes e limites em [`docs/marker-file.md`](marker-file.md),
   incluindo o que a correspondência de shell não consegue ver e como excluir resultados grandes
   de ferramentas que um agente salva e relê de outro caminho.

3. **Páginas wiki marcadas como `pinned: true` são imutáveis para a automação.**
   O decaimento de retenção e a curadoria as ignoram, a consolidação multipágina
   ignora qualquer atualização cujo caminho indique uma delas (com um aviso no log do
   servidor; `_slots/` mantêm seu próprio regime de estado/invariante), e o caminho
   de aplicação do autoaprimoramento recusa terminantemente reescrevê-las (a proposta
   é registrada como um conflito com o motivo). Desafixar é a opção explícita de
   saída.

Para um projeto sem registro no lado do repositório, as decisões vão *para* a wiki, e
a Agent Skill gerenciada durable-pages ensina aos agentes a receita:
`decisions/<slug>.md`, estrutura de ADR (Status / Contexto / Decisão /
Consequências, incluindo alternativas rejeitadas), `pinned: true` e
substituição por nova página em vez de editar o histórico. Peça a um agente para "record this as an architectural
decision" e a habilidade faz o resto. O formato estruturado também é recuperado
notavelmente melhor por meio de `memory_query` do que prosa em
formato livre.

## Investigar o uso de tokens no painel

Filtre o livro-razão autorizado completo antes da paginação por período UTC, modelo
exato, harness e sessão. From é inclusivo; Until é exclusivo. Filtros de turno
e ferramenta conectam uma resposta ao contexto nativo informado. "Turn & tools"
separa uma ferramenta solicitada, um resultado de entrada e uma associação de mesmo turno, e
o contexto compartilhado não prova que o custo em tokens pertença a uma única ferramenta. Atualize
clientes nativos bem como o servidor para coletar esses campos. Linhas antigas sem
metadados de origem permanecem desconhecidas. O
[guia de recursos](features.md#contabilização-e-investigação-de-tokens) explica
fallbacks de carimbo de data/hora, totais filtrados e pré-requisitos de captura.

Abra um projeto sob `/web` e selecione a aba Token usage. Hooks nativos atualizados do Claude Code e
Codex enviam o uso medido de respostas em `post-tool-use`, `stop`,
`pre-compact` e `session-end`. O servidor armazena o uso conforme ele chega, inclusive enquanto uma
sessão estiver aberta. O painel e os totais de sessão são atualizados a cada dez segundos. Para
habilitar a captura, atualize o cliente nativo em cada máquina de agente bem como o
servidor.

O Pi envia totais cumulativos de sessão em `agent_end` e `session_shutdown`.
Atualize tanto o binário do servidor quanto o do cliente, execute
`ai-memory install-hooks --agent pi --apply`, e depois `/reload` em uma sessão
existente do Pi. A extensão lê as entradas salvas, portanto recargas, retomadas e relatórios
repetidos não reiniciam nem contam os totais em duplicidade. Novas entradas, saídas, leituras de
cache e gravações de cache permanecem separadas; uso ausente permanece desconhecido. Uso salvo
de compactação, resumo de ramificação, aquecimento de cache e ferramentas aninhadas também contam quando
o Pi os registra.

Esses totais seguem a contabilização de `/session` do Pi: todas as entradas na sessão,
incluindo ramificações abandonadas e histórico copiado para uma bifurcação (fork). O total de uma bifurcação,
portanto, inclui o uso herdado, não apenas o trabalho faturado após a bifurcação.
O Pi **não** envia registros de respostas individuais para o livro-razão de uso de tokens ainda;
totais de sessão e cobertura podem estar presentes enquanto sua tabela de respostas estiver vazia.

Os cartões de métricas são controles de navegação. Cartões de espaço de trabalho ordenam a lista
de projetos pela contagem selecionada, e Open sessions também seleciona projetos que possuem
uma sessão não finalizada. Cartões de projeto abrem a aba correspondente de Agents, Sessions,
Knowledge ou Token usage. Captured events ordena sessões por contagem de eventos,
e Open sessions filtra a tabela para sessões não finalizadas. Na página de um agente,
cartões de tokens e chamadas abrem o livro-razão de respostas do projeto já filtrado para
aquele harness.

Respostas são desduplicadas por sua identidade nativa, de modo que uma atualização parcial aumenta
os contadores armazenados em vez de adicionar a mesma resposta novamente. Novas entradas
excluem leituras de cache. Busca, filtragem de sinais e ordenação recente consultam todas
as respostas capturadas antes que páginas de cinquenta linhas sejam selecionadas. Links de página mantêm esses
filtros, e resultados de busca e totais permanecem separados. Respostas conectam-se à sua
sessão e mostram modelo, entrada, saída, cache e total de tokens. O painel sinaliza
contextos de pelo menos 100.000 tokens, saídas de pelo menos 20.000 tokens e
respostas acima de três vezes a média de respostas capturadas do projeto assim que existirem cinco
respostas. Estas são dicas para investigação, não anomalias confirmadas ou
custos monetários.

A extração ao vivo lê em lotes delimitados de 4 MiB e relata no máximo 256
respostas por evento. Um cursor sob o diretório de dados do cliente avança apenas
após o enfileiramento local ser durável. Hooks posteriores continuam qualquer backlog, incluindo
registros fora da janela de bytes anterior, e respostas repetidas permanecem
desduplicadas. Um backlog grande necessita de vários eventos, e uma sessão sem
hooks posteriores não esgota sua transcrição por conta própria. Linhas incompletas aguardam
pela conclusão, e linhas com mais de 4 MiB são ignoradas em blocos delimitados. Truncamento
ou alteração no conteúdo do ponto de verificação reinicia a varredura. Atualize o binário nativo em
cada máquina de agente, porque atualizar apenas o servidor não altera a captura de
transcrições. Relatórios aparecem depois que o agente grava o uso. Registros ausentes ou
excluídos e formatos não compatíveis permanecem desconhecidos.

O livro-razão mantém contadores, identificadores de resposta e de modelo, e referências delimitadas de
turno nativo, tempo de resposta e ferramentas quando informados. Exclui prompts, argumentos
de ferramentas e saídas. Rótulos de modelos e ferramentas são saneados antes de seu limite de comprimento
ser aplicado, e identificadores de contexto privados ou malformados são recusados.
Filtros de autorização de projeto e proprietário de sessão se aplicam antes da classificação ou
agregação. Totais de término de sessão permanecem como fallback para o histórico.

O controle Light / Dark / System do cabeçalho armazena a preferência no navegador.
System segue alterações do sistema operacional, e uma escolha manual a sobrepõe.

### Explorar agentes e sessões

A barra lateral do espaço de trabalho vincula a projetos, caixa de entrada, workstreams, grafo de
conhecimento, revisão de aprendizado e integridade do servidor no desktop e no celular.
Cartões de projeto mostram os harnesses registrados e podem ser filtrados por nome, sessões
abertas ou mensagens enfileiradas. Uma sessão aberta não possui evento de término registrado; o agente pode não estar mais online.

Abas de projeto mostram uma seção de cada vez. Overview resume capturas e mensagens
pendentes, Agents agrupa harnesses, Token usage investiga respostas, Sessions
lista sessões recentes, e Knowledge navega pelas páginas salvas. A aba selecionada é
mantida na URL (`?tab=agents`, por exemplo), de modo que recarregamento, links compartilhados e
o avançar e voltar do navegador mantenham a mesma seção. Marcadores de seção como
`#agents` ainda abrem a aba correspondente. A barra de abas aceita Esquerda e Direita,
Home e End, e quebra linha em telas estreitas. Inbox e Link graph abrem visões
separadas.

Em Agents, os cartões de agentes somam o uso informado entre sessões e mostram quantas
sessões o relataram. "Explore sessions" pagina pelas sessões de um harness,
e uma sessão abre seu livro-razão de respostas e a linha do tempo de eventos capturados. Uso
ausente permanece desconhecido. Totais de sessões legadas podem existir sem registros
individuais de respostas, portanto a cobertura e as contagens de chamadas capturadas são mostradas separadamente.

A tabela de respostas pode ser buscada por modelo, agente ou resposta, filtrada por
sinal de investigação e ordenada por uso de tokens ou horário de captura. Esses controles
consultam todas as respostas capturadas no escopo atual antes que páginas de cinquenta linhas sejam
selecionadas. O uso do projeto e da sessão é atualizado a cada dez segundos enquanto estiver visível, e
os controles de filtro e o foco do teclado permanecem no lugar.

### Coordenar trabalho a partir do navegador

A Caixa de Entrada (Inbox) lista as mensagens que um projeto selecionado recebeu e enviou, cinquenta por
página, sem reivindicá-las. Mensagens recebidas abrem nas mensagens pendentes, e "All
messages" ou "Collected" mostram o histórico. Links de páginas mantêm o estado selecionado.
"Waiting for agents" mostra contagens reais de mensagens enfileiradas e links de caixa de correio para cada
espaço de trabalho e projeto que você pode acessar. Por padrão, a página abre um projeto com
mensagens enfileiradas, e o título nomeia a caixa de correio atual. A barra lateral
atualiza contagens pendentes a cada dez segundos em páginas visíveis, e seus links de
projetos abrem "Awaiting collection". Mensagens coletadas saem da contagem. A contagem é um
indicador de fila, não um rastreador de lidas ou não lidas por humanos. "Message state" filtra o
histórico antes da paginação. A leitura por meio de uma notificação não reivindica a mensagem.
Expandir uma linha mostra seu texto simples. New message escolhe um projeto de origem e um
harness destinatário a partir dos agentes que já possuem sessões em projetos
visíveis. O envio precisa de acesso de gravação a ambos os projetos e passa pela
política configurada de sanitizador e admissão de mensagens. Sessões de navegador usam as
verificações CSRF existentes. Uma mensagem enfileirada possui um botão Edit. A mensagem sai da
fila enquanto você altera seu assunto ou texto. Salvar ou descartar a retorna para
seu lugar original. Uma mensagem que um agente já coletou
não pode ser editada.

Uma solicitação enfileirada não inicia um agente. O harness endereçado a coleta na
próxima vez que verificar sua caixa de entrada, e "Collected" registra essa reivindicação, não que a
tarefa teve êxito. Clientes MCP estáticos passam seu harness no argumento `agent` de
`memory_message_pop`. Veja [mensagens entre agentes](agent-messaging.md).

A navegação e o detalhamento seguem a abordagem de visão geral até o detalhe do
[Grafana](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/best-practices/)
e o agrupamento de sessões documentado pelo
[Langfuse](https://langfuse.com/docs/observability/features/sessions). Eles são
referências de interação. A interface integrada ainda é HTML e CSS gerados via Cargo
com pequenos aprimoramentos em JavaScript puro.
