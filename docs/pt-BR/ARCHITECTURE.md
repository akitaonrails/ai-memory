# ai-memory - Arquitetura

> Documento canônico único para "o que é esta ferramenta e como ela é estruturada".
> Pesquisas extensas vivem ao lado deste arquivo sob [`docs/`](.); esta
> página é o resumo operacional para quem estiver lendo o código.

## Propósito

O ai-memory é um binário Rust único que oferece aos agentes de código na
[Matriz de Suporte do README](../../README.md#support-matrix), além de outros clientes
compatíveis com MCP, memória de longo prazo compartilhada entre CLIs.
Você pode sair de um no meio da tarefa, abrir outro no mesmo diretório e
continuar, sem chamadas manuais a `write_note` ou copiar e colar resumos
entre sessões.

O que se acumula é uma **wiki de LLM no estilo Karpathy**: uma árvore de páginas markdown
no disco com controle de versão em git que é *compilada* e incrementada ao longo do tempo.
Páginas são versionadas no próprio local via substituição, conceitos semânticos constroem-se
uns sobre os outros e logs episódicos decaem. Um
índice SQLite complementar fornece recuperação via FTS5 + entidade lexical + vizinho de link,
com vetores opcionais; o markdown permanece a fonte da verdade.

O recurso opcional `management-ui` habilita um console askama/Tailwind em
`/manage`, ativado separadamente em tempo de execução. Sua página de login é pública; suas
rotas de dados e mutação `/management/v1` reutilizam autenticação e CSRF de sessão humana.
Ações de administração usam manipuladores exclusivos de root existentes. Expurgos de sessão de autoatendimento
reverificam a propriedade exata e o acesso de escrita atual do projeto dentro da transação
do gravador, e depois removem arquivos via `Wiki`. Páginas compartilhadas mantêm o comportamento
de leitura existente. Consulte [management-ui.md](management-ui.md).

## Fluxo de dados

![Visão geral da arquitetura do ai-memory](architecture-overview.svg)

Setas sólidas são caminhos de requisição, leitura e escrita. Setas tracejadas são
reconciliações em segundo plano ou manutenções apoiadas por provedores. O invariante central permanece
inalterado: a wiki em markdown é a fonte da verdade, e o SQLite é o índice derivado
para busca, sessões, observações, handoffs, auditoria, embeddings e o registro contábil
opcional de continuidade de workstreams gerenciados.
O autoaperfeiçoamento fica no lado da manutenção apoiada por provedores: o servidor
agenda revisões para sessões recém-concluídas em cada projeto, registra
propostas validadas na trilha de auditoria de escritas pendentes e as aprova automaticamente
pelo caminho normal de escrita da wiki por padrão. Ciclos do agendador não
se sobrepõem; passagens longas de revisão de todos os projetos atrasam o próximo ciclo em vez
de iniciar outra cópia. O agendamento e a aprovação são separados. Admins podem definir
`[auto_improve.scheduler] enabled = false` para interromper revisões em segundo plano, ou
`[auto_improve] require_approval = true` para deixar propostas agendadas e manuais
pendentes para revisão. Operadores podem, adicionalmente, configurar `[auto_improve.eval]` para executar
um portão executável fornecido pelo projeto para prefixos de propostas selecionados após a validação
do LLM e antes da preparação/aprovação; ele vem desativado por padrão e nunca é executado
a partir dos caminhos de hooks.

**Ciclo de estado estável:**

1. A CLI do agente emite um hook de ciclo de vida (SessionStart, UserPromptSubmit,
   PostToolUse, …). Hooks em scripts shell executam `curl` com o JSON do evento para `POST /hook`
   com um tempo limite curto. Comandos nativos `ai-memory hook --event ...` armazenam em spool
   eventos localmente com uma chave de idempotência estável por entrada, realizam uma limpeza curta
   e limitada no início da sessão e delegam
   a entrega do fim da sessão a um utilitário `hook-drain` desacoplado e ciente de locks (o Zero,
   cuja sandbox de hooks encerra processos filhos desacoplados, drena em processo dentro de um
   orçamento fixo de 10 segundos);
   operadores de alta latência podem elevar os limites de drenagem/handoff/segundo plano com
   variáveis de ambiente baseadas em minutos.
   Caminhos críticos de agentes nunca bloqueiam na rede; servidores saturados retornam HTTP
   429 em vez de enfileirar trabalho ilimitado. Uma drenagem de spool nativo pausa aquele
   endpoint pelo restante de sua passagem sem cobrar tentativas de repetição; eventos enfileirados
   para outros endpoints continuam a ser entregues com suas próprias credenciais.
   Para comandos nativos compatíveis e integrações geradas com OpenCode/OMP/Pi/OpenClaw,
    a política de captura do marcador mais próximo executa primeiro: um evento de ferramenta de
    arquivo reconhecido e descartado nunca entra no spool, fila, transporte, logs ou
    armazenamento. O marcador de configurações mais próximo separado também pode encaminhar um valor
    de consulta de array JSON limitado `aliases` tanto na captura quanto na pesquisa de handoff no
    SessionStart. O roteador o aceita somente com proveniência de marcador e a identidade remota git
    com host normalizada do checkout, e resolve nomes dentro do espaço de trabalho selecionado;
    a chave de cache inclui apelidos, proveniência e
    modo de leitura/escrita. Consulte
    [a referência de marcadores](marker-file.md).

2. O roteador de hooks do servidor sanitiza o payload (o único caminho de
   texto não confiável para dentro do armazenamento), atribui um [`ObservationKind`] e
   enfileira um `WriteCmd` para o ator gravador. Para eventos nativos com chave, a chave
   com escopo de projeto e a observação são confirmadas juntas. Chaves normais expiram
   após 30 dias; chaves validadas `recovery_<55 hex>` são vinculadas à sessão real,
   ao agente, ao tipo de evento e à identidade sanitizada do evento, sendo retidas até
   a exclusão da sessão/projeto, de modo que um gravador não possa reivindicar previamente outro evento
   de recuperação e um diário antigo não possa duplicar observações após a limpeza
   comum. A chave é marcada
   como concluída somente após o processamento downstream: uma reprodução incompleta retoma
   efeitos de wiki/handoff sem outra observação, enquanto uma reprodução concluída
   é confirmada e ignorada. Um portão delimitado por projeto/chave serializa uma
   repetição sobreposta com o processador original. Efeitos downstream permanecem
   pelo menos uma vez até esse marcador de conclusão, portanto uma falha de processo durante esses
   efeitos pode repetir um efeito já aplicado em vez de perder silenciosamente o
   restante. Para um SessionEnd interrompido e já finalizado, a repetição converge o
   commit da wiki, o trabalho durável do provedor e a chave pendente sem acrescentar outra
   observação. O arquivo `log.md` recebe uma linha anexada
   `## [YYYY-MM-DDTHH:MM:SSZ] <event> | <title>`.
   Confirmações inline em `/hook/batch` incluem um resultado por índice reconhecido.
   Contadores de tempo de vida do processo distinguem eventos armazenados, reproduções, recuperações e
   encerramentos ignorados. `last_persisted_ms` avança após uma escrita durável ou efeito terminal;
   repetições e encerramentos sem operação não o avançam. Checkpoints normais reutilizam
   o timestamp de escrita da observação; apenas a recuperação verifica uma nova versão de página.
3. Em eventos `SessionEnd` verdadeiros, o servidor sintetiza uma página de resumo
   `sessions/<id>.md` (baseada em regras, sem LLM) e abre uma
   linha `Handoff` para o próximo agente. Uma transação SQLite insere esse
   handoff automático, marca a sessão como encerrada e registra a contagem de
   observações cobertas, para que a recuperação nunca veja apenas metade desses efeitos no banco de dados. Um
   SessionEnd posterior executa o caminho novamente apenas quando essa geração avança, de modo que sessões
   retomadas são capturadas enquanto entregas duplicadas e variações de relógio
   convergem. Sessões finalizadas existentes recebem linha de base na migração em vez de
   se tornarem trabalho histórico acumulado. Commita a wiki automaticamente. Clientes
   sem um hook de fim de sessão verdadeiro confiável necessitam de uma ação explícita de encerramento:
   `ai-memory finalize-session --agent antigravity-cli` para o Antigravity CLI
   (o Codex possui um `SessionEnd` nativo desde a CLI 0.145.0; `finalize-session
   --agent codex` é apenas o recurso alternativo em versões mais antigas do Codex). O OpenCode 2 e
   o Antigravity também publicam um checkpoint de turno em um `Stop` finalizado: a
   página da sessão e o único handoff aberto da sessão são atualizados no local
   enquanto a sessão permanece aberta.
   O comando seleciona a sessão aberta correspondente mais recente e ingressa no mesmo
   caminho canônico de SessionEnd de um hook nativo. O frontmatter da página de
   sessão gerada registra `session_id` além do `sessions.agent_kind` imutável
   como `agent`; ele descreve a origem de harness da página, não o gravador posterior.
   Escritas manuais de página não recebem metadados de agente inferidos. Uma sessão que
   nunca entregou um `SessionEnd` (harness travado, terminal fechado) é encerrada
   pelo ciclo de manutenção quando `[sessions] idle_end_after_hours` estiver definido, com
   a escrita de página própria do ciclo, o trabalho de consolidação e a liberação de handoff
   exclusiva de ciclo de vida — sem handoff adicional a partir do ciclo. A escrita da página é
   condicional à versão da página que a seleção capturou em sua própria
   instrução, e a marcação de encerramento é um CAS em `ended_at IS NULL` que também
   reverifica a contagem de observações da sessão (a população do resumo) e
   a versão mais recente da página — de modo que um `SessionEnd` real tardio (ou um checkpoint)
   vence essa disputa e converge através de `AlreadyEnded`/`ReEnd`. `ended_at` é
   o relógio do ciclo (a última atividade da sessão é preservada no detalhe
   da auditoria), e o trabalho de consolidação é enfileirado dentro da transação
   da marcação.
   Se um harness for interrompido antes do `SessionEnd`, o próximo `SessionStart` também
   procura a sessão aberta substantiva ativa mais recentemente no mesmo
   projeto e fronteira de propriedade. Ele renderiza um snapshot de recuperação somente leitura
   a partir das 64 observações armazenadas mais novas, limitado a 6.000 caracteres. Um
   handoff que cubra o trabalho mais novo suprime o snapshot, e quando um
   handoff é entregue, outros trabalhos não cobertos continuam como ponteiros apenas de metadados.
   A origem permanece aberta (agentes paralelos são válidos), nenhuma linha extra de handoff é
   criada e nada de outros escopos de projeto é carregado. Quando trechos de `Stop`
   do assistente estão ativados (eles passam por um portão de privacidade), o trecho é o
   checkpoint recente preferido. Caso contrário, prompts recentes e metadados de ciclo de vida ou
   de ferramentas fornecem contexto para retomar o trabalho.
4. Quando `AI_MEMORY_LLM_PROVIDER` estiver definido, `memory_consolidate` reescreve
   esse resumo em uma página durável mais rica ou faz fan-out para um
   lote de múltiplas páginas sob `concepts/`, `decisions/`, `gotchas/`. Prompts de consolidação
   preservam o idioma natural dominante do material de origem e solicitam
   ao modelo que conecte páginas relacionadas com wikilinks baseados em caminhos. Os prompts padrão
   de página única e de lote iniciam o conteúdo do usuário com seus blocos
   estáveis (o texto fixo de lote, ou seja, cabeçalho + esquema de campos, e as
   instruções do projeto), seguidos pelo corpo atual / estado do slot e título, e
   somente então o id da sessão e o dump de observações, para que provedores com cache de
   prefixo possam reutilizar o prefixo estável. No prompt de página única, o corpo atual
   é o próprio corpo da página da sessão (`sessions/<id>.md`), variando por
   sessão: o prefixo compartilhado entre sessões de um projeto é o prompt de
   sistema mais o bloco de instruções quando presente, e o bloco de títulos
   segue o corpo variável. O orçamento de entrada permanece uma estimativa
   aproximada de caracteres; elegibilidade de cache não é um acerto medido.
5. Quando um provedor de LLM está configurado, o agendador de autoaperfeiçoamento revisa
   sessões recém-concluídas em todos os projetos fora da latência de hooks. Ele registra propostas
   validadas em `concepts/`, `decisions/`, `gotchas/`, `procedures/` e `_rules/`
   na trilha de auditoria de escritas pendentes e as aprova pelo caminho de mutação
   da wiki por padrão. O agendador inicializa uma marca d'água de primeira execução por projeto
   para que sessões históricas não sejam processadas automaticamente no upgrade,
   registrando reivindicações por sessão antes do trabalho com LLM para que revisões agendadas que falharem
   não repitam indefinidamente. Chamadas explícitas de autoaperfeiçoamento via CLI/admin/MCP usam o mesmo
   pipeline para repetições direcionadas ou recuperação. Com `[auto_improve]
   require_approval = true`, propostas agendadas e manuais permanecem pendentes até
   a aprovação explícita em escritas pendentes. Se `[auto_improve.eval] enabled = true`,
   propostas direcionadas (padrão `_rules/` e `procedures/`) devem passar no
   contrato JSON executável configurado antes de serem preparadas; falhas tornam-se
   candidatas rejeitadas/entradas no buffer de rejeição em vez de escritas na wiki. Assim como
   os prompts de consolidação, o revisor por sessão, a passagem de experiência entre
   sessões, a mesclagem de sonho e a passagem de contradição de lint escrevem
   prosa no idioma natural dominante de suas entradas.
   Todo prompt de LLM trata texto de repositório, observações, páginas da wiki e propostas
   anteriores como dados não confiáveis em vez de instruções. A mesma fronteira de confiança
   explícita e delimitadores precedem handoffs injetados automaticamente,
   resumos de projeto e pacotes de workstreams gerenciados; instruções atuais e
   o estado do checkout permanecem autoritativos.
6. `memory_query` responde via FTS5 + correspondência de entidade + RRF de vizinho de link; um
   fluxo FTS auxiliar de unigrama/bigrama mantido transacionalmente lida com consultas em
   Han, Kana e Hangul sem espaços sem alterar o caminho unicode61 comum para textos não CJK. Quando
   um embedder estiver configurado,
   o cosseno vetorial sobre `page_embeddings` une-se ao mesmo
   RRF. O índice de entidades é derivado da lista canônica `entities` do frontmatter,
   e um índice vazio não contribui com candidatos ou pontuação. Antes do truncamento
   final, um multiplicador de autoridade limitado ajusta a relevância usando o tipo de página
   canônico, camada, `pinned` e tags explícitas positivas/negativas no frontmatter.
   Ele favorece regras, decisões, procedimentos e gotchas mantidos em disputas
   acirradas, enquanto mantém evidências episódicas, históricas, de lint e de testes
   pesquisáveis. Dois ajustes de feedback opcionais unem-se a essa mesma contenção
   de autoridade: feedback aberto de `stale`/`wrong` pode rebaixar a versão atual de uma página,
   e uma leitura prévia após consulta ou associação útil pode impulsioná-la para uma
   consulta semelhante. Ambos têm padrão zero, limpam na reescrita e aparecem nos detalhes
   de pontuação de `explain=true`. Nenhuma regex de intenção de consulta ou exclusão rígida
   participa. Uma
   passagem opcional com `AI_MEMORY_RERANKER=llm` envia uma consulta limitada mais até 30
   títulos/trechos limitados ao provedor configurado após a fusão de projeto/escopo;
   ela é limitada a uma chamada por consulta e quatro chamadas em andamento, e qualquer tentativa
   inválida, com falha, com tempo limite esgotado ou saturada preserva a ordem local.
   Resultados globais e de preferências globais suplementares não seguem esse caminho.
   Com `[retrieval] observation_stream` (ativo por padrão), buscas de projeto e de `scopes`
   adicionam mais um fluxo RRF: busca textual completa sobre `observations_fts`
   no mesmo `(workspace, project)`, onde cada sessão correspondente vota uma vez,
   através de sua observação melhor classificada, por sua própria página não expirada mais recente
   `sessions/<id>.md` naquele escopo. O voto nunca cruza projetos (tanto
   a observação quanto a página são filtradas por escopo), uma sessão sem página não
   contribui em nada e nada é escrito. Defini-lo como `false` restaura a classificação
   anterior byte a byte. As observações de todo operador votam, já que páginas de sessão
   são compartilhadas, mas o trecho e os ids de observação do voto são anexados
   apenas quando o chamador pode ler aquela sessão sob a regra de proprietário da linha do
   tempo de sessões (suas próprias sessões e as não atribuídas); caso contrário, o resultado
   exibe o trecho da própria página. Se
   páginas compiladas da wiki errarem totalmente no modo padrão, de projeto explícito ou de `scopes`
   explícito, o FTS de observações brutas limitado retorna `raw_hits` de contingência, a partir
   de sessões que o chamador pode ler sob essa mesma regra. Resultados que apenas os fluxos
   vetoriais (ou vizinhos de grafo deles) encontraram contam como falha aqui: esses fluxos
   não possuem piso de similaridade e respondem a qualquer consulta quando uma página possui
   um embedding;
   `global=true` pesquisa somente páginas compiladas da wiki entre projetos. Acertos de páginas
   incrementam `access_count` + `last_accessed_at`, o termo de reforço M8, que o
   `memory_feedback` complementa com relevância explícita por página. Operadores identificados
   também adicionam uma linha `page_access` por página; uma opção
   `[decay] breadth_weight` pode recompensar páginas reforçadas por vários operadores
   distintos. Esse incremento é limitado a no máximo uma vez por página por minuto, de forma que uma
   rajada de buscas sobrepostas não inunde o ator gravador com escritas redundantes
   de reforço. O mesmo reforço é acionado a partir de todo caminho de leitura que exibe uma página,
   não apenas de buscas: `memory_read_page` (e seu percurso de `include_related`, que reforça
   os vizinhos percorridos também) e `memory_explore` (as páginas que ele exibe) incrementam
   os mesmos contadores por meio do mesmo caminho limitado e isento de FTS, de modo que uma página
   aberta diretamente por um humano ou exibida pelo grafo resiste ao decaimento como um acerto
   de consulta (design-memory-aging.md C1).
7. A varredura de esquecimento executa sob demanda e no cronograma de `[maintenance]`
   do servidor: páginas além do TTL de seu frontmatter `expires_at:` são
   excluídas permanentemente pela camada da wiki (arquivo + linhas, fixadas ou não);
   páginas com `retention < cold_threshold` são despejadas pela camada da wiki,
   o que remove o arquivo autoritativo e deixa uma lápide de decaimento;
   lápides mais antigas que `hard_delete_after_days` são expurgadas com toda a sua
   ancestralidade de versões apenas no espaço de trabalho/projeto resolvido daquela varredura,
   juntamente com linhas do índice de entidades que ficaram órfãs pelo expurgo. Uma página mais nova
   recriada no mesmo caminho é preservada. Páginas semânticas / fixadas / recém-tocadas
   sobrevivem. Uma quarta passagem, desativada a menos que `[decay] observation_retention_days`
   seja positivo, exclui `observations` brutas mais antigas que essa idade, mas apenas
   para sessões já consolidadas em uma página de resumo cujo caminho ainda tenha
   um cabeçalho ativo (uma reescrita posterior dessa página conta; um cabeçalho despejado ou
   excluído não), de forma que a captura bruta nunca seja removida enquanto for a última cópia
   do trabalho daquela sessão, e nunca para uma sessão que ainda possua uma linha de
   `session_consolidation_jobs` em `pending` ou `running` — uma consolidação enfileirada (ou
   com concessão desatualizada) ainda lê a captura bruta — e nunca enquanto a contagem de observações
   ativas for mais recente do que `sessions.ended_observation_count`, o que fecha o intervalo
   entre a captura retomada e sua próxima inserção na fila. A contagem de execução de teste
   aplica a mesma proteção para o mesmo snapshot. Ela executa por último para que as desocupações
   e exclusões permanentes da própria execução já excluam suas sessões, excluindo em
   transações de `observation_prune_batch` para que uma poda de vários milhões de linhas não possa
   reter o lock de escrita, e repara
   `sessions.ended_observation_count` para baixo na mesma transação. A
   poda é irreversível em um sentido específico: observações são a entrada para a
   consolidação, portanto uma sessão podada nunca poderá ser reconsolidada (nem com
   um modelo melhor, um prompt melhor ou correção de bug no consolidador), e sua
   página de resumo torna-se o único relato sobrevivente daquela sessão. Páginas
   SQLite liberadas são reutilizadas em vez de devolvidas ao SO: o arquivo `.db` não
   diminui, mas o arquivo tarball de backup sim. Uma quinta passagem, desativada a menos que
   `[decay] ledger_retention_months` seja positivo (variável de ambiente
   `AI_MEMORY_DECAY__LEDGER_RETENTION_MONTHS`), exclui registros antigos de eventos de hooks
   (`log-YYYY-MM.md`) assim que essa quantidade de meses civis UTC inteiros tiver passado
   após o término do mês deles. Ela qualifica um arquivo por nome E conteúdo (os
   predicados de `log_ledger`), nunca toca em `log.md`, nunca toca no mês atual
   ou anterior qualquer que seja a configuração, e remove arquivos através de
   `Wiki::delete_event_ledger` (confinado, sob a guarda de mutação da wiki, portão de
   conteúdo reverificado) em vez de diretamente da varredura. Cada remoção consulta a
   cadeia de admissão bloqueante na operação `delete_event_ledger`, a mesma verificação que
   uma exclusão de página executa: um webhook com política `reject` mantém o arquivo, o qual
   a varredura contabiliza em `ledgers_undeleted`, continua e tenta novamente na próxima execução —
   e observadores não bloqueantes são despachados apenas após o desvínculo. Registros contábeis
   não são páginas:
   o armazenamento é somente lido, e uma linha legada de `pages` para um registro contábil excluído é deixada
   para `reclaim-ledger-versions`. Uma execução de teste relata os arquivos e bytes que seriam
   excluídos (`ledgers_prunable`, `ledger_bytes_prunable`) e não exclui nada.
   Ciclos agendados de varredura, lint baseado em regras, lint com LLM opcional e preenchimento
   retroativo opcional de embeddings
   enumeram todo escopo existente de espaço de trabalho/projeto antes de executar o trabalho por projeto,
   correspondendo ao modelo de escopo abrangente de todo o armazenamento do agendador de autoaperfeiçoamento. Uma
   limpeza diária separada remove linhas de projetos com mais de uma semana apenas quando eles não contêm
   páginas, sessões, observações, handoffs, workstreams gerenciados, mensagens de agentes
   enviadas ou recebidas (incluindo histórico reivindicado/cancelado) ou dados de
   autoaperfeiçoamento; o histórico de continuidade gerenciada, portanto, mantém o escopo de seu projeto
   ativo mesmo quando nenhuma sessão de hook de ciclo de vida tiver sido capturada.
   O mesmo ciclo agendado de varredura também executa um `UPDATE` em lote
   de `close_abandoned_sessions` em todo o armazenamento. Uma sessão sem atividade
   por 48 horas e sem `SessionEnd` tem seu `ended_at` definido a partir de sua própria última
   atividade, sem página de resumo e sem uso. Isso evita que
   `open_session_count` cresça em sessões que ninguém continua, sendo mais
   restrito do que um encerramento real (veja a linha de uso da sessão abaixo).
   Uma sessão que é continuada, significando que uma nova sessão se inicia no mesmo
   escopo enquanto a antiga esteve inativa por 15 minutos ou mais, é fechada mais
   cedo no cliente, incluindo recuperação de uso. Após esse limite, o
   pacote de recuperação somente leitura que `render_interrupted_session_context`
   (`ai-memory-hooks/src/router.rs`) constrói também carrega um marcador analisado por
   máquina. O cliente o utiliza para encontrar a transcrição da sessão antiga, relatar
   seu uso e enviar um `SessionEnd` sintético normal. Isso é seguro mesmo se a
   sessão antiga ainda estiver ativa: um `SessionEnd` real posterior com mais
   observações do que o encerramento antecipado viu executa o caminho de encerramento completo
   (`SessionEndDisposition::ReEndWithNewWork`, issue #152).
8. Backups: `ai-memory backup --to <tarball>` usa a API de backup online do
   SQLite para que a origem permaneça gravável; `ai-memory restore`
   reverte. Ou: `git push` no diretório da wiki + `rsync` no diretório de dados.

**Aviso de continuidade no início da sessão:** `GET /handoff` também nomeia uma sessão que
foi interrompida sem ser finalizada. Ele lê as sessões do projeto ordenadas por sua
observação mais recente de prompt, ferramenta, stop ou compactação nos últimos sete dias
(`ReaderPool::recent_session_activity`, filtrado por proprietário na linha da sessão). Ele
adiciona um aviso quando a mais recente não for a própria sessão do receptor, nem
tiver sido finalizada nem escrito um handoff após aquele trabalho (um bastão de checkpoint por turno
cobre o turno encerrado), sendo mais recente do que o trabalho próprio do receptor e qualquer
handoff entregue. O aviso carrega apenas o tipo de agente, id de sessão, idade e nomes de
escopo, nunca texto capturado, apontando para
`memory_read_session_observations`. Um cliente que busca novamente a cada prompt
envia `profile_digest=0` após o primeiro, o que também cessa o aviso.
`memory_briefing` lista a mesma ordenação de `recent_sessions`, e
tem como padrão cinco entradas; `recent_sessions_limit` eleva esse limite para até 50.
`memory_handoff_list` nomeia a sessão como `newer_unended_session` quando for
mais recente que todo handoff aberto. `memory_read_session_observations` sem um
`session_id` lê a partir da mesma ordenação: a sessão mais recente de outro no diretório
de trabalho atual da sessão chamadora, e depois a sessão mais recente de outro no escopo.

**Resumo de projeto no início da sessão:** quando o marcador do projeto define
`[briefing] inject_on_session_start`, `GET /handoff` anexa um resumo das páginas
fixadas, de `_rules/` e de `_slots/` do projeto, empacotadas em `[briefing] max_chars`
(limitado entre 1500-20000) sem nenhuma chamada a LLM. Toda página recebe primeiro um
trecho extrativo L0 (`ai_memory_core::excerpt`: frontmatter `abstract`, senão `summary`,
senão a prosa inicial lida através de parágrafos e listas, 256
bytes); os bytes restantes aprofundam
as páginas para um trecho L1 (`summary` primeiro, 500 bytes, o mesmo corte da compactação
A2) e depois para o corpo inteiro, páginas fixadas e de `_slots/` antes das
demais, outros corpos do mais econômico para o mais caro (`ai_memory_hooks::brief_tiers`). Um corpo é incluído inteiro ou não é incluído.
Um corpo mais curto que o trecho de frontmatter que ele substituiria é sempre exibido
por inteiro, tarifado pelo seu próprio tamanho.
Quatro ou mais trechos não fixados de uma mesma família compartilham um título. Páginas que não
couberem são listadas por caminho para `memory_read_page`, e sua contagem é
reservada antes que qualquer página seja aprofundada. O aviso de segurança, a cerca de
histórico não confiável (com marcadores de cerca dentro do texto da página escapados) e o
rodapé de encerramento nunca são sacrificados por conteúdo. O algoritmo aceito e seus
invariantes de orçamento estão registrados em
[`design-tiered-session-brief.md`](design-tiered-session-brief.md).

**Resumo para subagentes:** quando o marcador define `[briefing] inject_on_subagent_start`,
o hook `SubagentStart` do agente `claude-code` imprime o mesmo resumo como `additionalContext`
do subagente. Ele provém de `GET /brief`, que resolve e autoriza o projeto exatamente como
o `/handoff` faz (um utilitário compartilhado) e renderiza somente o resumo:
ele nunca lê ou reivindica um handoff, não publica nenhum projeto ativo e não registra
nenhuma flag de `[profile]`, já que um subagente é executado dentro da sessão de seu pai.
Para esse agente, o orçamento também é limitado a 9.800 caracteres, abaixo do
limite de contexto de hook de 10.000 caracteres. Um servidor sem a rota responde
404 e o hook imprime `{}` como antes; ele nunca recorre a `/handoff`.

**Perfil entre projetos:** após o resumo do projeto, o SessionStart anexa um resumo delimitado,
orçado em bytes e estável em bytes das escolhas habituais do usuário em outros projetos
(`docs/cross-project-profile.md`), filtrado pela stack que a atividade do projeto
indica, com uma linha de base maior para um projeto que ainda não possui páginas. Ele
vem ativo por padrão para um servidor de operador único e desligado para um multiusuário.
Entradas são aprendidas pelo ciclo de perfil (`ai_memory_consolidate::profile`), executado
por um worker do servidor logo após a inicialização, em um momento após debounce a cada
SessionEnd (`HookState::profile_notify`), a cada hora e em
`POST /admin/profile/rebuild`. Ele **coleta** cada projeto participante além
de suas marcas (`profile_harvest_marks`): frases em formato de preferência de
observações `user-prompt` (nunca saída de ferramentas), as páginas `_rules/`,
`decisions/`, `gotchas/` e `procedures/` do projeto, e seus sinais de stack, em
`profile_candidates`. Em seguida, ele **converge** por escopo de perfil: candidatos agrupam-se
por sobreposição de tokens de tópico entre projetos; um grupo definido como geral ou visto em
`min_projects` projetos torna-se uma página `profile/<category>/<slug>.md` escrita
através de `Wiki::write_page`, vencendo a declaração mais recente (a versão substituída
permanece na cadeia de substituição). Páginas que ele não escreveu, ou cujo corpo mudou
desde que foram escritas, nunca são reescritas; `profile_entry_ledger` mantém uma entrada
removida fora até que cheguem evidências mais recentes. Com um provedor de LLM e
`[profile] llm`, a classificação e a mesclagem por entrada usam chamadas estruturadas com
esquema JSON e recorrem ao caminho sem LLM em qualquer erro. Um perfil privado
converge apenas os candidatos próprios de seu operador em projetos que ele pode
ler; um perfil compartilhado em um servidor multiusuário ignora projetos restritos.
`ai-memory profile apply` lê `GET /admin/profile/apply` (a seleção do resumo,
limitada a `apply_max_lines`, vazia para um projeto com `consume = false`)
e a grava entre os delimitadores `<!-- ai-memory:profile:start/end -->` no arquivo
de regras do repositório através de `apply_atomic` (tmp + rename + fsync, backup sob
`<data_dir>/backups/profile-apply/`); este é o único caminho de perfil que
grava fora da wiki, e somente quando o usuário o executa.

**Ciclo opcional de workstreams gerenciados:** `ai-memory run` abre uma concessão para o
workstream do repositório/worktree atual, resolve um harness explícito ou o
harness local/vinculado utilizável mais novo, cria ou retoma a sessão nativa daquele
harness e marca chamadas de ciclo de vida com um id de execução de escopo de invocação.
SessionStart injeta um intervalo limitado de eventos não vistos; o Crush o recebe através de
um caminho temporário aceito de contexto global porque não possui SessionStart. O host
também fixa a identidade da sessão mais recente de outro harness quando a inicialização for uma
troca real de harness. O cliente revalida essa identidade contra o checkout e
materializa um dossiê privado e temporário de histórico visível para o filho através de
`AI_MEMORY_PREVIOUS_SESSION_HISTORY`; seu caminho e corpo permanecem locais no cliente e
o arquivo é removido juntamente com o processo filho. O host importa a cauda da transcrição nativa
e um checkpoint Git quando o processo filho é encerrado.
Todo pacote injetado inicia com um marcador de origem versionado. O normalizador
de transcrição do Claude exclui um pacote marcado caso o Claude o persista e o leia de
volta, impedindo que o histórico entregue reingresse recursivamente no registro contábil.
Um handoff explicitamente pendente é considerado antes do intervalo de eventos gerenciados.
Por padrão, suas reivindicações de entrega de uso único compartilham uma transação de escrita após
a resposta de inicialização completa ter sido montada. Com a configuração global do servidor
`[handoff].claim_on_session_start = false`, SessionStart emite, em vez disso, um aviso
apenas de metadados nomeando o id exato do handoff e deixa a linha aberta para
um `memory_handoff_accept` explícito; o intervalo de eventos gerenciados ainda é reivindicado.
A justificativa do modo de oferta não destrutiva está registrada em
[`design-handoff-offer-not-claim.md`](design-handoff-offer-not-claim.md).
Com a configuração global `[handoff].create_on_session_end = false`, o término da sessão grava
a página de resumo e enfileira a consolidação, mas pula a criação de um bastão automático,
e um checkpoint de turno do OpenCode ou Antigravity atualiza a página
sem nenhum bastão (#1043). Um cliente de hook cujo ambiente defina
`AI_MEMORY_AUTO_HANDOFF=off` encaminha `auto_handoff=0`, o que ignora o bastão automático
daquela sessão da mesma forma, mantendo todas as outras sessões no padrão do
servidor (#1951). Handoffs manuais têm precedência;
caso contrário, o handoff automático elegível pelo diretório atual mais recente é entregue, e essa
mesma transação expira handoffs automáticos elegíveis mais antigos enquanto preserva trabalhos
manuais e de diretórios irmãos. A inserção também expira handoffs automáticos abertos anteriores
do diretório de trabalho exato, limitando SessionEnds repetidos antes que qualquer receptor
seja iniciado.
Uma sessão que ainda tem um handoff aberto próprio de `memory_handoff_begin`
termina sem baton automático, e seus checkpoints de turno não gravam nenhum;
gravar esse handoff explícito também expira os batons automáticos abertos da própria sessão
(#2012). O bloco de inicialização de um handoff com sessão de origem nomeia a página
`sessions/<id>.md` dela, que a consolidação opcional em SessionEnd reescreve
depois que o baton foi gravado.
`GET /recap?session_id=` responde a um prompt que começa com `reload context`
(Claude Code e Codex no envelope `UserPromptSubmit`, Grok no próximo
`PostToolUse`, OpenCode através de seus hooks de sistema/contexto). Ele é somente leitura e
ancorado na linha da sessão em vez de um diretório: autorização de leitura do projeto e o filtro
de proprietário do handoff o controlam, e ele renderiza os prompts capturados e a última
resposta entre as duas observações de compactação mais recentes da sessão (`pre-compact`,
ou `post-compaction` do Devin) mais os handoffs abertos do projeto, listados sem
reivindicação. `mode=anchor` retorna a âncora pós-compactação, sob os mesmos
portões: os primeiros prompts do operador da sessão e os mais recentes, lidos
através de cada compactação, em um orçamento pequeno (padrão 2.000 caracteres, restrito a
800-4.000) dentro da cerca de histórico não confiável, com um ponteiro para
`memory_read_session_observations`. Essa ferramenta aceita `session_id: "current"`
apenas quando o cliente do chamador encaminha seu id de sessão de hook (o cabeçalho bridge
ou `_meta` do OpenCode 2), recusando nos outros casos.
O cliente de hook busca a âncora quando `SessionStart` chega com origem
`compact` (Claude Code, Codex, Factory Droid) e a emite através do envelope `SessionStart`
daquele agente em vez do pacote de inicialização, de modo que uma compactação não
reivindica handoff. Uma inicialização `compact` de qualquer outro harness, e dos hooks de
contingência em shell e PowerShell, também pula a reivindicação, mas não recebe âncora.
Os plugins gerados do OpenCode V1/V2 a armam no evento de compactação e a injetam através de seus hooks de sistema/contexto até a
próxima compactação. Todos os outros harnesses dependem das superfícies de instrução
(`SNIPPET_BODY`, `COMPACT_SNIPPET_BODY`, o núcleo de `MEMORY_INSTRUCTIONS` e a
skill de handoff), que orientam o agente a se reancorar após uma compactação e a
confiar nos prompts gravados acima do resumo (#1983).
O ai-memory abre armazenamentos nativos como somente leitura. Segmentos JSONL brutos sanitizados são
imutáveis, enquanto o SQLite fornece sequências monotônicas, FTS, cursores nativos
de origem/entrega e estado de repetição idempotente. Um caminho de busca em todo o registro
`workstream-search` complementa
o pacote de inicialização limitado. Um workstream interativo vazio pode adotar uma sessão nativa
correspondente ao checkout uma vez. A elegibilidade vem do estado autoritativo do
registro/sessão: após qualquer harness estabelecer o workstream, um harness recém-ingressado
inicia do zero e recebe histórico portátil em vez de adotar histórico nativo antigo e não
relacionado. Falhas tratadas do inicializador cancelam sua concessão; repetições normais de reabertura
tentam novamente conflitos breves de finalização, enquanto uma terminação abrupta de processo
permanece limitada pela expiração da concessão renovável. Uma recuperação explícita com
`--force-unlock` expira e substitui uma concessão ativa selecionada na mesma transação
de escrita, mas somente quando sua atribuição de operador durável for igual à atribuição da
nova execução; o rótulo informativo de concessão `host:pid` nunca é uma chave de
autorização. A execução antiga não pode mais emitir heartbeat ou finalizar normalmente.
`POST /workstream/runs/{run_id}/recover/finish` é o caminho mais restrito para incidentes: ele
reutiliza as verificações normais de proprietário e escrita no projeto e admite uma execução expirada
apenas quando nenhuma execução mais recente naquele workstream a tiver substituído. Uma repetição concluída
valida somente a identidade durável de transcrição/nativa/cursor/saída e não regenera um
checkpoint do repositório. Sessões retomadas degradadas persistem um cursor de origem nativa
validado antes da inicialização, e a exportação de recuperação limitada lê apenas o delta
em um worker bloqueante. Consulte [Workstreams gerenciados
entre harnesses](managed-workstreams.md).

## Vocabulário de eventos de hooks

O vocabulário central de observações é um conjunto fechado de eventos do ciclo de vida
do agente. Os hook bridges podem aceitar aliases específicos de cada cliente, mas o armazenamento
os normaliza para exatamente um destes valores de `ObservationKind`:

| Tipo armazenado | Semântica |
|---|---|
| `session-start` | A sessão do agente começou; cwd, modelo e identidade da sessão capturados. |
| `user-prompt` | O usuário enviou o texto de um prompt ao agente. |
| `pre-tool-use` | O agente está prestes a chamar uma ferramenta. |
| `post-tool-use` | O agente concluiu uma chamada de ferramenta. |
| `pre-compact` | O agente está prestes a compactar ou comprimir o seu contexto. |
| `post-compaction` | O agente compactou o contexto e forneceu um resumo ou checkpoint posterior. |
| `notification` | O agente emitiu um evento no estilo de notificação. |
| `stop` | O agente concluiu um turno interativo ou parou naturalmente. |
| `session-end` | A sessão do agente terminou; o caminho de resumo/handoff pode ser executado. |
| `other` | Evento de hook desconhecido ou sem suporte. |

Os clientes nativos de hook corrigem um caso entre projetos que de outra forma passaria despercebido, antes
de enfileirar o evento: se a localização absoluta de uma chamada de ferramenta reconhecida (alvos de ferramentas de arquivo,
o `path` de uma ferramenta de busca/listagem ou o `workdir` de um comando de shell) estiver dentro de outra
fronteira de repositório/marcador, esse destino passa a ser o cwd do evento.
A política de captura do destino, o marcador de lista de permissão, o perfil de servidor e o escopo
viajam, portanto, juntos; o redirecionamento é recusado quando a própria política de captura do cwd do harness
não manteria o evento, de modo que as exclusões da origem continuam
valendo. Alvos relativos, de projetos mistos, sem suporte e fora de projetos mantêm o
cwd do harness. A identidade da sessão e a propriedade da sessão compilada não mudam; apenas
as observações brutas são atribuídas ao projeto tocado (#932; #1199:
o Hermes informa o cwd do seu processo e localiza cada ferramenta por `workdir`/`path`).
Para o Hermes, o hook nativo primeiro substitui o cwd desse processo por
`TERMINAL_CWD`, a variável por meio da qual o Hermes exporta `terminal.cwd`, quando ela
nomeia um diretório absoluto existente; o cwd do processo de um gateway é onde o
gateway foi iniciado. Um gateway multiplexador exporta apenas o `terminal.cwd` do perfil
padrão, de modo que, em um gateway (`_HERMES_GATEWAY=1`), a variável se aplica somente
a eventos cujo `profile` seja `default`. Um evento de gateway de outro perfil
é roteado pelo Hermes home desse perfil (`<HERMES_HOME or ~/.hermes>/profiles/<name>`,
com o nome verificado antes contra o padrão de id de perfil do Hermes) quando esse diretório
contiver seu próprio `.ai-memory.toml`; caso contrário, mantém o cwd do processo. Todo
evento do Hermes é roteado a partir daí, e os eventos de ferramenta ainda podem mudar pelas suas próprias
localizações.

O ciclo de vida do Hermes mapeia por significado: `on_session_start` é `session-start` e
registra apenas o início (o Hermes ignora seu stdout), `pre_llm_call` é
`user-prompt` com o texto de `extra.user_message` e nunca o histórico
da conversa, `on_session_end` é `stop` porque o Hermes o aciona após cada turno,
e `on_session_finalize` é `session-end` somente para a sessão finalizada. O
handoff é verificado a cada `pre_llm_call` de sessão primária e exibido como
`{"context": ...}`, o único formato de stdout que o Hermes adiciona a um turno. Isso permite que
uma sessão nativa retomada do Hermes receba um handoff produzido por outro harness;
`extra.is_first_turn` controla apenas a composição de resumo/perfil de inicialização. Um
filho de `delegate_task` (`extra.platform` `subagent`; o
Hermes nunca envia seu `parent_session_id`) também inicia com histórico vazio,
portanto seus eventos de ciclo de vida são descartados antes de qualquer spool ou requisição de handoff.

O `SessionStart` do Antigravity mapeia para `session-start`. Seu hook `PreInvocation`
é acionado antes de cada chamada de modelo, logo a ponte atua somente no payload documentado
`invocationNum = 0`, o início de um turno do usuário: ele registra os prompts do
turno a partir da transcrição da conversa (nenhum payload do Antigravity carrega um prompt),
e em compilações sem `SessionStart` a chamada do primeiro turno envia
`session-start` em seu lugar, uma vez por conversa. Invocações posteriores são
ignoradas antes de efeitos colaterais de spool ou rede. Um `Stop` que encerrou o turno
sem nada mais em execução é um checkpoint de turno, assim como no OpenCode 2.

Eventos desconhecidos **não** expandem o enum e, por padrão, não deixam
metadados de evento de origem no armazenamento; eles colapsam para `other`. Integrações
de terceiros que necessitam de seu próprio vocabulário podem optar por enviar
`extension=<namespace>` em `/hook`. Com um namespace de extensão válido,
o ai-memory armazena um `source_event=<name>` explícito quando fornecido, ou a string
desconhecida de `event` quando `source_event` for omitido. O par armazenado são
metadados anuláveis de observação; `kind` permanece canônico. Trata-se de um ponto
de extensão, não de um sistema de plugins em tempo de execução: processadores externos devem usar
as APIs HTTP/MCP existentes e não podem ignorar o sanitizador, a contrapressão de hooks
ou o ator gravador único do SQLite.

Um produtor de ciclo de vida externo pode definir `AI_MEMORY_CAPTURE_OWNER` no processo
do harness para suprimir sua captura nativa instalada enquanto retém a entrega compatível
de handoff/briefing e MCP. O produtor usa `extension`/`source_event`
para proveniência e valores estáveis e com namespace de `ingest_key` para repetições. Consulte o
[contrato de captura externa](external-lifecycle.md) para empacotamento em lote, identidade e
os limites deste modo cooperativo com escopo de processo.

Resumos de sessão expõem o estado da geração de consolidação mais recente e a contagem
de tentativas, selecionados na mesma consulta com escopo e filtrada por proprietário. Leituras de sessão
via HTTP e `memory_read_session_observations` via MCP compartilham esse tipo de resumo; nenhuma
das respostas inclui diagnósticos do provedor.

Ferramentas podem usar o MCP diretamente para gravar páginas, consultar conhecimento e reivindicar handoffs.
Com `--enable-api` (ou `--enable-web`, que o implica), páginas incrementais `recent`
usam uma consulta limitada `(updated_at, path)` no escopo autorizado. O modo somente API
não monta rotas de navegador. Consulte [memória programática](programmatic-memory.md).

`GET /identity` é uma rota autenticada por máquina montada independentemente da
interface web. O `ai-memory doctor` a utiliza para exibir a identidade do chamador e a propriedade de captura;
suas contagens por agente sinalizam sessões com múltiplas fontes de captura (eventos nativos
ou extensões distintas, incluindo preenchimento retroativo).
A contagem de fontes mistas é calculada na mesma consulta SQL com escopo e filtrada por proprietário.
`GET /admin/project-coordinate` é um diagnóstico somente leitura de root/admin sobre os
mesmos índices limitados exatos/canônicos/legados usados pela resolução de escopo. Ele retorna
status tipado e metadados de colisão, mas nunca cria, reivindica, renomeia, audita
ou grava um manifesto de escopo. Um resultado `ambiguous` pode incluir o UUID/nome atual do candidato
preferido baseado em identidade como contexto; isso não é resolução única
e nunca torna a requisição elegível para renomeação. O endpoint omite o valor de identidade armazenado e relata
apenas sua fonte/estilo seguro e nomes de candidatos derivados. O `doctor` adiciona uma classe
segura de status/fonte de marcador e evidências de repositório sem credenciais a partir de uma inspeção
local compartilhada antes de renderizar essa resposta. Se a resolução de compatibilidade for
ambígua, linhas de sessão locais permanecem visíveis enquanto as contagens capturadas são marcadas como
indisponíveis em vez de renderizadas
como zero.

Corpos de ciclo de vida possuem limites de conteúdo independentes do limite de 10 MiB
da requisição HTTP. Prompts de usuário e resumos pós-compactação são limitados com segurança UTF-8 em
16 KiB; trechos de notificação e ferramentas são limitados em 2 KB. Comandos
nativos `ai-memory hook` aplicam o limite específico de evento antes do spool local
e transporte, e o servidor o repete ao analisar cada requisição para que clientes diretos
e mais antigos não possam ignorá-lo. A fronteira tipada do sanitizador aplica então uma barreira
de 16 KiB a todo corpo de observação durável após a redação. O trecho de assistente/Stop do
Claude Code, controlado separadamente, permanece limitado a 2 KB.

Um `occurred_at` RFC 3339 opcional no corpo do hook permite que o cliente forneça o horário
original do próprio evento. Atualmente apenas o `ai-memory backfill` faz isso,
reproduzindo os timestamps por evento de uma transcrição, de modo que sessões/observações importadas são
marcadas com o momento em que realmente aconteceram em vez da hora da importação. Ele é lido
apenas do nível superior do corpo (ao contrário da busca aninhada em `payload`/`event`/
`properties`/`info`/`path` que outros campos de hook utilizam), portanto um payload real de harness
que porventura carregue uma chave `occurred_at` em sua própria estrutura nunca é confundido
com esse campo. Trata-se de metadados numéricos, não texto, logo nunca passa pelo sanitizador
(fora da fronteira do invariante #6); ainda é uma entrada controlada pelo cliente via `/hook`, de modo que
`HookEnvelope::occurred_at_micros` o limita (deve ser > 0 e não mais que
cinco minutos à frente do relógio do servidor) antes de confiar nele. Qualquer outra coisa
(ausente, impossível de analisar ou fora dos limites) resolve para `None`, o que o armazenamento
trata como "agora" em vez de erro, mantendo os hooks como envio sem confirmação (invariante #5).
O `ended_at` de uma sessão preenchida retroativamente pode, portanto, ficar bem no passado, o que
pode empurrá-la para baixo da marca d'água de autoaperfeiçoamento e da âncora da passagem de experiência
(ambas indexadas por `ended_at`), e uma janela de retenção medida a partir do tempo da própria
observação pode tornar uma observação antiga preenchida retroativamente elegível para poda
imediata em vez de apenas após envelhecer
no local.

## Arquitetura de armazenamento

**Duas camadas, uma fonte da verdade.**

* `<data_dir>/wiki/`: fonte da verdade em markdown. Pertence a um repositório `git2`
  para que cada passada de consolidação e cada fim de sessão produza um
  commit durável. Editável à mão no Obsidian / vim; o watcher
  reconcilia as edições externas. Um checkpoint nunca inclui um registro de eventos de hook nem
  uma gravação ainda em andamento (um arquivo temporário `.ai-memory-tmp.` ou uma
  página reservada `.ai-memory-delete.`).
* `<data_dir>/db/memory.sqlite`: índice derivado. Modo WAL. Um
  ator gravador é dono da `Connection` de escrita; as leituras passam por um
  pool somente leitura clonável, limitado a quatro chamadas e conexões em andamento.
  As chamadas seguintes aguardam uma permissão de forma assíncrona, em vez de criar threads
  bloqueantes, e o fechamento do pool as desperta com `ReaderClosed`.
* `<data_dir>/raw/`: segmentos JSONL imutáveis e sanitizados de workstreams gerenciados.
  A recuperação legada de fallback bruto pesquisa a tabela durável `observations` via
  FTS5; o JSON HookEnvelope do ciclo de vida não é um arquivo completo de transcrição.
* `<data_dir>/logs/`: saída diária rotativa de `tracing`.
* `<data_dir>/models/`: o modelo de embedding local (`all-MiniLM-L6-v2`),
  baixado no primeiro uso e que pode ser removido à mão em instalações offline.
* `<data_dir>/client-projects.json`: vínculos de checkout privados e locais do cliente para
  `ai-memory show`, indexados por identidade de servidor sem credenciais mais workspace e
  projeto. Não faz parte da fonte da verdade do SQLite/wiki, e nenhuma API do servidor
  expõe caminhos do host.

**Esquema (cabeça atual):**

| Tabela | O que faz |
|---|---|
| `workspaces`, `projects` | Topo da coordenada de identidade de 3 tuplas. `projects.identity` / `identity_source` (V70, #708) mantêm a identidade de repositório pela qual um projeto é roteado (um `identity` de marcador explícito ou um remoto git normalizado, resolvido no lado do cliente), exclusivo por espaço de trabalho quando definido; vazio até que uma captura reivindique o projeto. Clientes da Fase 5 enviam explicitamente `IdentityStyle::Path` (#1033) para projetos com suporte remoto, nomeando-os a partir do caminho completo do repositório sem o host (`acme-api`), e enviam `host_path` explícito para a opção anterior à v3. A omissão no lado do servidor permanece `HostPath` porque clientes antigos omitiam esse padrão histórico; nenhum novo campo de transmissão é necessário. A criação ou promoção autorizada de captura usa esse nome canônico apenas quando nenhuma identidade com host diferente o reivindicar; caso contrário, a resolução recorre ao nome com host/compatível legado existente. V76 armazena chaves canônicas e v2-basename indexadas derivadas dessa identidade completa: leituras não criam/não renomeiam, enquanto uma escrita explícita autorizada através do nome canônico promove apenas `projects.name` na mesma transação de gravador de resolver-ou-criar e preserva o UUID. O `_meta.md` do projeto carrega a identidade/fonte completa opcional mais chaves derivadas; um reíndice limpo as valida e reconstrói a mesma pesquisa de UUID/nome/coordenada, enquanto manifestos mais antigos sem identidade permanecem válidos. Chaves ambíguas entre forges ou de múltiplos candidatos falham fechadas. Um marcador local pode, adicionalmente, enviar até 16 nomes anteriores validados como um valor de consulta de array JSON temporário `aliases`: a pesquisa permanece dentro do espaço de trabalho selecionado, exige que a identidade remota git completa com host do checkout seja igual à linha armazenada, retorna leituras sem renomeação e permite que a captura execute apenas a mesma promoção canônica autorizada que preserva o UUID. Apelidos não são armazenados e permanecem distintos no cache de roteamento de hooks. O marcador exato no home do operador pode, adicionalmente, definir blocos limitados `[routes.identity."host/path"]` e `[routes.path."absolute-or-tilde-path"]`: uma correspondência remota exata com host vence a correspondência de caminho lexical mais longa por componentes, qualquer marcador de configurações locais alcançável fora do home vence ambos, mapas de rota malformados ou ambíguos falham fechados, e campos selecionados reutilizam a transmissão de marcador existente sem alteração de servidor/esquema. `AI_MEMORY_HOME` é autoritativo para chamadores nativos; clientes portáteis usam `HOME`/`USERPROFILE`, e o Docker compara seu diretório de trabalho do host encaminhado. Consulte `docs/marker-file.md#repository-identity`. `projects.access_mode` (V68; padrão `open` / `restricted`) decide se qualquer usuário autenticado ou apenas o root, o criador (`projects.created_by`, V69) e detentores de concessões (`project_grants`, V68) o acessam, decidido por `ai_memory_store::authorize_project` (consulte `docs/users.md#per-project-access`). `projects.profile_contribute` / `profile_consume` (V74) registram as opções de exclusão de `[profile]` do marcador, encaminhadas no início da sessão; `profile_candidates`, `profile_harvest_marks` e `profile_entry_ledger` (V75) mantêm as evidências da passagem de perfil, a posição de leitura por projeto e as entradas que ele escreveu. Projetos reservados: `scratch`, `_global` (preferências compartilhadas; mantém o perfil de usuário único sob `profile/`), `_profile` (um perfil de espaço de trabalho) e `_profile.<user id>` (um perfil privado, sempre `restricted`); a captura nunca é atribuída a nenhum deles (consulte `docs/cross-project-profile.md`). |
| `pages` | Páginas versionadas da wiki com cadeia de `is_latest` + `supersedes`. Colunas M8: `last_accessed_at`, `access_count` e marcador de lápide exclusivo de decaimento `superseded_at`. Colunas M9: `embedding_provider`, `embedding_model`, `embedding_dim`. V36: `expires_at` (TTL do frontmatter). V37: `salience` (NULL = `salience_default`; derivado de `page_feedback`). |
| `pages_fts` | Tabela virtual FTS5 sobre `(title, body, path_search)` mais as grafias `title_parts` / `body_parts`, que transformam `-`/`_` em espaços (V100, #2013), de modo que um token com hífen e cada uma de suas palavras correspondam; sincronizada automaticamente por gatilhos. |
| `pages_cjk_fts` | Índice FTS5 derivado de unigrama/bigrama sobre sequências CJK no título, corpo e caminho da página. O gravador o atualiza na transação da página; a inicialização repara linhas ausentes após um upgrade. Consultado apenas quando a consulta limitada contém Han, Kana ou Hangul. |
| `sessions`, `observations` | Projeções sanitizadas e limitadas de hooks de ciclo de vida. `sessions.ended_observation_count` é a marca d'água de geração estável para elegibilidade de reencerramento de sessão retomada; relógios de parede não são usados para essa decisão. São uma trilha de auditoria operacional, não uma transcrição nativa completa. |
| `session_consolidation_jobs` | Fila durável e idempotente por geração de observações para consolidação automática por LLM. Trabalhos de checkpoint PreCompact / PostCompaction são enfileirados independentemente de a sessão ter terminado (uma sessão retomada mantém `ended_at`); trabalhos opcionais de SessionEnd necessitam de uma sessão *encerrada*, e as gerações de observação ordenam os dois. Um worker de servidor limitado arrola trabalhos, repete falhas do provedor com backoff e recupera concessões expiradas após reinicialização. O worker inicia sempre que um consolidador estiver configurado; o enfileiramento de SessionEnd permanece atrás de `AI_MEMORY_CONSOLIDATE_ON_SESSION_END`. Um `memory_consolidate` manual grava sua página fora de banda e depois reconcilia esta tabela (mudando uma linha `failed`/`pending`/`superseded` da sessão para `completed`, nunca tocando em uma concessão `running` ativa), para que o operador não veja um trabalho `failed` para uma sessão que está, de fato, consolidada. |
| `observations_fts` | Tabela virtual FTS5 sobre `(title, body)` de observações brutas, com as mesmas grafias `title_parts` / `body_parts` (V100, #2013). Alimenta a contingência limitada de `raw_hits` e, através de `[retrieval] observation_stream` (ativo por padrão), o fluxo RRF de evidências de observação que vota em páginas de sessão. |
| `observations_cjk_fts` | Complemento derivado de unigrama/bigrama para texto de observação CJK sem espaços. Segue os mesmos limites de escrita transacional, escopo, visibilidade de proprietário, contingência bruta, linha do tempo e votos de sessão que `observations_fts`. |
| `workstreams`, `managed_runs`, `workstream_native_sessions` | Estado opcional de concessões mais cursores de entrega e origem nativa por harness para `ai-memory run`. |
| `workstream_events`, `workstream_events_fts` | Eventos de transcrição visíveis normalizados somente de anexo e busca textual completa; lotes de origem sanitizados e imutáveis também residem sob `raw/workstreams/`. |
| `links` | Referências cruzadas de wikilinks / markdown. `to_page_id` (um PageId global) é anulável para links diretos não resolvidos. `to_workspace` / `to_project` carregam escopo entre projetos (NULL = o próprio projeto da página de origem). |
| `handoffs` | Registros tipados de handoff entre agentes (aberto / aceito / expirado). O `manual` da V101 marca um handoff de `memory_handoff_begin`, que agora pode carregar a própria sessão do chamador (#2014); uma linha com `manual = 1` ou sem `from_session_id` é manual: abrange o projeto inteiro na correspondência de cwd, tem preferência na entrega e nunca é renovada nem varrida como baton automático. |
| `page_embeddings` | Linhas de vetores opcionais para páginas mais recentes, com `(provider, model, dim)` desnormalizados para que a busca híbrida possa ignorar vetores obsoletos após uma mudança de configuração de embedding e relatar diagnósticos de embeddings ausentes. |
| `page_feedback` | Sinais de `memory_feedback` somente de anexo (`helpful` / `not_helpful` / `stale` / `wrong`) indexados pela *versão* da página, com um motivo sanitizado opcional e `salience_after`. Fonte da verdade para o `pages.salience` derivado; a passagem de lint lê linhas pendentes de stale/wrong combinadas com `is_latest = 1`, de modo que uma reescrita encerra a constatação. |
| `query_page_signals` | Associações opcionais de classificação com escopo de consulta (`[retrieval] query_signal_weight > 0`) (V98): uma linha por `(workspace, project, normalized-query hash, page version)` com o texto sanitizado da consulta (≤256 caracteres), um `signal_count` de sessões distintas e a última sessão de escrita. Gravado apenas para chamadores cientes de sessão e com autorização de escrita após leitura ou `helpful` do resultado da consulta anterior; `not_helpful` exclui a linha, `purge_session` exclui linhas que a sessão gravou por último e as FKs de projeto e página executam cascata. |
| `page_access` | Uma linha por página mais recente e identidade qualificada de operador. Fornece o termo opcional de retenção por amplitude de acesso sem alterar o contador de acesso compartilhado existente. |
| `page_evidence` | Registro V63 somente de acréscimo do que produziu ou reafirmou cada versão de página: a consolidação cita a `session` em que foi executada, gravada na transação de upsert da página e excluída em cascata no expurgo. Alimenta a **`confidence` de força de crença** em tempo de leitura, sem LLM (`ai_memory_store::belief`, B1 / `docs/design-hindsight-borrowings.md` P2): uma função limitada em `[0.0, 0.95]` de sessões de suporte distintas (amplitude, não contagem bruta), recência do avistamento mais novo e contagem ativa de `contradicts`. Exposta como `confidence` + `evidence_count` em `memory_query(explain=true)`, como `evidence_rows` em `memory_status`, e usada para ordenar o resumo opcional `settled_first`, todos **inertes na classificação**. Ela se integra a `PageAuthority` apenas atrás de `[retrieval] belief_authority_weight` (**padrão `0.0`/OFF, restrito por R2**), como um fator limitado dentro da contenção `[0.55, 1.50]`; uma substituição sempre vence independentemente das evidências (uma versão substituída nunca é impulsionada). |
| `agent_messages` | Caixa de entrada/fila de mensagens entre projetos V64 (`docs/agent-messaging.md`), estendida pela V90 com evidência/natureza epistêmica opcional. Mensagem direcionada e de reivindicação única de uma coordenada de remetente para uma coordenada de destinatário; `pending`→`claimed` (retirada exatamente uma vez, o compare-and-set de handoff) ou `pending`→`cancelled` (o remetente retrata). A única tabela que cruza o isolamento por projeto, logo as leituras são indexadas pela coordenada do destinatário (inbox) ou do remetente (outbox); `from_owner_user`/`claimed_by_user` são apenas atribuição, nunca um filtro de leitura. A profundidade da caixa de entrada do destinatário é limitada. `ON DELETE CASCADE` em ambos os pares de coordenadas. |
| `pages.compacted_at` | Marcador anulável V65 de rebaixamento de nível A2 (`docs/design-memory-aging.md` §A2). Definido quando a varredura de esquecimento compacta de forma extrativa uma página episódica fria (`[decay] compact_cold_episodic` opcional) em vez de despejá-la; derivado no ponto único de controle de upsert da página a partir de um espelho de frontmatter `compacted: true`, de forma que o marcador e o corpo compactado caiam em uma única transação. Aditivo `ADD COLUMN`, sem preenchimento retroativo; a varredura o preenche de forma preguiçosa. A varredura e o curador ignoram uma página marcada para que ela nunca seja recompactada, redesperjada ou rerrelatada como fria. Reversível: o corpo completo pré-compactação permanece no git + na cadeia de substituição. |
| `session_usage` | Linha única V91 por sessão: tokens cumulativos de entrada/saída/escrita de cache/leitura de cache que a própria transcrição do harness relatou em `session-end`, mais o nome do último modelo visto. Gravado no lado do cliente (o módulo `session_usage` do `ai-memory-cli` lê a transcrição; `hook.rs` a insere no corpo de fim de sessão como `_ai_memory_usage`; o gravador faz upsert com `MAX(existing, reported)` por coluna). Executa cascata com sua sessão no expurgo/exclusão. Exibido na estatística de Tokens e na tabela de Sessões da página do projeto no /web; `None` (não zero) para uma sessão cujo harness nunca relatou uso. |
| `token_calls` | Identidades de respostas nativas V92 por sessão com contadores de entrada, saída e cache livres de conteúdo. Hooks ativos mesclam respostas parciais/repetidas com máximos por coluna em uma transação de gravador, atualizando em seguida o uso cumulativo da sessão. Leituras de projetos autorizam o projeto e aplicam a propriedade da sessão antes de contar ou classificar. A V92 também converte totais históricos de entrada do Codex em entrada nova, excluindo leituras de cache. |
| `agent_messages.to_agent` | Destinatário de harness registrado opcional V93; NULL mantém a caixa de entrada compartilhada do projeto. Ambas as proteções de reivindicação impõem entrega endereçada. |
| `token_calls` context e `token_call_tools` | Coordenadas anuláveis V94 de turno/resposta nativa e tempo de resposta, mais referências limitadas de ferramentas indexadas por sessão/resposta. Uma transação de gravador persiste contadores, proveniência e totais de sessão; ferramentas são excluídas em cascata com sua resposta. Contexto ausente permanece desconhecido. Filtros no lado do servidor de UTC/modelo/harness/sessão/turno/ferramenta executam após verificações de escopo/proprietário e antes da paginação. |
| `agent_messages.state` `editing` | V95 permite que um remetente mantenha uma mensagem pendente fora da fila enquanto a edita. O caminho de reivindicação seleciona apenas `pending`, portanto uma mensagem retida não pode ser retirada por ordem ou por id. A linha mantém seu id e `created_at`, e `edited_at` registra a última alteração de texto. `ops::begin_message_edit` e `finish_message_edit` são `UPDATE`s únicos com escopo na coordenada do destinatário e no estado esperado, de modo que mensagens coletadas, canceladas ou externas não são tocadas. Salvar passa pelo sanitizador e pela admissão de `message_send`. |
| `observations.file_path` | Caminho relativo ao checkout anulável V99 do único arquivo que um PostToolUse de ferramenta de arquivo retido nomeou, definido apenas com `[ingest] index_file_paths` ativo (desativado por padrão) e apenas a partir do campo `_ai_memory_file` do cliente de hook nativo e das integrações TypeScript geradas. `Sanitized::<NewObservation>::new` limpa caminhos enraizados, com `..`, excessivamente longos ou alterados por limpeza. O índice parcial `idx_observations_file_path` cobre `(workspace_id, project_id, file_path, created_at)`. A coluna viaja com sua linha através de poda, expurgo, movimentação e mesclagem. `ReaderPool::observations_for_file` lê um projeto, aplica a regra de proprietário da sessão antes de seu limite e não possui caminho `global` ou `scopes` ([design](design-file-path-index.md)). |
| `client_activity` | Contadores de chamadas de ferramentas MCP em todo o servidor divididos em leituras/escritas e agrupados por dia UTC. O ponto de afunilamento de requisições MCP descarrega chamadas em buffer em um intervalo de segundo plano de um minuto; lotes com falha tentam novamente a partir de memória limitada. Cada dia armazena no máximo 128 rótulos sanitizados de clientes mais `other`, para que um `clientInfo.name` não confiável não possa criar linhas proporcionais ao tráfego. |
| `auto_improve_proposals` | Edições preparadas de aprendizado e manutenção com snapshots imutáveis de destino e eventos de decisão somente de anexo. A exclusividade de destino pendente tem escopo pela identidade de preparação qualificada; propostas não atribuídas retêm o bucket histórico compartilhado. |
| `entities`, `entity_page_links` | Índice de substantivos V38 derivado do frontmatter canônico. Nomes são normalizados e exclusivos por projeto; links apontam para versões imutáveis de páginas enquanto a recuperação filtra para a versão mais recente. Gatilhos de pareamento de escopo evitam links entre projetos. Alimenta o quarto fluxo de recuperação RRF. |
| `audit_log` | Toda mutação, endereçável por `at DESC`. |

**Níveis de memória (política M8):**

| Nível | Tempo de vida | Decaimento |
|---|---|---|
| De trabalho | Apenas sessão atual | Queda permanente no fim da sessão (mantida em `observations` para análise forense) |
| Episódica | 30d quente → 180d fria → despejo (ou rebaixamento, A2 opcional) | `salience · exp(−λΔt) + σ · log(1+access_count) · exp(−μ · days_since_access) · (1 + breadth_weight · ln(1 + max(distinct_actors−1, 0)))` |
| Semântica | Indefinido | Nenhum - apenas substituível via reescrita M7 de LLM |
| Procedural | Indefinido | Decaimento por frequência se não for reobservada |

`λ` é o escalar `[decay] lambda` por padrão, mas pode ser definido por nível através de
`[decay.half_life_days]` (uma meia-vida em dias por nível, convertida em
`λ = ln(2)/days`); um nível não definido usa o escalar, logo o padrão reproduz
as pontuações de λ único atuais com exatidão.

**Rebaixamento de nível extrativo (A2, opcional).** Com `[decay] compact_cold_episodic =
true`, a varredura executa uma passagem de compactação *antes* da passagem de decaimento-despejo: uma página
episódica fria que ainda não foi compactada é reescrita através da camada da wiki
para manter seu L0 `abstract:`, um resumo de primeiro parágrafo L1 e um conjunto de tokens retidos L2 minerados
por regex (caminhos, URLs, blocos de código, códigos de erro, identificadores), descartando a
prosa em vez de receber uma lápide. A reescrita substitui a versão anterior,
de forma que o corpo completo permanece acessível (git + cadeia de substituição; `restore-page`
o recupera). A reescrita em lote é condicional a cada versão de origem ainda
ser a mais recente, verificada dentro da transação de escrita; qualquer edição simultânea rejeita
o lote inteiro sem substituir a edição ou compactar parcialmente seus pares.
O marcador `V65` `pages.compacted_at` torna uma página compactada
terminal para a passagem de decaimento (nunca recompactada, nunca redespejada, nunca
rerrelatada como fria). Sem LLM, desligado por padrão; a prova de não regressão
de recuperação R2 controla qualquer futura ativação padrão.

**Passagem de "sonho" por LLM (B2/B3/B4, LLM opcional, DESLIGADO por padrão, restrito antes
de vir ativo por padrão).** Onde A3 colapsa clusters frios quase duplicados de forma *extrativa*
(sem LLM, união de tokens mantidos), a passagem de sonho
(`ai-memory-consolidate::dream::run_dream_pass`) entrega cada cluster frio ao
provedor configurado para ser reescrito em UMA página coerente, uma mesclagem em prosa
coerente que a extração não faz. Ela reutiliza a matemática de clusterização de A3 (`adaptive_eps` /
`dbscan`) sobre o *mesmo* conjunto frio limitado que a varredura de esquecimento materializa
(`sweep::materialize_cold_set`, invariante #2). Ela executa apenas quando `[dream]
enabled` estiver definido E um provedor E um embedder estiverem configurados; um armazenamento sem provedor
mantém o caminho A3 sem LLM (invariante #13). Ela **nunca exclui uma fonte**
(invariante #16): o membro de maior retenção é reescrito e cada membro mesclado
é *substituído* com uma nota de mesclagem, para que o corpo completo pré-mesclagem continue
acessível (git + cadeia de substituição; `restore-page` o recupera), e
`page_evidence` (`reconsolidation` + `b2_dream:<id>`) registra quais membros alimentaram
cada mesclagem (a guarda contra mesclagem alucinada). A reescrita segue pelo caminho com portão de
aplicação (`preflight_admission(Consolidate)` antes do LLM, depois
`Wiki::apply_batch` no ator gravador único, invariante #2) com **`dry_run`
primeiro** (retorna o plano, não chama o LLM nem o gravador) e
**apenas saída estruturada com esquema JSON** (invariante #7). O agendamento (B3, em
`serve.rs`) inicia uma execução apenas após `[dream] idle_window_secs` sem nenhuma atividade
do cliente (lida do `ActivityClock` compartilhado do roteador de ferramentas) e **o cancela
no instante em que a atividade for retomada** (uma flag leve `DreamCancel` consultada entre
clusters), limitado a `max_clusters_per_run` clusters por execução (invariante #5).
O trabalho é ordenado pela **surpresa primeiro** (B4): os clusters mais inéditos, mais distantes
no espaço de embedding em relação à página existente (não fria) mais próxima, primeiro. Toda execução
retorna um `DreamReport` observável para que uma execução ruim nunca seja silenciosa. Nenhuma nova
migração (reutiliza `page_evidence` + substituição); nenhuma ferramenta MCP nova.

Páginas fixadas (`pinned: true` no frontmatter) são isentas de todos
os caminhos de decaimento. Páginas sob `_slots/` são fixadas automaticamente e exibidas
em snapshots de briefing/explore como pequenos slots de memória editáveis. Páginas de slot
podem declarar um regime de escrita com `slot_kind: state` ou
`slot_kind: invariant`; a omissão significa `state` para compatibilidade
com versões anteriores. Use `state` para contexto de trabalho mutável, como foco
atual e itens pendentes. Use `invariant` para contexto de projeto de alta resistência,
identidade, regras ou preferências do usuário; a consolidação não deve reescrever
um slot invariante existente a menos que novas observações contradigam diretamente
conteúdos existentes específicos.

Servidores compartilhados podem optar por `[slots] per_user = true`. Escritas de slots pelo
motor e por MCP usam então um namespace limitado derivado do `IdentityKey` autenticado;
resumos de sessão e prompts de consolidação incluem slots compartilhados mais o namespace do
chamador. Slots existentes sem namespace continuam compartilhados e o padrão permanece desativado.
Leituras exatas da wiki e buscas permanecem deliberadamente inalteradas: essa fronteira limita
injeção de prompt, não o acesso a páginas.

## Links entre projetos

Páginas normalmente criam links dentro de seu próprio projeto (`[[decisions/0001.md]]`, ou um
`label` apontando para `../gotchas/x.md`). Um wikilink também pode nomear outro projeto
para que dependências entre projetos se tornem arestas explícitas no grafo:

* `[[project:path.md]]`: um projeto irmão no mesmo espaço de trabalho.
* `[[workspace/project:path.md]]`: um projeto em outro espaço de trabalho.
* `[[_global:path.md]]`: o projeto reservado de preferências `_global` no
  espaço de trabalho padrão, mesmo quando a página de origem residir em outro lugar.
  `[[name]]` simples permanece local ao projeto; esta é a forma que nomeia o
  escopo compartilhado. Um `[[workspace/_global:path.md]]` explícito mantém o
  espaço de trabalho nomeado.

O analisador (`ai-memory-wiki::extract_links`) produz um `LinkTarget
{ workspace, project, path }`; o armazenamento o resolve contra a página mais recente
do projeto nomeado e registra o escopo em `links.to_workspace` /
`links.to_project` (NULL = o próprio projeto da origem, o caso comum).
A resolução é segura para adiamentos: um link para uma página que ainda não existe
permanece com `to_page_id = NULL` e é reapontado por
`refresh_incoming_links_for_path` quando essa página for criada posteriormente, tanto dentro
de um projeto quanto entre projetos.

Como `to_page_id` é um id global e `ReaderPool::page_links` faz o join por
id sem filtro de projeto, um link resolvido entre projetos aparece como um
backlink no seu destino sem trabalho extra; `RelatedPage` carrega o `workspace` /
`project` da origem, de modo que a dependência fica identificada e navegável. Isso
transforma as wikis por projeto em um único grafo de dependências (veja também
a verificação de referências pendentes do `memory_lint`, as contagens de dependentes do briefing e
o endpoint `/api/v1/graph`).

## Estrutura de crates

```
crates/
├── ai-memory-core/        domain types, errors, ids. NO IO.
├── ai-memory-store/       SQLite + writer actor + reader pool + decay math.
├── ai-memory-wiki/        atomic markdown writes, file watcher, git.
├── ai-memory-mcp/         rmcp transport + tool router.
├── ai-memory-hooks/       payload schemas, sanitiser, /hook ingress.
├── ai-memory-llm/         provider auth boundary + LlmProvider / Embedder traits.
├── ai-memory-consolidate/ Karpathy ingest / lint / sweep / auto-improve pipeline.
├── ai-memory-web/         read-only /web UI and /api/v1 JSON routes.
├── ai-memory-workstream/  read-only native transcript + launch adapters.
├── ai-memory-test-support/ shared test helpers (dev-dependency only, no test binary).
└── ai-memory-cli/         `ai-memory` binary entry point + thin HTTP subcommands.
```

Cada crate possui uma responsabilidade única e expõe uma API tipada;
nenhuma dependência circular. Limites entre crates impõem os invariantes
abaixo.

O transporte MCP é o rmcp 3.8 e atende aos dois ciclos de vida do protocolo: os clientes
MCP 2026-07-28 começam com `server/discover` e enviam metadados de versão
a cada requisição, enquanto os clientes mais antigos ainda concluem o handshake de
`initialize`. As requisições HTTP continuam sem estado por padrão; `--http-stateful`
vale apenas para o modo de sessão legado. A ponte stdio encaminha qualquer um dos
ciclos de vida para esse transporte HTTP.

## Superfície de ferramentas MCP (27 ferramentas)

Toda ferramenta registrada carrega anotações MCP explícitas para seu título legível por
humanos e comportamento somente leitura, destrutivo, idempotente e de mundo aberto. Essas
são dicas voltadas ao cliente, não autorização: clientes devem tratar anotações
de um servidor não confiável como não confiáveis, e toda verificação de auth em tempo de execução,
propriedade, escopo e admissão permanece autoritativa. O aperto de mão coloca seu contrato
completo de escopo e confiança antes da referência detalhada de roteamento para que clientes que
truncam instruções do servidor em 2.048 caracteres ainda recebam o essencial.
Um índice de uma linha para cada ferramenta segue esse núcleo e termina dentro de 4.096
caracteres. Esse é o limite de instruções do
[Claude Code](https://code.claude.com/docs/en/mcp#tool-search-for-mcp-server-authors)
desde a v2.1.296, e sua busca de ferramentas adia cada definição de ferramenta até que o Claude
peça por uma, portanto o índice é o único roteamento por ferramenta que essas sessões veem de
início. Testes mantêm o núcleo em 2.048 e o índice em 4.096 unidades UTF-16 (#1980).
O restante da #920 permanece deliberadamente não lançado: encurtar descrições exige
avaliação de seleção de ferramentas antes de remover orientações autocontidas de clientes
somente MCP; variantes de `response_format` ampliariam cada esquema de leitura enquanto
removeriam identificadores necessários por chamadas de acompanhamento; e uniões de escopo no período
de compatibilidade anunciariam formatos antigos e novos ao mesmo tempo. Dividir o código-fonte
do servidor é uma decisão interna de manutenibilidade, não um recurso de custo de tokens, e
não precisa pegar carona na alteração do protocolo público.

| Ferramenta | Dica | Propósito |
|---|---|---|
| `memory_query` | somente leitura | Busca FTS5 + correspondência de entidades + grafo RRF + RRF vetorial opcional + fluxo de evidência de observações (ativo por padrão), seguida de um ajuste limitado de autoridade por kind/tier/pinned/tag e de fallback bruto. Incrementa os contadores de acesso dos resultados de página. Usa por padrão o projeto atual; chamadas de projeto único (projeto implícito ou nomeado com `workspace`+`project`) também unem o escopo reservado de preferências `_global` como `global_scope_hits`, e somente um conjunto explícito de vários `scopes` o dispensa (#930); `scopes` busca em projetos irmãos nomeados; `global=true` busca em todos os projetos de uma vez (cada resultado anotado com seu workspace + projeto). Com `AI_MEMORY_RERANKER=llm`, os pools de candidatos de projeto/scopes são fundidos antes de, no máximo, uma passada final de relevância por LLM; os dados de consulta/título/snippet são limitados e codificados em JSON, e qualquer timeout, erro do provedor, conjunto de notas inválido/incompleto ou saturação de concorrência de quatro chamadas preserva a ordem ajustada. O ranqueador distinto de `global=true`, somente FTS, e os resultados suplementares de preferências globais não são reranqueados. `explain=true` anexa a cada resultado de projeto/scopes `score_details` (ranks por fluxo, entidades correspondentes, pontuações brutas de FTS/cosseno/frequência inversa de entidades, os ids de observação correspondentes quando o fluxo de observações votou, contribuições do RRF, procedência do grafo incluindo o tipo de aresta tipada (`causes`/`fixes`/`contradicts`) pelo qual um vizinho foi alcançado, a contagem de evidências da página, o multiplicador de autoridade e a nota opcional de rerank), além de uma lista `streams_active` no nível superior e de um objeto `timings` com o tempo de relógio de cada fluxo em milissegundos (`fts_ms`, `entity_ms`, `vector_ms` para as varreduras de vetores armazenados, `graph_ms`, `observation_ms`, `rerank_ms`; um fluxo que não rodou fica de fora, e as buscas com `scopes` somam por fluxo). Os tempos só são lidos em uma consulta explicada; o caminho normal não faz nenhuma leitura de relógio para eles. O ranqueador global somente FTS informa seu fluxo ativo e `fts_ms` sem detalhes por resultado; `as_of` não informa tempos. `include_expired=true` também devolve páginas com TTL expirado. `include_superseded=true` também devolve versões de página substituídas (não mais recentes) nos fluxos FTS/entidade/vetor/grafo, cada resultado marcado com `superseded: true` (a versão atual nunca é marcada); com o padrão desligado, o resultado é idêntico byte a byte ao comportamento apenas com as versões mais recentes, e `global=true` / `as_of` não são afetados. `pin_first=true` antepõe as páginas fixadas mais recentes do projeto, em número limitado (`ReaderPool::list_pinned_pages`, limite de 10), aos resultados fundidos, sem duplicar por id de página (uma página fixada que também corresponde aparece uma vez, marcada com `pinned: true`) e truncando de novo ao limite pedido; vale para buscas de projeto único (padrão ou `workspace`+`project`), é ignorado em `scopes`/`global`/`as_of`, e com o padrão desligado o resultado é idêntico byte a byte. `answer=true` (opcional, desligado por padrão) sintetiza, além disso, uma resposta citada em linguagem natural sobre os principais resultados, por meio do provedor de LLM configurado (`complete_structured`, JSON-schema `{ answer, citations }`), anexada como `answer: { text, citations }`. Os caminhos citados devem identificar exatamente uma versão de página fornecida, incluindo preferências suplementares. Citações repetidas são deduplicadas na ordem do modelo. Um caminho desconhecido, ou um caminho compartilhado por ids de página distintos, retém a resposta inteira e devolve `answer_unavailable` com os resultados de recuperação inalterados; isso inclui versões históricas com o mesmo caminho. Citações vazias continuam válidas como abstenção. Isso valida a identidade da fonte, não a veracidade das afirmações geradas. Sem provedor configurado, devolve os resultados mais uma nota `answer_unavailable` em vez de falhar, e com `answer` não definido/`false` nenhum provedor é acessado e a resposta é idêntica byte a byte (invariante #13). Aplica-se ao caminho normal de projeto único/`scopes`; `global`/`as_of` o ignoram. O parâmetro opcional `[query_answer] max_context_tokens` limita a admissão da síntese e preserva todos os resultados de recuperação. Cada byte de entrada UTF-8 conta como um token estimado; os bytes do schema serializado e uma margem fixa de 1024 unidades para o envelope do provedor também contam, e a margem de saída do nível de raciocínio é reservada à parte. Este é um contrato de admissão conservador, não uma garantia para o tokenizador ou o enquadramento de cada provedor. A admissão da resposta dá prioridade aos sucessores de substituição conhecidos sobre os predecessores, e às evidências mais novas dentro do mesmo workspace/projeto e título exato normalizado. Uma consulta limitada de metadados lê apenas ids de resultados já autorizados; as páginas canônicas de sessão usam a hora de início da sessão, e não a de ingestão. Evidências não relacionadas mantêm a ordem de recuperação. Snippets inteiros são admitidos antes de qualquer corte. O espaço que sobrar pode conter uma última frase ou instrução final completa, com reticências marcando o texto removido; um fragmento sem fronteira completa é omitido. A regra de fronteira reconhece a pontuação de frases e os terminadores de instrução de código e não faz inferência semântica. Snippets vazios e só com pontuação são omitidos. `answer_context` informa `admitted`, `omitted`, `clipped`, `estimated_input_tokens`, `reserved_output_tokens` e `max_context_tokens`; aparece somente quando a admissão roda. Se o conteúdo fixo ou as evidências não couberem, o provedor não é chamado e `answer_unavailable` acompanha os resultados completos. A pergunta nunca é cortada silenciosamente. A validação das citações usa apenas as fontes admitidas. O status de substituída é explícito nos prompts com orçamento. O corte pode remover uma ressalva ou correção; quem chama deve inspecionar o recibo e os resultados originais. Um limite explícito igual a zero ou subdimensionado impede a síntese; somente a omissão desativa a admissão. A configuração omitida preserva os bytes existentes de requisição e resposta. A qualidade das respostas ainda não foi validada por avaliações. Um nível opcional `reasoning` (`minimal` (padrão) / `low` / `medium` / `high` / `max`) ajusta o esforço da síntese: `ChatRequest` não carrega nenhum campo de raciocínio por requisição (o `reasoning_effort` do nível do provedor é fixado na construção a partir da configuração), então o nível se traduz em um orçamento máximo de tokens por nível, escalado a partir da base do caminho (base da resposta 2 000; `minimal` = 1x = idêntico byte a byte, `low` 1,5x, `medium` 2x, `high` 3x, `max` 4x). O nível é inerte, a menos que o caminho LLM de `answer` seja executado (invariante #13); um valor desconhecido é rejeitado pelo schema (invariante #7). Os filtros opcionais `kind`, `tag`, `tier` e `path_prefix` restringem os candidatos antes do ranqueamento, em todos os fluxos, inclusive na busca de substring CJK. |
| `memory_recent` | somente leitura | Páginas `is_latest=1` atualizadas mais recentemente. |
| `memory_read_page` | somente leitura | Busca o corpo de uma única página da wiki por `path` ou pelo principal resultado FTS5 para uma `query`; `workspace` + `project` opcional tem como alvo um espaço de trabalho/projeto irmão nomeado. O corpo é limitado por `max_chars` (padrão 12000, máx 64000); as respostas incluem `truncated` e `total_chars`. Use quando um agente precisar de mais do que os trechos de 24 palavras de `memory_query`. `include_related=true` também percorre o grafo de links a partir da página (BFS limitado reutilizando a primitiva `page_links` por nó: padrão 1 salto, limite rígido de 3, conjunto visitado global para desduplicação/segurança contra ciclos, limite total de nós de 50, ciente de múltiplos projetos) e retorna um array `related` de páginas acessíveis, cada uma rotulada com sua `depth` (distância em saltos) e `direction` (`link`/`backlink`); desativado por padrão, não adiciona campo `related`. A resposta carrega a versão atual da página como `page_id`, o valor que chamadas de comparar-e-escrever passam de volta, mais no máximo 100 títulos Markdown analisados com tokens SHA-256 de seção exatos para `memory_patch_page` (`sections_truncated` relata estouro). Hashes de seção sempre cobrem o corpo completo mesmo quando o corpo retornado estiver truncado. Títulos em blocos de código são excluídos. Uma página cujo TTL `expires_at` tenha expirado ainda é retornada e traz `expired: true`; a resposta de uma página ativa não possui esse campo. Uma leitura exata pode nomear um projeto existente pelo `project_id` estável em vez de `workspace` + `project`. |
| `memory_patch_page` | destrutiva | Anexa atomicamente dentro de uma seção Markdown analisada ou no EOF, adiciona uma seção única de nível 2-6 ou substitui uma seção. Exige o id da página atual; a substituição também exige o hash exato da seção retornado na leitura. Todas as edições são resolvidas contra um único corpo capturado, ambiguidades/conflitos falham antes da escrita, e a página completa resultante passa por `Wiki::write_page_if`, preservando o frontmatter e os limites normais de sanitizador, admissão, atribuição, substituição, índice, reversão e checkpoint do git. A autorização de escrita em escopo existente falha fechada e não pode criar um alvo. |
| `memory_read_session_observations` | somente leitura | Pagina através das observações brutas de hook de UMA sessão (`ObservationRecord` com corpo sanitizado completo, limitado por linha por `body_max_chars`), restrito às linhas que caíram no escopo resolvido e a sessões que o chamador pode ver; `total` e `elided_other_scope` relatam a contagem no escopo e as linhas que a sessão deixou em outro projeto. A omissão de `session_id` lê a sessão visível com o trabalho capturado mais recente além do próprio chamador, finalizada ou interrompida (`ReaderPool::recent_session_activity` ao longo de 30 dias, preferindo o diretório atual da sessão chamadora), senão a sessão visível concluída mais recente. `session_id: "current"` resolve para a própria sessão de hook do chamador quando o cliente a encaminha (`X-Memory-Actor-Session-Id`) e é recusada caso contrário (#1983). |
| `memory_status` | somente leitura | Contagens, caminhos, versão, mais o `scope` que respondeu: `workspace`, `project` e `resolved_by` (`explicit`, `session`, `shared_slot`, `startup_seed`, `default`, `default_after_mismatch`). Leituras sem escopo resolvidas por `startup_seed` ou `default_after_mismatch` também registram um aviso no servidor. |
| `memory_briefing` | somente leitura | Snapshot estruturado de contagens/atividade/regras/slots/recentes. Snapshots com escopo de projeto também carregam uma lista limitada `pinned` (até 10) das páginas mais recentes fixadas do projeto (`pinned = 1`, mais recente primeiro) como contexto quente permanente no SessionStart, distinto de `slots`, que é indexado pelo prefixo de caminho `_slots/`; vazio e omitido do JSON quando o projeto não possui itens fixados, de modo que o formato padrão permanece inalterado. `settled_first: true` opcional inicia com até 8 das páginas de `rule`/`decision` de maior destaque do projeto, ordenadas por contagem de evidências e depois por recência; desativado por padrão. `scope` nomeia o espaço de trabalho/projeto resumido e `resolved_by` como ele foi escolhido. `recent_sessions_limit` (1-50) define quantas sessões recentes ele lista. `lint`, presente depois que uma execução de lint grava `_lint/report.md` com suas contagens, informa os `warnings`, `findings`, `updated_at` do relatório e até cinco `warning_pages`; o contexto de início de sessão acrescenta uma linha apenas com a contagem quando `warnings` é diferente de zero. |
| `memory_explore` | somente leitura | Resumo em prosa por LLM sobre o snapshot de briefing, degradando para JSON sem um provedor. Um nível `reasoning` opcional (`minimal` (padrão) / `low` / `medium` / `high` / `max`) dimensiona o orçamento de tokens máximos do resumo a partir de sua base (16.000; mesmo mapeamento de 1x/1.5x/2x/3x/4x de `memory_query`); inerte no caminho somente de briefing sem provedor, e `minimal` é idêntico byte a byte. |
| `memory_handoff_begin` | destrutiva | Abre um handoff com escopo de proprietário para o próximo agente; `shared=true` deliberadamente o publica para o projeto. `workspace` + `project` opcional tem como alvo um espaço de trabalho/projeto irmão nomeado. |
| `memory_handoff_list` | somente leitura | Lista handoffs abertos próprios/compartilhados com campos de corpo e identidade inspecionáveis; não reivindica nem expira. `any_owner=true` exclusivo de root recupera entre operadores. `workspace` + `project` opcional tem como alvo um espaço de trabalho/projeto irmão nomeado. |
| `memory_handoff_accept` | destrutiva | Busca + confirma um handoff aberto próprio/compartilhado. Passe `handoff_id` de `memory_handoff_list` para reivindicar essa linha exata; omiti-lo ainda reivindica o handoff aberto elegível mais recente (handoffs automáticos correspondem ao diretório atual). `any_owner=true` exclusivo de root recupera entre operadores. `workspace` + `project` opcional tem como alvo um espaço de trabalho/projeto irmão nomeado. Retorna `handoff` mais `status`: `claimed` (esta chamada o ganhou), `consumed_by_hook` (o próprio SessionStart da sessão chamadora o reivindicou, portanto ele já está no contexto; necessita do id de sessão que o hook reivindicou, ou seja, um cliente ciente de sessão) ou `none_pending`. |
| `memory_handoff_cancel` | destrutiva | Marca um id exato de handoff aberto visível como expirado quando ele foi criado por engano; `any_owner=true` exclusivo de root recupera entre operadores. |
| `memory_message_send` | destrutiva | Envia uma mensagem direcionada entre projetos para a caixa de entrada de outro projeto (V64/V90). Exige `to_workspace` + `to_project`; o destinatário já deve existir (falha fechada, nunca criado). O corpo e evidências opcionais têm segredos removidos e tamanho limitado; uma natureza `measured` exige evidência não vazia. Recusa um escopo de remetente inferido (clientes estáticos passam `from_workspace` + `from_project`). A única ferramenta que cruza o isolamento de projetos intencionalmente. `to_agent` opcional restringe a reivindicação a um harness registrado; `from_agent` registra o harness do remetente. |
| `memory_message_list` | somente leitura | Lista mensagens pendentes para este projeto: `box="inbox"` (retirável, padrão) ou `box="outbox"` (cancelável). Corpos são entradas não confiáveis entre projetos. Um escopo inferido adiciona `resolved_scope` / `scope_source` / `hint`. |
| `memory_message_pop` | destrutiva | Reivindica UMA mensagem da caixa de entrada exatamente uma vez (a mais antiga ou um `message_id` específico); retorna-a delimitada como entrada não confiável com proveniência do remetente, ou `null` quando vazia. Recusa um escopo inferido. `agent` opcional reivindica apenas mensagens endereçadas àquele harness ou a nenhum. |
| `memory_message_cancel` | destrutiva | Retrata uma mensagem enviada pendente por `message_id`, ou limpa toda a caixa de saída quando omitido. Com escopo para o projeto remetente; recusa um escopo inferido. |

`memory_handoff_list` é o caminho de inspeção sem reivindicação para clientes que não conseguem injetar o stdout do SessionStart. `memory_handoff_cancel` exige um id exato. `ai-memory handoffs` lista os handoffs
abertos de um projeto, do mais antigo ao mais novo, com seus ids. É somente leitura e
não expõe conteúdo (identidade, procedência e idade, nunca o corpo do resumo). A expiração
automática poupa de propósito os handoffs manuais e de diretórios irmãos, de modo que uma
entrada de longa duração ali é o comportamento esperado da política, não uma falha.

| `memory_consolidate` | destrutiva | Reescrita de página orientada por LLM. `multi_page=true` para fan-out atômico; uma atualização cujo caminho nomeie uma página fixada existente é ignorada (`_slots/` excetuados). Omitir `session_id` (ou enviar um em branco) consolida a sessão concluída mais recente no projeto resolvido; a mesma omissão em um projeto sem nenhuma falha como `no completed session in <scope>`. Prompts de consolidação anexam o corpo reservado ativo `_prompts/consolidation.md` do projeto de destino como preferências consultivas sanitizadas, limitadas a 2.000 caracteres, codificadas em JSON e não confiáveis; páginas com TTL expirado são ignoradas e um argumento `instructions` por chamada substitui a página para uma chamada. Ambos os prompts do sistema mantêm regras de esquema, evidência, divulgação, uso de ferramentas e saída como autoritativas. |
| `memory_feedback` | escrita | Registra um sinal de qualidade para uma página por `path` exato: `helpful`/`not_helpful` alteram `pages.salience` para páginas episódicas elegíveis à varredura, enquanto `stale`/`wrong` nivelam a relevância na base e expõem qualquer página atual como uma constatação de lint `feedback_flagged`. Nunca exclui; o caminho resolve para a versão atual na transação, de modo que uma reescrita posterior o limpa. Conteúdo recuperado nunca autoriza feedback por si só. |
| `memory_auto_improve` | escrita | Revisa manualmente uma sessão concluída e aplica ou prepara edições validadas da wiki através do caminho de aprovação de autoaperfeiçoamento. Sem um ID de sessão, seleciona a sessão concluída mais recente sem execução persistida de autoaperfeiçoamento para que chamadas repetidas avancem por saltos de pré-verificação; um ID explícito continua reexecutável. O servidor também agenda revisão para novas sessões; `[auto_improve] require_approval = true` deixa propostas pendentes para revisão manual. |
| `memory_write_page` | destrutiva | Grava conhecimento durável na wiki quando o usuário pede explicitamente para lembrar/anotar algo. `scope: "global"` grava no escopo reservado de preferências `_global` e `scope: "profile"` no perfil entre projetos do chamador (caminho colocado sob `profile/`); `expires_at` opcional define um TTL RFC3339 ou apenas de data; `session_id` opcional cita uma sessão do mesmo projeto como evidência e, em `sessions/<id>.md`, carimba o frontmatter da página de sessão e liquida o trabalho de consolidação enfileirado; `source_files` opcional (até 16 entradas `{path, commit}`) nomeia os arquivos do repositório que a página descreve, os quais o servidor armazena mas nunca lê, para que um cliente possa compará-los com seu checkout e registrar feedback `stale`. `expected_page_id` opcional (o `page_id` que uma leitura retornou) grava apenas se essa ainda for a versão mais recente, e `create_only: true` recusa sobrescrever uma página existente; uma pré-condição com falha não grava nada e retorna `reason: "precondition_failed"` com a versão atual (consulte `docs/programmatic-memory.md`). Leituras e escritas exatas de páginas podem selecionar um projeto existente com `workspace` + `project_id` estável; isso é mutuamente exclusivo com o nome do projeto e nunca cria um escopo. |
| `memory_resolve_write_target` | somente leitura | Pré-verifica um `workspace` + `project` ou `project_id` exato existente com autorização de escrita, sem contingência/criação/promoção, retornando o nome canônico e o id estável. Importadores em lote resolvem cada destino mapeado antes de sua primeira escrita para que um mapeamento incorreto não falhe apenas após o término de lotes anteriores. |
| `memory_write_pages_batch` | destrutiva | Grava de 1 a 100 versões completas de páginas atomicamente em um projeto existente, selecionado por `workspace` + `project` ou `project_id` estável. Toda entrada é condicional (`create_only` ou `expected_page_id`), caminhos duplicados são recusados, cada corpo é limitado a 1 MiB e o total a 8 MiB. Admissão, sanitização, atribuição, atualizações de índice e reversão usam a mesma fronteira `Wiki::apply_batch_if`; uma incompatibilidade retorna seu índice de requisição/id atual e não grava nada. Destinado a importações de companheiros limitadas, não para notas rotineiras de agentes. |
| `memory_delete_page` | destrutiva | Exclui uma única página pelo `path` exato; `scope: "profile"` exclui do perfil entre projetos do chamador. Dispara a cadeia de admissão (op=delete); idempotente. `expected_page_id` opcional exclui apenas se essa ainda for a versão mais recente; caso contrário, nada é excluído e a chamada retorna `reason: "precondition_failed"`. |
| `memory_restore_page` | destrutiva | Somente Admin, assim como `/admin/restore-page`. Sem `rev`, lista os checkpoints git que alteraram `path`. Com `rev`, grava essa versão (corpo e metadados de frontmatter) de volta pelo caminho `memory_write_page` (sanitizador, admissão, slots, substituição), incluindo uma página excluída. Resumos de sessão expurgados são recusados antes da escrita. Páginas de projeto apenas, não `scope: "profile"`. |
| `memory_forget_sweep` | destrutiva | Passagem de retenção: despeja páginas frias através da camada da wiki, expurga ancestralidade de lápides antigas e exclui permanentemente páginas com TTL expirado. `dry_run=true` para pré-visualização. |
| `memory_lint` | destrutiva | Constatações de contradição baseadas em regras + LLM → `wiki/_lint/`. Também executa um **detector de candidatos de similaridade sem LLM** (design-memory-aging.md A5): páginas semânticas/procedurais frias cujos embeddings já armazenados estejam na faixa de similaridade de cosseno de `contradiction_band_min`–`contradiction_band_max` (padrão 0.4–0.75; "mesmo tópico, não uma quase duplicata"; no/acima do máximo é desduplicação A3, abaixo do mínimo não relacionado) recebem uma constatação consultiva `similarity_candidate`. Similaridade por si só nunca alega contradição ou substituição; arestas explícitas de `contradicts` ainda emitem `contradiction`. A varredura é limitada (uma carga de embeddings sobre o conjunto frio limitado, constatações limitadas, determinística), uma operação sem efeito limpa quando nenhum embedder estiver configurado e apenas consultiva (nunca exclui/edita/substitui uma página e não persiste arestas, invariantes #13, #16, #2), não necessitando de migração. Em um armazenamento de idioma único ou domínio único, a similaridade de fundo entre páginas não relacionadas pode ficar acima do piso padrão; eleve `contradiction_band_min` (`config.toml` ou `AI_MEMORY_CONTRADICTION_BAND_MIN`) para reduzir essa fila de revisão. |
| `memory_install_self_routing` | somente leitura | Retorna o trecho canônico e enxuto de roteamento mais os payloads de Agent Skills gerenciadas e dicas de destino para instalações em CLAUDE.md / AGENTS.md. |

`memory_briefing`, `memory_explore`, `memory_write_page`,
`memory_install_self_routing`, `memory_read_page`,
`memory_read_session_observations`, `memory_delete_page`,
`memory_handoff_cancel`, `memory_auto_improve` e `memory_feedback`
são posteriores ao corte original "restrito de propósito" (§10 de
`design-decisions.md`): briefing/explore separam as metades estruturada e
em prosa do "o que está acontecendo", `memory_write_page` cobre anotações
duráveis explícitas sem abusar de handoffs de uso único,
`memory_install_self_routing` existe para o caso meta em que o agente
deve reescrever suas próprias regras de roteamento no `CLAUDE.md` /
`AGENTS.md` de um projeto e instalar as Agent Skills gerenciadas complementares em
`.claude/skills` ou `.agents/skills`, `memory_read_page` complementa
`memory_query` para o caso "preciso da página completa, não de um trecho"
(ex.: abrir uma página de decisão de ponta a ponta),
`memory_read_session_observations` abre as evidências brutas por trás de uma página
compilada ou de um acerto bruto (uma sessão, em escopo, paginada e com corpo limitado) para que o agente
possa auditar o que os hooks realmente capturaram, `memory_auto_improve` expõe uma
revisão de aprendizado segura e ativa por padrão através do mesmo caminho de aprovação/escrita das
escritas pendentes, e `memory_delete_page` é o par destrutivo de caminho exato
necessário para espelhos cientes de admissão. `memory_patch_page` fornece edições limitadas
e pré-condicionadas sem exigir que agentes reemitam uma página inteira.
`memory_handoff_cancel` é a válvula de segurança
para a criação equivocada de handoffs. `memory_feedback` implementa o
item P2 de "reforço mais detalhado além das contagens de acesso" de
`prior-art-implementation-findings.md`: ele não pode pegar carona em uma ferramenta de leitura
sem misturar semânticas de leitura e escrita, e o contador de acessos que ele
suplementa não sabe distinguir "esta página respondeu à dúvida" de "esta página
desperdiçou uma leitura". A disciplina de superfície estreita ainda se mantém (cada nova ferramenta
precisa justificar seu espaço), mas a contagem agora é 25, não 10.

As Agent Skills gerenciadas são uma estreita exceção de empacotamento de prompt em relação
à arquitetura que de outra forma é centrada na wiki. São arquivos `SKILL.md` estáticos que
ensinam os agentes quando chamar as ferramentas MCP do ai-memory; não são páginas duráveis da wiki,
não são saídas de autoaperfeiçoamento e não são um roteador de skills em tempo de execução dentro do ai-memory.

Apelidos de parâmetros MCP são intencionalmente escassos: `memory_query.query` aceita
`q|search`, e campos de limite aceitam `n` / `top_k` onde disponibilizados. Parâmetros de projeto e
diretório de trabalho usam seus nomes canônicos.

O `ai-memory mcp-bridge` é um adaptador de transporte, não uma segunda implementação
de ferramentas. Ele serve o catálogo de ferramentas upstream sobre stdio local e
delega chamadas de ferramentas ao servidor HTTP configurado através do transporte cliente do rmcp.
Ele usa o bearer que o `Config::load` resolveu, ou a chave de hook armazenada da instalação antes
de uma chave root descoberta, a qual envia apenas para um servidor de loopback.
O servidor mantém, portanto, a mesma autenticação, resolvedor de escopo e manipuladores de ferramentas que clientes
HTTP diretos. Sua perna a montante roda em uma revisão mais antiga do MCP, então a ponte
restaura o campo `resultType: "complete"` que a revisão 2026-07-28 exige nos resultados de
`tools/list` e `tools/call` que ela encaminha; o rmcp o descarta de novo para um cliente
stdio em uma revisão mais antiga. Ele possui dois modos, escolhidos pela entrada que o inicializa:

- ciente de sessão (Claude Code, `install-mcp --client claude-code
  --session-aware`, e toda entrada do Claude Code em uma instalação com arquivo de token; e
  Meta Muse Code, cuja entrada é sempre `mcp-bridge --agent muse`): ele
  injeta o id de sessão herdado do harness inicializador
  (`CLAUDE_CODE_SESSION_ID`, ou `MUSE_SESSION_ID` com `--agent muse`, nunca
  o outro) como `X-Memory-Actor-Session-Id` e falha fechado sem ele,
  de modo que um registro destinado ao roteamento por sessão nunca se degrade silenciosamente para
  o slot compartilhado;
- sem sessão (`--no-session-id`, todos os outros clientes em uma instalação com arquivo de
  token): ele não encaminha identidade de sessão e ignora um `CLAUDE_CODE_SESSION_ID`
  herdado, o qual um cliente iniciado a partir de um terminal do Claude Code
  caso contrário emprestaria ao ponteiro de projeto ativo daquela sessão.
  O cliente é um cliente MCP estático e passa `workspace` + `project`; uma
  chamada que não nomeia escopo obtém o projeto do diretório de lançamento, resolvido uma vez
  na inicialização com as regras da CLI leve (`docs/auto-scope.md`).

A bridge conecta-se ao upstream antes de servir o stdio, portanto uma credencial recusada
(401) ou um servidor inacessível a encerra imediatamente com o motivo.

## Classes de autenticação HTTP

O processo separa quatro classes de credenciais ativas de um caminho transitório
de compatibilidade com navegadores:

| Classe | Transmissão | Autoriza |
|---|---|---|
| Senha humana | Corpo de `POST /auth/login` | Apenas emissão de sessão |
| Sessão web | Cookie `ai_memory_session` + CSRF | `/auth/me`, `/admin/*`, `/api/v1/*` por `AuthLevel`; nunca `/mcp` ou hooks |
| Recuperação | Corpo de `POST /auth/recovery` | Redefinição de senha de root; sem sessão |
| Chave de API | `Authorization: Bearer` (`aim_`, o bearer root de `AI_MEMORY_AUTH_TOKEN`, `[auth].bearer_token` ou `[auth].bearer_token_file`, ou externo `amk_`) | APIs de máquina; nunca uma sessão web |
| Login com token no navegador (sem contas humanas ainda) | HTTP Basic com o bearer root como senha, reenviado pelo navegador a cada requisição (nenhum cookie é definido; um cookie `ai_memory_auth` definido por uma compilação mais antiga ainda é aceito) | Rotas de navegador apenas GET até que exista qualquer senha humana ou bootstrap concluído; nunca rotas de máquina. Uma instalação nova não possui contas humanas, logo é assim que um navegador chega ao `/web` ali: o 401 carrega um desafio Basic, que o redirecionamento de login HTML deixa passar |

O caminho depreciado Basic/cookie é interrompido imediatamente quando a autenticação humana
se torna ativa; não é necessário reiniciar. O HTML da SPA `/web` é estático público; o navegador
de wiki integrado e as APIs JSON permanecem atrás da classe de rota acima.

### `GET /admin/metrics`

Uma coleta em formato de texto Prometheus (`text/plain; version=0.0.4`) de contadores
do tempo de vida do processo: admissões de hooks por resultado e entregas concluídas por
desfecho (os atômicos de `IngestMetrics` que `/admin/status` já relata), o
timestamp da última persistência, os desfechos de caminhos de arquivos de
`ai_memory_hook_file_paths_total` (`[ingest] index_file_paths`, uma série por desfecho fixo, nunca um
caminho), chamadas de ferramentas MCP divididas em `read` e `write`, e
histogramas de latência em intervalos fixos para `POST /hook` (tempo para responder, não o
processamento gerado) e `memory_query` (tempo de relógio, erros incluídos). Os intervalos
vão de 1 ms a 10 s mais `+Inf`. Cada observação é uma varredura de bucket limitada e
adições atômicas relaxadas, sem lock e sem alocação. Cada rótulo provém de um
conjunto fechado em tempo de compilação (`result`, `outcome`, `kind`, `le`), de modo que nenhum
texto de consulta, corpo de página, prompt, nome de cliente, escopo ou nome de projeto, ou token
pode aparecer em uma série, e escopo nunca é um rótulo. A rota fica protegida pelo mesmo
portão `require_root_for_multiuser_admin` que toda rota `/admin/*`: exclusiva de root assim que
a implantação distingue operadores. Os contadores são zerados na reinicialização.

### Telemetria de cargas de trabalho de LLM

O snapshot de `GET /admin/status` protegido por root inclui um array limitado
`providers.llm_workloads` quando um LLM está configurado, com
`providers.llm_workloads_started_ms` informando quando seus contadores iniciaram. Pontos de chamada
anexam um membro do enum fechado `LlmWorkload` ao id de operação lógica UUIDv7
existente; seu valor exibido/transmitido permanece apenas o UUID, para que o rótulo não
possa afetar uma requisição de provedor ou correlação de contingência. O wrapper de saúde do
provedor registra uma invocação da interface do provedor em torno de toda a cadeia de contingência
(repetições do chamador externo são invocações separadas): desfecho,
tempo de relógio, limites p50/p95 em buckets fixos, estimativas grosseiras de quatro
caracteres por token, uso reportado pelo provedor quando disponível e classe/status de erro
com redação de dados sensíveis. Ele não armazena prompt, saída, escopo, projeto, modelo selecionado por um
chamador ou rótulo ilimitado. O estado fica na memória, zera na reinicialização e não
escolhe provedor ou modelo. O servidor constrói o wrapper uma vez na inicialização;
não há recarregamento a quente de provedores em tempo de execução.

Os rótulos não roteiam nada. Um perfil `[llm.<task>]` na configuração fornece àquela
tarefa seu próprio provedor e cadeia de contingência (#1969), qualquer outra tarefa usa o
provedor global e sua cadeia ordenada de contingência, e `llm_task_chains` relata
a saúde de cada cadeia de tarefas configurada.

### Busca web e o rastreamento de recuperação

Uma busca de projeto na interface web (`/web/search` com `workspace` e `project`)
e um `/api/v1/search` com escopo (um projeto ou `scopes`) realizam a classificação pelos mesmos
três passos compartilhados que uma busca de projeto/escopos de `memory_query`:
`ai_memory_llm::embed_search_query` gera embeddings da consulta com o embedder configurado,
`ReaderPool::hybrid_search_scopes` funde os fluxos FTS5, de entidade, vetorial, de grafo
e de observações e aplica o ajuste de autoridade (mesclando vários escopos pelo melhor
ranking, depois ranking, caminho e id), e o `ai_memory_llm::RerankStage` opcional reordena
um prefixo limitado. O `serve` lê a configuração uma vez e entrega o mesmo embedder e estágio
de reclassificação ao servidor MCP e ao `WebState` (`ai_memory_web::Recall`); o portão de
simultaneidade de quatro chamadas do estágio é compartilhado, de modo que a busca web não pode adicionar
chamadas de provedor além dele. Uma busca sem escopo permanece no classificador FTS5 entre projetos,
assim como `memory_query(global=true)`. A busca web aceita filtros de `kind` e `tier`, enquanto
`memory_query` também aceita filtros de `path_prefix` literal e `tag` exata. Todo fluxo aplica seu
`PageFilter` em sua própria consulta, portanto uma busca filtrada restringe cada fluxo antes da
fusão e o reclassificador vê apenas as páginas que o filtro mantém. `/api/v1/search`
atualmente não passa nenhum filtro.
`ai-memory-mcp/tests/suite/web_search_parity.rs` mantém as ordens alinhadas.
`explain=1` em uma busca de projeto renderiza, por acerto, o ranking de cada fluxo, pontuação bruta
e contribuição RRF, a pontuação fundida, o multiplicador de autoridade, o impulso de intenção
e a pontuação de reclassificação quando presente, mais os tempos por fluxo
(`StreamTimings`). Uma busca de projeto resolve seu escopo com a consulta protegida de não criação,
portanto um projeto restrito recusa um visualizador sem concessão antes que qualquer
classificação seja
executada.

### Painel de saúde web

`/web/health` é uma página somente leitura protegida pela mesma decisão `Capability::Admin`
que `/web/pending` e `/admin/*`: exclusiva de root assim que a implantação distingue
operadores. Ela exibe os contadores de hooks (as `IngestMetrics` por trás de
`/admin/metrics`, incluindo os 429 decorrentes de saturação e limite de taxa), a
profundidade da fila do gravador, o spool de hooks e o diário de recuperação sob o diretório de dados
do servidor (`ServeHealthProbe` em `serve.rs`), a fila de consolidação de SessionEnd
(`ReaderPool::session_consolidation_queue`: contagens por estado, última conclusão,
requisição pendente mais antiga), o embedder configurado e a saúde passiva de ambos os provedores,
e os números do índice derivado e de armazenamento. `ai_memory_web::health::checks`
transforma esses dados em avisos com os comandos de correção que o `ai-memory status` imprime
(`ai_memory_core::fix_lines`: `ai-memory recover`, `ai-memory compact --confirm`,
`ai-memory embed`, `ai-memory embed --force` e a dica de repetição do provedor de LLM).
A página contém apenas contagens, timestamps e rótulos configurados; texto de erro de provedor,
conteúdo de páginas e nomes de escopo nunca chegam até ela.

`GET /admin/status` (e `ai-memory status --json`) carrega adicionalmente um bloco
`consolidation` — as mesmas contagens, mais até 20 `failed_sessions`
(`ReaderPool::failed_consolidation_jobs`: id da sessão, tentativas, horário da falha
e o erro limitado do worker através da remoção de dados sensíveis). O id da sessão é o que uma
reconsolidação precisa após a causa ser corrigida, logo pertence à superfície do operador
e não ao painel de saúde, cujo contrato de "apenas contagens e timestamps"
permanece inalterado (#1692).

## Superfície de subcomandos da CLI

```
init                        status               list-projects
run                         show                 continue
resume                      workstreams          rename-workstream
workstream-search           audit-contamination  search recent
read-page                   write-page           delete-page
serve                       reset                backup
restore                     reindex              install-hooks
hook                        install-mcp          commit
checkpoints                 restore-page         llm-test
forget-sweep                lint                 curator
auto-improve-report         auto-improve         finalize-session
recover                     pending-writes       embed
generate-auth-token         setup-agent          bootstrap
install-instructions        install-skills       reorg
purge-project               rename-project       move-project
merge-project               move-session          uninstall
upgrade                     auth                  user
completions                 handoffs             purge-session
compact                     api-key              export-okf
message                     doctor               backfill
project                     reclaim-ledger-versions  repair-split
repair-backfill-timestamps  server               backup-agents
restore-agents              profile              setup
```

Execute `ai-memory --help` para a árvore completa.

`setup` é a instalação nativa em um único passo. É uma composição enxuta de chamadas de biblioteca
existentes: `init` (que emite o bearer root em um diretório de dados novo), um
serviço por usuário renderizado a partir de `packaging/systemd/ai-memory-user.service` ou
do modelo LaunchAgent do launchd com o binário em execução, diretório de dados e caminho
de configuração preenchidos, depois `install-mcp`, `install-hooks` e
`install-skills` global para cada agente nomeado com `--agent` ou detectado a partir de seu
armazenamento de sessão ou configuração MCP (os armazenamentos que `doctor` varre). Ele finaliza com uma
autoverificação somente leitura: `/healthz`, uma chamada `memory_status` via MCP e
`doctor`. O gerenciador de serviço é acessado por um executor injetável, de modo que a
renderização da unidade e as chamadas a `systemctl`/`launchctl`/`reg` são testadas sem uma
sessão real. No Windows, a "unidade" é um valor `ai-memory` sob a chave por usuário
`HKCU\...\CurrentVersion\Run` que executa o comando oculto `service-host`
no logon; o `service-host` desacopla do console e supervisiona
`serve` com um console oculto, reiniciando-o após uma falha e registra ambos
os pids em `<data_dir>/service-host.json`. Uma segunda execução não altera arquivos. Cada
unidade renderizada carrega uma linha de identidade com o SHA-256 do restante do
arquivo; `uninstall` para e remove apenas uma unidade cujo digest ainda corresponda (no
Windows, apenas um valor de `Run` com o formato que o `setup` grava).
`install.sh` e, para o Windows, `install.ps1` são ativos de release com seus próprios
arquivos `.sha256`; cada um instala o arquivo de release após verificá-lo contra
o checksum publicado, sob `~/.local` ou `%LOCALAPPDATA%\Programs`.
`setup --web` serve o plano a partir de um roteador axum de disparo único na CLI
(`commands/setup_web.rs`), separado da interface somente leitura `/web`: somente loopback,
um segredo de caminho de uso único, verificação exata de `Host`, `Origin` mais CSRF em
todo POST e um Apply que executa `setup --yes --agent ...` como um processo
filho (linha 17 de security-boundaries).

O comando oculto `render-plugins --out <dir>` gera os plugins de marketplace do
Claude Code e do Codex sob `plugins/` e seus catálogos
(`.claude-plugin/marketplace.json`, `.agents/plugins/marketplace.json`) a partir
das mesmas listas de eventos e skills gerenciadas que `install-hooks` e `install-skills`
usam, fixados na versão do binário e na tag `v<version>` (#1166). Um teste da suíte
de CLI falha quando a árvore commitada diferir de sua saída, e
`bin/release` o executa novamente após o bump de versão. Os hooks de cada plugin chamam um
inicializador POSIX que encontra o binário instalado e executa via `exec` o `hook` nativo
com `AI_MEMORY_HOOK_ORIGIN=<agent>-plugin`; o inicializador não possui lógica de captura,
logo o plugin é uma quarta porta de entrada que não adiciona nada à superfície de
confiança. A origem permite que `hook` descarte uma cópia de plugin que um hook de arquivo de
configurações do Claude já entrega e ignore sessões sem pasta
(`commands/plugin_origin.rs`). O `setup` e a ligação automática de `run` deixam um agente
com um plugin ativado desconectado; `doctor`, `upgrade` e `uninstall` leem a instalação
de plugin em nível de usuário (`commands/agent_plugins.rs`).

`auth enable` ativa o bearer root para uma instalação criada antes que novas
instalações tivessem um; consulte [bearer root de `[auth]`](#auth-root-bearer).

`auto-improve-report` é somente leitura por padrão; `--stage` cria uma página pendente
de relatório de telemetria para auditoria/aprovação sem preparar edições de memória de aprendizado.

`reclaim-ledger-versions` descarta as versões substituídas do registro contábil de eventos brutos
de hook que o indexador anterior à 2.1.1 deixou para trás (#660). É uma execução de teste a menos
que `--confirm` seja passado. Um caminho só é candidato quando seu *conteúdo* inicia
com uma entrada de log de hook, usando a mesma barreira
`ai_memory_core::log_ledger::body_opens_with_log_ledger` que o indexador
(#660), a migração de conformidade OKF (#669) e a exportação de bundle (#748) usam,
de modo que uma página real chamada `log-2026-09.md` mantém toda a sua cadeia de versões. Apenas
linhas com `is_latest=0 AND superseded_at IS NULL` são elegíveis, de forma que linhas que uma lápide
de decaimento possui fiquem com `forget-sweep`. Linhas derivadas de FTS/entidade/vetor/link
vão com a página através dos `ON DELETE CASCADE`s existentes, e o gatilho de exclusão do FTS é desativado
para a exclusão em massa (sua DDL é lida de volta de `sqlite_master` e reexecutada)
para que a limpeza não tokenize novamente dezenas de gigabytes de corpo de registro linha por linha;
`pages_fts` é então reconstruída por completo.
`--compact` executa adicionalmente `VACUUM` para retornar os bytes, ao custo que `compact`
documenta.

## Invariantes transversais

Definidos em M0/M1; todo marco deve respeitá-los. Cada um provém de
um bug documentado do estado da arte anterior; cite a fonte ao revisar alterações
que toquem na área relevante.

1. **Um único caminho de leitura de configuração.** As configurações próprias do ai-memory são lidas uma vez pelo
   `Config::load()` na inicialização e nunca relidas do ambiente;
   código que precisa ler o ambiente do processo ou do harness recebe uma consulta injetável
   para que testes possam simular. (agentmemory #456 / #469.)
2. **Ator gravador único do SQLite.** Todas as escritas passam por um canal `mpsc`
   para uma thread de SO dedicada. Descartar o manipulador final do gravador
   drena comandos aceitos e junta aquela thread, mesmo quando a fila limitada
   estiver cheia ou chamadores tiverem cancelado suas esperas de resposta. (cognee #2717.)
3. **Índices são confirmados na mesma transação que os dados.** Sem
   indexação-em-tarefa-de-segundo-plano-após-retorno. (basic-memory #763 / #578.)
4. **Identidade tipada de 3 tuplas** (`workspace_id`, `project_id`, caminho)
   em toda linha de domínio desde o primeiro dia. (basic-memory #783 / #834.)
5. **Hooks são de disparo sem espera.** Scripts de hook têm timeout rígido em
   ≤200 ms; o servidor retorna 202 imediatamente ou 429 quando saturado.
   A única exceção é um 409 para uma sessão que o servidor já sabe estar
   vinculada a outro agente para aquele chamador, decidido a partir de uma visão
   em memória (`SessionAgents`) sem ida ao banco de dados (#2023).
   (agentmemory #221 / #143.) As sondagens do sistema de arquivos do roteador do diretório de trabalho de um evento
   (descoberta git, canonização) nunca rodam em um worker assíncrono: elas
   passam por `CwdProbes`, um caminho de pool bloqueante com quatro permissões e um
   prazo de 2 segundos, para que um `open()` travado (controles de privacidade do macOS, uma montagem de
   rede inativa) falhe aquele evento em vez de paralisar o servidor (#1514).
6. **A remoção de privacidade é uma fronteira tipada.** `Sanitized<NewObservation>`
   não tem outro construtor além de `sanitize()`. (design-decisions §14.)
   O trecho opcional de assistente/Stop (#196) entra através dessa mesma
   fronteira: o cliente o sanitiza antes que ele atinja a rede, e o
   servidor o limpa novamente aqui com seus padrões configurados antes da escrita.
7. **Apenas saídas estruturadas com esquema JSON.** Modos JSON nativos
   do provedor; sem XML, sem empacotamento com Instructor. (agentmemory #492 / #539,
   cognee #2840.)
8. **`{provider, model, dim}` desnormalizados ao lado de cada embedding.**
   Avisa e ignora vetores obsoletos em divergências até que o reembedding termine.
   (agentmemory #469.)
9. **Verificação de processo ativo antes de operações de ciclo de vida direto no disco.** `ai-memory reset`,
   `restore`, `reindex` e `uninstall --purge-data` consultam `sysinfo`; a
   guarda de desinstalação é condicional a `--purge-data`. `backup` é um cliente HTTP leve
   em vez disso: o servidor faz snapshot do SQLite com sua API de backup online enquanto
   o gravador permanece ativo. (basic-memory #765.)
10. **Escritas atômicas de arquivo** (tmp + rename + fsync). O observador ignora
    suas próprias escritas pelo prefixo do nome de arquivo. No Windows, um antivírus, o
    indexador ou um processo simultâneo pode reter um arquivo por um momento, logo operações
    de arquivo que competem com eles tratam erros específicos do Windows como
    transitórios: uma repetição limitada ou um salto, nunca uma espera ilimitada e nunca
    uma verificação mais fraca para um arquivo que permanece inacessível. Cada local mantém sua
    própria política porque seus prazos diferem (o caminho de hook fica dentro de seu
    orçamento de 200 ms); os locais estão listados em
    [design-windows-ci.md](design-windows-ci.md#windows-transient-file-errors).
    Novo código de E/S de arquivo com a mesma concorrência segue o mesmo padrão, com um
    teste que injeta o erro para que também seja executado fora do Windows.
    O `.git` da wiki obtém a mesma durabilidade do libgit2: todo manipulador que
    o ai-memory abre carrega `core.fsyncObjectFiles = true` de
    `.git/ai-memory.gitconfig`, um arquivo que a CLI do git nunca lê, para que objetos
    e refs sejam sincronizados antes de sua renomeação, e a inicialização registra um `ERROR` quando
    `HEAD` não resolve para um commit legível (o reparo está em
    [lifecycle-ops.md](lifecycle-ops.md#histórico-git-da-wiki-ilegível-após-travamento)).
11. **Diretório de dados canônico absoluto** como padrão; registrado de forma visível na
    inicialização. (agentmemory #303.)
12. **Sem singletons globais / configurações `lazy_static`.** Todas as dependências
    explícitas. (cognee #2228.)
13. **Caminho padrão sem LLM.** O LLM é opcional via variáveis de ambiente. O
    sistema funciona sem nenhum provedor configurado.
14. **A autenticação do provedor resolve antes da construção do provedor.** Clientes
    nativos de provedor consomem material `ProviderAuth` tipado; eles nunca
    leem variáveis de ambiente diretamente. Provedores baseados em token recebem caminhos
    explícitos de arquivo de autenticação / material de token derivado de ambiente por essa fronteira,
    assumindo então a atualização e persistência específicas do provedor.
15. **Subscritores de tracing filtram explicitamente seu próprio módulo.**
    Sem loops de feedback. (agentmemory #519.)

## Configuração (`config.toml`)

Fica em `<data_dir>/config.toml`. Valores escalares podem ser substituídos por variáveis
de ambiente prefixadas com `AI_MEMORY_*`; listas como `[[llm_fallbacks]]` são exclusivas do arquivo.

```toml
language = "en"                    # human-facing terminal language: "en" or "pt-BR";
                                   # --language overrides it. JSON, MCP and stored
                                   # content keep their format (docs/localization.md)
bind = "127.0.0.1:49374"
log_level = "info"                 # default filter also pins `rmcp=warn` (the MCP SDK's
                                   # per-request info logs) and drops the 30s reconcile
                                   # summary to debug (#894). Restore either via log_level
                                   # (e.g. "info,rmcp=info", "debug") or RUST_LOG;
                                   # `tracing_appender=warn` stays forced (feedback-loop guard)
tcp_keepalive_secs = 60            # idle time before TCP keepalive probes an accepted `serve`
                                   # connection; reaps sockets left half-open by a dead peer
                                   # (laptop sleep, VPN flap) that would otherwise leak fds
                                   # until EMFILE (#792). 0 disables keepalive. Env:
                                   # AI_MEMORY_TCP_KEEPALIVE_SECS
contradiction_band_min = 0.4       # `memory_lint`'s A5 zero-LLM contradiction band
contradiction_band_max = 0.75      # (lower/upper cosine-similarity edge). The band is a
                                   # fixed absolute cosine value, but a single-language or
                                   # single-domain store's background similarity sits well
                                   # above the general-purpose default, so the default band
                                   # ends up measuring domain proximity rather than conflict
                                   # and produces noisy findings — raise `contradiction_band_min`
                                   # for such a store. Must satisfy 0.0 <= min < max <= 1.0.
                                   # Env: AI_MEMORY_CONTRADICTION_BAND_MIN /
                                   # AI_MEMORY_CONTRADICTION_BAND_MAX

# Capture / launch UX (all default-on where noted). Each has an AI_MEMORY_* env
# override (AI_MEMORY_CAPTURE_ASSISTANT / AI_MEMORY_BACKFILL_ON_START /
# AI_MEMORY_RUN_AUTOWIRE).
capture_assistant = false          # server-side opt-in: honor a Claude Code / Codex
                                   # client's sanitized assistant-final-message marker
                                   # on Stop (#196). Client half is baked separately by
                                   # `install-hooks --capture-assistant`.
backfill_on_start = true           # on first SessionStart in a brand-new (empty) project,
                                   # import that project's existing local harness history
                                   # once so hooks-mid-project isn't amnesiac. Only ever
                                   # bootstraps an empty project; hard-capped. `ai-memory
                                   # backfill` runs it by hand.
run_autowire = true                # `ai-memory run <harness>` auto-installs that harness's
                                   # hooks + MCP on first launch if missing (idempotent,
                                   # one-time per harness+version+install location).
                                   # Also `--no-autowire`.
pi_tool_exposure = "direct"        # how the generated Pi extension registers the MCP
                                   # tools: "direct" (declared to the model) or
                                   # "codemode" (reached through Pi's codemode tool).
release_base_url = ""              # override the GitHub releases base URL that `ai-memory
                                   # upgrade` checks and downloads from (#801). Empty =
                                   # https://github.com/akitaonrails/ai-memory/releases.
                                   # For hermetic tests / mirrors, not day-to-day installs.
                                   # Env: AI_MEMORY_RELEASE_BASE_URL.

[run]
require_server = false             # when true, `ai-memory run` refuses to launch if the server
                                   # cannot be reached (also `--require-server` /
                                   # AI_MEMORY_RUN_REQUIRE_SERVER).
exit_prompt = true                 # when false, skip the interactive switch/rerun menu after a
                                   # successful session (also `--no-exit-prompt` /
                                   # AI_MEMORY_RUN_EXIT_PROMPT).

[run.profiles.work.env]            # env-only launch profile for `ai-memory run --profile
CLAUDE_CONFIG_DIR = "/home/me/.claude-work"  # work <harness>` (#922): same reach as
                                   # `--env` (child, native-session lookup, auto-wire),
                                   # layered under `--env-file` and `--env`. Values are
                                   # literal (use absolute paths); names and keys are
                                   # validated at load, and an unknown name fails before
                                   # lease, auto-wire or launch. TOML only: the
                                   # AI_MEMORY_* overlay lowercases env-derived keys.

[maintenance]                      # scheduled server jobs (run outside hook latency)
enabled = true                     # master switch for the scheduled jobs below
forget_sweep_interval_secs = 86400 # retention forget sweep; 0 disables. Cadence persists
                                   # across restarts; overdue work starts after a bounded delay
lint_interval_secs = 86400         # rule-based wiki lint; 0 disables (same persistence)
lint_llm_interval_secs = 0         # lint with the LLM contradiction pass; 0 = off (calls
                                   # the lint provider; skipped with one warning without
                                   # one). While on, the rule run keeps its findings.
embedding_backfill_interval_secs = 0  # embedding backfill; 0 = off (may call a paid provider)
reconcile_tombstones_deleted_pages = false
                                   # opt-in (#929/#964): the 30s reconcile pass soft-tombstones
                                   # (is_latest=0 + superseded_at — never a filesystem write) an
                                   # OKF-imported content page whose file has been missing on
                                   # two consecutive passes, behind a circuit breaker and with
                                   # session pages excluded. OFF = byte-identical to pre-2.5
                                   # behavior (deletions still need `ai-memory delete-page`).
                                   # Docs: docs/okf.md, docs/install.md.

[sessions]                         # server-side session lifecycle
idle_end_after_hours = 0           # 0 (default) = off. N > 0 (capped at 8760):
                                   # the [maintenance] tick ends open sessions whose
                                   # last activity is older than N hours with the tick's
                                   # own effects (summary page attributed to the
                                   # session owner + consolidation job, and the
                                   # lifecycle-only handoff release), never an
                                   # additional handoff from the tick. Bounded batch
                                   # of 25 every 15 minutes; the page write is
                                   # conditional on the version the selection
                                   # captured, the end stamp is an `ended_at IS NULL`
                                   # CAS that also re-verifies the session's
                                   # observation count and the latest page version,
                                   # so a real SessionEnd landing around the tick
                                   # wins that race. `ended_at`
                                   # is the tick's clock; the last activity is kept
                                   # in the audit detail.

[decay]                            # M8 retention params
lambda = 0.02                      # ↓ to forget less aggressively (fallback λ)
sigma = 0.6                        # ↑ to reward query-hits more
mu = 0.04                          # ↑ if recent hits should count more
cold_threshold = 0.20              # below this → remove file + retain tombstone
hard_delete_after_days = 180
breadth_weight = 0.0               # opt-in reward for distinct operators
observation_retention_days = 0     # 0 = never prune raw observations
observation_prune_batch = 5000     # rows per prune transaction
ledger_retention_months = 0        # 0 = never delete hook event ledgers; N > 0 =
                                   # delete a log-YYYY-MM.md N whole months after
                                   # its month ended (current + previous month
                                   # are never deleted)
compact_cold_episodic = false      # A2 opt-in: tier-down (compact) a cold
                                   # episodic page instead of evicting it —
                                   # keep abstract+summary+keep-tokens, drop
                                   # prose. Reversible (git + supersession),
                                   # zero-LLM. false = today's evict behaviour.
dedup_cold_clusters = false        # A3 opt-in: cluster near-duplicate cold
                                   # episodic pages by embedding (cosine DBSCAN,
                                   # adaptive eps) and collapse each cluster to
                                   # one survivor (union of keep-tokens), others
                                   # superseded with a merge note. Reversible
                                   # (git + supersession), zero generative LLM,
                                   # no-op with no embedder. Merge provenance in
                                   # page_evidence. false = no clustering.
# dedup_min_pts = 2                # DBSCAN density floor (0 ⇒ default 2)
# dedup_max_eps = 0.15             # conservative eps ceiling (cosine distance;
                                   # 0 ⇒ default). Lower = merges less.

[decay.half_life_days]             # opt-in per-tier retention curves (all keys
                                   # optional). Half-life in DAYS; converted to
                                   # λ = ln(2)/days. An omitted key falls back to
                                   # the scalar `lambda` above, so the default
                                   # (no keys) is byte-identical to today — no
                                   # score change or mass-eviction on upgrade.
# working = 7                      # e.g. keep scratch short…
# episodic = 365                   # …and session history long
# semantic = 180
# procedural = 90

[slots]                           # optional shared-server injection boundary
per_user = false                  # shared + own slots in agent context

[handoff]                         # optional server-wide handoff policy
claim_on_session_start = true     # false offers metadata; explicit accept claims
create_on_session_end = true      # false stops automatic handoffs at session end and OpenCode/Antigravity turn checkpoints

[profile]                         # cross-project profile (docs/cross-project-profile.md)
enabled = "auto"                  # auto: on for single-user, off for multi-user; true / false
share = "auto"                    # auto | global | workspace | user | off
                                  # auto: global (all workspaces) single-user, user (private) multi-user
min_projects = 2                  # distinct projects a choice must appear in to join the profile
inject_on_session_start = true    # fenced digest after the project brief
digest_max_bytes = 3000           # UTF-8 bytes, clamped 1500..12000
baseline_max_bytes = 6000         # digest budget for a project with no pages yet, clamped 2000..20000
apply_max_lines = 40              # most lines `profile apply` writes into a rules file
max_output_tokens = 2000          # LLM output budget for the profile pass
llm = true                        # use the configured provider when there is one
                                  # env: AI_MEMORY_PROFILE__<KEY>, e.g. AI_MEMORY_PROFILE__SHARE=workspace

[query_answer]                     # optional memory_query answer synthesis admission
# max_context_tokens = 16000       # omitted by default; disabled preserves requests
                                   # one UTF-8 byte = one estimated input token
                                   # includes schema + 1024 envelope units and reserves
                                   # the reasoning tier's output allowance (2000..8000)
                                   # this is NOT a provider tokenizer guarantee

[consolidation]                    # LLM consolidation prompt sizing
verbatim_residue = false           # opt in to bounded literal diagnostics from
                                   # omitted/truncated observations; this widens
                                   # the captured text sent to the LLM provider
                                   # design: docs/design-verbatim-residue.md
max_input_tokens = 100000          # approximate whole-input target; min 8350 at the default margin (scales with it)
                                   # a flat chars-per-token heuristic, so it
                                   # UNDER-budgets denser corpora: pt-BR prose
                                   # and source code tokenize at fewer chars per
                                   # token than English and can overshoot the
                                   # provider's real limit by ~40% — lower this
                                   # (or input_token_safety_margin) for such a corpus
max_output_tokens = 32000          # provider generation limit; min 1000
                                   # their sum must fit the model context window;
                                   # leave headroom for tokenizer variance
input_token_safety_margin = 0.8    # scales the char budget, (0.0, 1.0]; the
                                   # 0.8 default buys pt-BR/code headroom

[auto_improve]                     # default-available learning reviewer
require_approval = false           # true leaves proposals pending for review
min_observations = 8
min_session_duration_secs = 120
min_confidence = 0.75
max_input_tokens = 24000
max_proposals_per_run = 5
max_patchable_pages = 8
patchable_page_prefixes = ["_rules/", "procedures/"]
max_patchable_body_chars = 8000
max_edits_per_proposal = 5
max_edit_content_chars = 4000
max_changed_chars_per_proposal = 12000
max_patch_edits_per_run = 8
max_rejection_context = 50
rejection_context_days = 180
max_final_body_chars = 32000
max_rule_page_tokens = 2000
max_procedure_page_tokens = 2000
include_raw_fallback = false
proposal_actor = "auto_improve"
pending_path = "_pending/auto-improve"

[auto_improve.scheduler]           # background review; separate from approval
enabled = true
interval_secs = 3600
max_sessions_per_tick = 1        # per project; scheduler ticks do not overlap
min_session_age_secs = 600
experience_every_sessions = 0    # 0 disables the cross-session experience pass
experience_sessions = 10         # session summaries one experience pass reads

[auto_improve.scheduler.experience_entropy_filter]  # A4 opt-in; off by default
enabled = false                  # true: skip low-information session pages from
                                 # the experience consolidation pass BEFORE the
                                 # prompt/eval-gate/apply_batch. Advisory (skip,
                                 # never delete); zero-LLM. false = no filtering.
# min_chars = 16                 # near-empty floor (non-whitespace chars)
# min_entropy_bits_per_char = 2.0
# max_repetition_ratio = 0.7     # 1 - distinct/total tokens above this ⇒ skip
# repetition_min_tokens = 6      # repetition check applies only above this

[retrieval]                       # ranking signals; only observation_stream is on by default
query_intent = false              # lexical session-recall routing: queries phrased as
                                  # "上次 / …的会话 / last time / yesterday / última sessão"
                                  # hand session pages back their default kind/tier authority penalty
session_recall_bonus = 0.25       # extra authority on top of the cancelled penalty;
                                  # lower it (e.g. 0.15) if rank drift on
                                  # "之前/上次"-prefixed fact queries matters more
abstract_vectors = false          # fifth RRF stream over page_abstract_embeddings
                                  # (L0: each page's frontmatter `abstract:` line, embedded
                                  # by the same backfill as the body)
belief_authority_weight = 0.0     # fold read-time belief-strength confidence (P2) into page
                                  # authority as ONE bounded factor inside the [0.55, 1.50]
                                  # clamp. 0.0 = OFF (default): ranking is byte-identical and
                                  # no belief query runs. confidence/evidence_count are still
                                  # exposed in explain regardless (inert). DEFAULT OFF,
                                  # R2-gated: do not default on without a positive R2 delta.
observation_stream = true         # R1 observation-evidence stream: FTS over observations_fts
                                  # in the queried project; each matching session votes ONCE
                                  # (best observation) for its own latest sessions/<id>.md
                                  # page as one more RRF stream. Its excerpt is the snippet
                                  # only when page FTS missed that page; explain reports
                                  # `observation_rank` + `observation_ids`. Read-only;
                                  # sessions without a page keep
                                  # the raw_hits fallback; global=true unchanged. DEFAULT ON;
                                  # false restores the previous ranking byte for byte.
                                  # Env: AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=false
# feedback_demotion = 0.0          # subtract this much authority from a current page
#                                  # version carrying open stale/wrong feedback. A rewrite
#                                  # supersedes the version and clears the penalty. 0.0 is off.
# query_signal_weight = 0.0        # boost a page from a prior read-after-query or helpful
#                                  # association, capped by query similarity and distinct
#                                  # sessions. Recorded only for write-authorized,
#                                  # session-aware callers; not_helpful withdraws one and a
#                                  # session purge deletes its rows. 0.0 disables both
#                                  # capture and ranking.

[search.fts]                      # FTS5 query-preparation tuning (contrast with [retrieval]:
                                  # this is not a ranking signal). Omit the whole section for
                                  # byte-identical behaviour on every existing install.
# stopwords = []                  # words dropped from a bare natural-language FTS query
                                  # before the OR-join. Three states:
                                  #   - key absent (the default): the built-in English list
                                  #     (a/an/and/the/…, ~60 entries) — unchanged behaviour.
                                  #   - stopwords = []: disables the filter entirely, English
                                  #     included.
                                  #   - a non-empty list: REPLACES the default outright with
                                  #     exactly those words (does not extend the English list).
                                  # Entries are folded to lowercase with Unicode case folding
                                  # (not ASCII-only — a sentence-initial "É" or all-caps "NÃO"
                                  # still matches a lowercase entry), and a query token is
                                  # compared the same way, but diacritics are never stripped:
                                  # "e" and "é" stay distinct. What matters here is how a query
                                  # is actually TYPED, not how wiki content is spelled — content
                                  # matches through the FTS index's own diacritic-folding
                                  # tokenizer regardless, but this filter only ever sees the
                                  # literal characters someone typed. So a list for an accented
                                  # language should include every spelling a user or agent might
                                  # type, e.g. Portuguese BOTH "e" and "é", BOTH "nao" and "não".
                                  # The filter also matches whitespace-split raw tokens before
                                  # any punctuation handling, so an entry never matches a token
                                  # with attached punctuation ("de," / "que?") — the same
                                  # limitation English stopwords have always had. Explicit FTS5
                                  # syntax (quoted phrases, OR/AND/NOT/NEAR, parens) that FTS5
                                  # accepts bypasses this filter, exactly as it does with the
                                  # built-in list; syntax that does not parse degrades to a
                                  # bare OR-join of its words, with quotes and parens stripped,
                                  # and is filtered like any bare query except for the words
                                  # inside quotes. A bare query made ONLY of configured stopwords
                                  # keeps them all rather than returning nothing. At most 2000
                                  # entries of at most 64 characters each, no internal whitespace
                                  # (entries are matched against single whitespace-split
                                  # tokens); anything past that fails startup.
                                  #
                                  # Env override: AI_MEMORY_SEARCH_FTS_STOPWORDS as a
                                  # comma-separated string — the same convention
                                  # allowed_hosts/cors_allow_origins/auth.trusted_proxy_cidrs
                                  # use for a Vec<String> — but read once as data in
                                  # Config::load rather than through the usual `__`-split env
                                  # layer, because a present-but-blank env var must mean
                                  # "unset" (leave config.toml's value alone), never an
                                  # accidental "disable filtering"; the automatic layer merges
                                  # raw values before any such distinction could be made.
                                  #
                                  # Non-English example — a Portuguese-majority wiki, so its
                                  # own function words (not English's) get filtered, spelling
                                  # out both accented and unaccented forms someone might type:
                                  # stopwords = [
                                  #   "a", "o", "as", "os", "de", "da", "do", "das", "dos",
                                  #   "em", "um", "uma", "uns", "umas", "com", "para", "por",
                                  #   "que", "se", "no", "na", "nos", "nas", "e", "ou",
                                  #   "nao", "não", "voce", "você",
                                  # ]

[dream]                           # B2/B3/B4 opt-in LLM "dream" pass. OFF by default,
                                  # R2-gated before it may default on. Never deletes a source.
enabled = false                   # true starts the scheduled pass — but ONLY if a provider AND
                                  # an embedder are also configured. A provider-less store keeps
                                  # the zero-LLM A3 path untouched (invariant #13).
interval_secs = 3600              # how often the scheduler CONSIDERS a run (0 ⇒ 3600)
idle_window_secs = 300            # operator must be quiet this long before a run starts; returning
                                  # activity CANCELS an in-flight run at the next cluster boundary
                                  # (B3). 0 ⇒ default 300.
# min_pts = 2                     # DBSCAN density floor (0 ⇒ default 2)
# max_eps = 0.15                  # conservative eps ceiling (cosine distance; 0 ⇒ default)
# max_clusters_per_run = 8        # bounded fan-out per run (invariant #5; 0 ⇒ default 8)
# min_cold_pages = 2             # events-accrued gate: skip a run below this many cold pages

[hook_spool]
max_attempts = 8                  # failed drain passes before dropping an event; 0 disables
                                  # only this limit. Age (7 days) and spool size (10,000 files)
                                  # still bound retention. Runtime env wins when set:
                                  # AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS

[ingest]                          # server-side drop list for tool events (#1241)
index_file_paths = false          # opt-in file path index (#1843): store the checkout-relative
                                  # path a kept file-tool post names on its observation.
                                  # Env: AI_MEMORY_INGEST__INDEX_FILE_PATHS
drop_tool_use_id_prefixes = []    # e.g. ["toolu_plugin_"]: drop a tool event whose id starts so
drop_tool_names = []              # exact tool names to drop; both lists apply to every project,
                                  # at most 64 entries of 128 characters
drop_events = []                  # whole hook event kinds to drop (e.g. ["pre-tool-use"]); a
                                  # matching event is acknowledged and discarded before storage,
                                  # all three lists are checked right after the subagent exclusion (a marked
                                  # subagent event still seeds its tail tracking) and before the
                                  # rate limiter, the ingest semaphore and the sanitizer.
                                  # session-start/session-end are rejected (they open/close the
                                  # session state machine); unknown kinds are rejected. Dropping
                                  # a kind also turns off everything that kind's processing
                                  # does: user-prompt loses prompts (summaries, baton open
                                  # questions, profile learning); pre-tool-use loses the
                                  # pre-call evidence, so a session whose only work was a tool
                                  # call that never finished ends lifecycle-only; post-tool-use
                                  # loses results and the completed-call count; stop loses the
                                  # opt-in assistant reply,
                                  # the normal-completion signal and OpenCode's and Antigravity's turn checkpoints;
                                  # pre-compact/post-compaction lose the compaction checkpoint
                                  # and its rewrite job (the session is not ended);
                                  # notification loses the observation;
                                  # subagent-start/subagent-stop lose subagent metadata (stored
                                  # as `other`). user-prompt and pre-tool-use (with
                                  # session-start) advance the project fallback pointer; dropping
                                  # them removes those updates. The portable workstream ledger
                                  # (`ai-memory run`) is not filtered by this key. In production
                                  # ~87% of pre-tool-use observations have an empty body and
                                  # each pairs 1:1 with a post-tool-use that carries the
                                  # content. Empty by default. Env:
                                  # AI_MEMORY_INGEST__DROP_EVENTS=pre-tool-use
record_tool_names = false         # name each closed-tool call: title `tool non-file: Bash`,
                                  # body `tool_name:` plus `skill:` / `subagent_type:` for a
                                  # Claude Code Skill / Agent call. Identifiers only (a name of
                                  # at most 64 [A-Za-z0-9_-], a descriptor of at most 128
                                  # [A-Za-z0-9_.:-]); never arguments. Applied after the capture
                                  # policy, so a metadata-only call stays unnamed. Session pages
                                  # and handoffs then count and list calls by name. Env:
                                  # AI_MEMORY_INGEST__RECORD_TOOL_NAMES=true
```

O equilíbrio entre privacidade e fidelidade por trás de `[consolidation].verbatim_residue`,
incluindo seu resultado opcional medido, está registrado em
[`design-verbatim-residue.md`](design-verbatim-residue.md).

O roteador de `query_intent` sem LLM também reconhece frases explícitas de histórico
em português do Brasil: "o que fizemos na última sessão", "onde paramos ontem" e
"lembre a decisão anterior", além de "sessão anterior".
Grafias com e sem acento são aceitas, incluindo "ultima sessao" e
"decisao anterior". Marcadores latinos correspondem a palavras inteiras. Consultas técnicas como
"erro na sessão do usuário", "sessão expira", "antes de salvar" e as palavras isoladas
"sessão" ou "antes" não disparam o roteamento. Ele permanece desativado por padrão; habilitá-lo
reutiliza o ajuste de autoridade de sessão existente sem alterar as stopwords do FTS
ou adicionar um sinal de ranking.

**Variáveis de ambiente do provedor de LLM** (opcional):
```
AI_MEMORY_LLM_PROVIDER     anthropic | anthropic-oauth | openai | openai-oauth | codex | copilot |
                           cursor | gemini | openai-compat | opencode
AI_MEMORY_LLM_MODEL        optional when the provider has a default; e.g. claude-haiku-4-5, gpt-5.4-mini
ANTHROPIC_API_KEY / OPENAI_API_KEY / GEMINI_API_KEY / LLM_API_KEY
AI_MEMORY_LLM_BASE_URL     required for openai-compat (Ollama, vLLM); optional override for
                           opencode (defaults to the Go endpoint, set
                           https://opencode.ai/zen/v1 for Zen's catalogue).
                           Applies to every HTTP API provider: naming ai-memory
                           is how an operator says a vendor endpoint is proxied
                           on purpose. The ChatGPT backend (openai-oauth, codex)
                           and cursor's CLI have no override. Provider and
                           embedder clients follow a redirect only on the
                           origin they sent to, so set the URL a gateway
                           redirects to rather than one that moves hosts
LLM_BASE_URL               the unprefixed cross-tool convention, accepted for
                           openai-compat and opencode only. Providers with a
                           fixed vendor endpoint (anthropic, openai, gemini,
                           the OAuth backends, copilot) ignore it and log why —
                           an operator's leftover Ollama URL must not silently
                           rewrite every Gemini request into a 404
AI_MEMORY_LLM_COMPAT_STRICT true by default; false disables response_format=json_schema
AI_MEMORY_LLM_COMPAT_DISABLE_THINKING  openai-compat only, false by default; true sends
                           chat_template_kwargs:{"enable_thinking":false} on every
                           request (vLLM / SGLang thinking models). A structured
                           response that stops with finish_reason=length, or a 2xx
                           with empty message.content, is a terminal error
                           (classes truncated-response / empty-content)
AI_MEMORY_LLM_COMPAT_STREAM optional false to request non-streaming openai-compat JSON;
                           omitted by default; true is rejected because SSE is unsupported
AI_MEMORY_LLM_TIMEOUT_SECS  per-request timeout for chat providers; 300 by default
AI_MEMORY_LLM_REQUEST_METADATA_HEADERS  openai-compat only, false by default; true adds
                           x-ai-memory-workload to every chat attempt, plus
                           x-ai-memory-workspace / x-ai-memory-project when the
                           operation reads one scope and x-ai-memory-user when it
                           acts for one username. Unknown values are omitted,
                           never guessed; values are names, never page content
AI_MEMORY_BOOTSTRAP_MAX_CONCURRENT  bootstrap runs in flight at once
                           (`bootstrap_max_concurrent`); 1 by default
AI_MEMORY_LLM_REASONING_EFFORT  optional reasoning/thinking effort
                           (none|minimal|low|medium|high|xhigh|max|ultra|persistent)
                           mapped per provider: OpenAI `reasoning_effort`,
                           OpenRouter `reasoning.effort`, xAI Grok
                           `reasoning_effort`, Anthropic `output_config.effort`,
                           Codex `reasoning.effort`. Gemini and Copilot
                           ignore the key. Host-unsupported values are
                           clamped to each provider's published enum.
AI_MEMORY_LLM_HEADERS      optional extra HTTP headers on every chat request, as
                           comma-separated `Name=Value` (or `Name: Value`) entries;
                           e.g. `x-opencode-session=prod-01,x-opencode-client=ai-memory`.
                           For gateways that require a caller-identifying header.
                           Headers ai-memory sets itself (authorization,
                           content-type, x-api-key, x-goog-api-key,
                           anthropic-version, anthropic-beta, openai-beta,
                           host, content-length) are refused at startup.
                           `x-request-id` and the `x-ai-memory-*` request
                           metadata names are additionally refused at
                           initialization for the `openai-compat` provider,
                           which sends them itself.
                           Values are never logged. A header value cannot
                           contain a comma through the env var — use
                           `llm_headers = [...]` in config.toml for that.
AI_MEMORY_RERANKER         optional `llm`; reranks project/scopes query candidates
AI_MEMORY_RERANK_TIMEOUT_SECS optional rerank budget in seconds (default 20, minimum 1)
COPILOT_GITHUB_TOKEN       optional GitHub token for copilot
AI_MEMORY_CODEX_EXECUTABLE optional Codex executable; defaults to codex on PATH
AI_MEMORY_CURSOR_AGENT     optional Cursor Agent CLI executable for cursor; defaults to agent on PATH
GITHUB_COPILOT_API_TOKEN   optional pre-minted Copilot API token
COPILOT_API_URL            optional Copilot API base URL override
```

**Cadeia ordenada de contingência de LLM** (opcional, somente TOML; consulte
`docs/llm-provider-fallback.md`, #648): o provedor primário acima sempre
executa primeiro; entradas de `[[llm_fallbacks]]` executam apenas após uma falha transitória
(429, 5xx, timeout, erro de conexão), na ordem de declaração, com a
requisição/esquema/id de operação originais preservados a cada tentativa. Uma
falha determinística (4xx diferente de 429, um esquema não compatível, uma
resposta malformada) ainda é interrompida no primeiro candidato, sem loop de
repetição em toda a cadeia.

```toml
llm_provider = "opencode"
llm_model = "mimo-v2.5-free"

[[llm_fallbacks]]
provider = "openai-compat"          # same wire names as llm_provider
model = "poolside/laguna-s-2.1-free"
base_url = "http://127.0.0.1:49375/v1"   # required for openai-compat, as above
api_key_env = "AI_MEMORY_LOCAL_ROUTER_TOKEN"  # env var *name*; the key itself
                                               # never lives in config.toml

[[llm_fallbacks]]
provider = "gemini"
model = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"
```

`api_key_env` é obrigatório para qualquer provedor que necessite de chave de API
(`anthropic`, `openai`, `gemini`, `opencode`; opcional para `openai-compat`,
que pode rodar sem chave). Ele nunca é herdado da variável de ambiente fixa própria
do primário, de modo que uma contingência não pode parecer configurada enquanto
na verdade não resolve nenhuma credencial. É opcional apenas para um provedor com fonte
nativa de credenciais (`openai-oauth`, `codex`, `copilot`, `anthropic-oauth`,
`cursor`), que então compartilha o material de token em nível de processo do primário. Quando
definido, `copilot` e `anthropic-oauth` usam a chave nomeada em seu lugar, e
`openai-oauth`, `codex` e `cursor` a rejeitam no carregamento. `Config::load` valida
cada perfil e resolve sua credencial uma vez, na inicialização: um provedor
ou modelo ausente/vazio, um provedor desconhecido ou uma credencial ausente
falham a inicialização em vez de deixar uma contingência latente que só falha
quando o primário já estiver inativo. Cada candidato carrega seu próprio circuito de 30s
em memória (`ai_memory_llm::fallback::CIRCUIT_COOLDOWN`): uma falha transitória o abre,
um sucesso o fecha e uma reinicialização limpa todo o estado do circuito.
Não há circuito durável ou tempo limite forçado para toda a cadeia.

**Cadeias de provedores por carga de trabalho** (opcional, `[llm.<task>]`, #1969): uma tabela
de tarefa (`consolidation`, `bootstrap`, `auto_improve`, `experience`, `dream`,
`lint`, `profile`, `rerank`, `query_answer`) constrói sua própria cadeia primária e de
`fallbacks` na inicialização com as mesmas regras de validação e credenciais.
`ai_memory_llm::LlmProviders::get(workload)` retorna essa cadeia ou, para uma tarefa
sem tabela, a cadeia padrão compartilhada. Uma cadeia de tarefa nunca anexa as contingências
globais nem herda o endpoint global, e uma tabela de tarefas funciona sem provedor global
sem habilitar as outras tarefas. A saúde relata cada cadeia de tarefas sob
`llm_task_chains`.

`GET /admin/status` (`ai-memory status`) relata uma lista `llm_candidates`
ao lado dos papéis existentes de `llm`/`embedding`: o rótulo de provedor/modelo de cada candidato,
se ele respondeu à chamada concluída mais recentemente, seu último timestamp de sucesso/erro,
uma classe de erro com dados sensíveis removidos + status HTTP (nunca um corpo de resposta ou credencial),
e seu timestamp de circuito-aberto-até. Fica vazio para uma configuração simples de provedor único;
os campos de nível superior do papel `llm` permanecem
inalterados.

A mesma resposta relata `llm_workloads`, um diagnóstico de ordem fixa e cardinalidade
fixa descrito em [Telemetria de cargas de trabalho de LLM](#telemetria-de-cargas-de-trabalho-de-llm).
Use `ai-memory status --json` para inspecionar as linhas; a saída comum de status
para humanos permanece concisa.

Toda requisição de chat carrega `User-Agent: ai-memory/<version>`
(`ai_memory_llm::DEFAULT_USER_AGENT`, colocado em camadas em `build_provider`). O `reqwest`
não envia nenhum user agent a menos que configurado, portanto, sem esse cabeçalho as requisições
do provedor chegam anônimas, e gateways que exigem que os chamadores se identifiquem
as relatam como um cliente desconhecido. `AI_MEMORY_LLM_HEADERS=user-agent=...` o substitui. O
provedor Copilot mantém `GitHubCopilotChat/<version>` em vez disso, o
agente de plugin do editor esperado pela API Copilot do GitHub.

`openai-oauth` usa `auth login openai-oauth` e armazena o token de atualização
do ChatGPT/Codex em `<data_dir>/auth.json`; ele é separado da autenticação bearer
do MCP/servidor e das chaves de API da OpenAI Platform.

O `codex` lê apenas o token de acesso e o id da conta a partir do `auth.json` pertencente
à CLI do Codex, resolvido a partir de `CODEX_HOME` ou da home da plataforma. Ele nunca persiste
credenciais do Codex. Uma única recuperação de 401 é serializada e delegada ao
`codex app-server --stdio`, com JSONL/stdout/stderr limitados e um tempo limite máximo de
recuperação de 30 segundos.

O `copilot` usa `auth login copilot` ou `COPILOT_GITHUB_TOKEN`, troca o token
do GitHub por meio de `/copilot_internal/v2/token` e chama o Copilot Chat com
os cabeçalhos de integração `vscode-chat`. O token bruto do GitHub não é enviado para o
endpoint de chat do Copilot.

**Env do incorporador** (opcional):
```
AI_MEMORY_EMBEDDING_PROVIDER   openai | voyage | google | gemini | openai-compat | copilot
AI_MEMORY_EMBEDDING_MODEL      e.g. text-embedding-3-small, gemini-embedding-001
AI_MEMORY_EMBEDDING_BASE_URL   optional override; required for openai-compat
AI_MEMORY_EMBEDDING_DIM        1536 (OpenAI, Copilot), 1024 (Voyage), 768 (Google);
                               required explicitly for openai-compat
AI_MEMORY_EMBEDDING_QUERY_PREFIX     optional; prepended to query text before
                                     embedding (openai / openai-compat only)
AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX  optional; prepended to document text
                                     before embedding (openai / openai-compat
                                     only); e.g. "query: " / "passage: " for
                                     Nemotron-3-Embed / base E5
OPENAI_API_KEY / VOYAGE_API_KEY / GEMINI_API_KEY / GOOGLE_API_KEY
LLM_API_KEY                    accepted for openai with a custom base URL and as
                               optional bearer auth for openai-compat
EMBEDDING_API_KEY              optional embedding-only key; checked before
                               OPENAI_API_KEY and LLM_API_KEY for the openai and
                               openai-compat embedders
```

`EMBEDDING_API_KEY` credencia apenas a função de incorporação, para que o incorporador possa
mirar em um provedor diferente do modelo de chat: `openai` em `api.openai.com`
para o LLM, um endpoint mais barato ou auto-hospedado compatível com OpenAI para vetores.
Sem ela, o incorporador `openai` usa `OPENAI_API_KEY` e depois `LLM_API_KEY`
quando uma URL base de incorporação personalizada está configurada, exatamente como antes. `voyage` e
`google`/`gemini` continuam lendo apenas suas próprias `VOYAGE_API_KEY` e
`GEMINI_API_KEY`/`GOOGLE_API_KEY`.

`openai-compat` também requer um modelo explícito porque motores auto-hospedados não têm
nenhum padrão seguro compartilhado de modelo ou dimensionalidade. Ele não envia cabeçalho de autorização
quando tanto `EMBEDDING_API_KEY` quanto `LLM_API_KEY` estão ausentes e armazena vetores
sob a identidade distinta `provider="openai-compat"`.

O `copilot` não aceita nenhuma chave de API: ele resolve o mesmo `CopilotAuth` que o provedor
LLM `copilot` (`auth login copilot`, `COPILOT_GITHUB_TOKEN` ou `GITHUB_COPILOT_API_TOKEN`)
e compartilha sua troca de token do GitHub -> token de curta duração da API do Copilot
(`CopilotAuthState` em `ai-memory-llm::copilot`) em vez de um caminho de troca separado.
Ele usa como padrão `text-embedding-3-small` / dimensão 1536 e chama o endpoint
`/embeddings` do Copilot com os mesmos cabeçalhos de integração `vscode-chat` do chat. O formato de
transmissão desse endpoint segue o contrato compatível com OpenAI documentado pelo Copilot para
chat, não uma especificação publicada de incorporações, e não é coberto por um teste
ao vivo contra o Copilot aqui.

### `[auth]` root bearer

```toml
[auth]
bearer_token_file = "auth-tokens/default"   # written by `init` on a fresh data dir
                                            # and by `ai-memory auth enable`
# bearer_token = "..."                      # inline alternative; never both
```

O bearer raiz vem, em ordem, de: `AI_MEMORY_AUTH_TOKEN`, depois de
`[auth].bearer_token` ou `[auth].bearer_token_file` (definir ambos é um erro
de inicialização). `Config::load` lê o arquivo uma vez. Um caminho relativo é resolvido em
relação ao diretório de dados, de modo que um contêiner que o monta em `/data` e um cliente host
que enxerga o caminho do host leiam o mesmo arquivo. Um arquivo configurado que esteja ausente ou
em branco não deixa nenhum bearer, e o `serve` então se recusa a iniciar. Variável de ambiente:
`AI_MEMORY_AUTH__BEARER_TOKEN_FILE`.

O `ai-memory init` gera o token (32 bytes aleatórios, hexadecimal, `0600` sob um `auth-tokens/`
com permissão `0700`) apenas em um diretório de dados novo: nenhum arquivo de configuração e nenhum
banco de dados ainda, e nenhum bearer fornecido por outro meio. Uma instalação existente mantém o
comportamento de loopback sem autenticação até o `ai-memory auth enable`, que é idempotente: ele reutiliza
ou gera o arquivo, registra a chave, copia o token para os arquivos de credenciais do hook
(`auth-token`, `auth-header`) quando nenhum estiver armazenado e registra novamente
cada cliente MCP cuja entrada `ai-memory` tenha este servidor como destino, da forma como o
`install-mcp` conecta uma instalação de arquivo de token, listando o que alterou e o que
precisa de alteração manual.

Um token lido do arquivo nunca é escrito em uma configuração de harness ou em uma
linha de comando. O `install-mcp` fornece ao Claude Code a entrada stdio `ai-memory mcp-bridge`
com reconhecimento de sessão, que lê o próprio arquivo, e o Meta Muse
Code sempre possui uma (`--agent muse`, com o `--data-dir` da instalação porque o
Muse limpa o ambiente de seus servidores stdio); Codex,
Prime Agent e Factory Droid fazem referência ao `AI_MEMORY_AUTH_TOKEN` pelo nome; todos os outros clientes recebem
uma entrada stdio, em seu próprio formato de configuração, para `ai-memory mcp-bridge
--no-session-id`. O ZCode mantém apenas entradas HTTP e é recusado.
`install-hooks --apply` o persiste nos arquivos de credenciais do hook e
nunca o coloca em linha. Comandos de cliente de uma instalação empacotada, cuja configuração reside
fora do diretório de dados e, portanto, não é lida sem `--config`, usam
`<data_dir>/auth-tokens/default` quando nenhum arquivo de configuração existe onde procuram,
a URL do servidor é de loopback e nenhum perfil de servidor `default` está registrado.
`/healthz` permanece sem autenticação.

## Trabalhos futuros

* **Integração com `sqlite-vec`.** O cosseno por força bruta funciona bem até alguns
  milhares de páginas; além disso, a extensão `sqlite-vec` é o próximo
  passo. Consulte [`docs/vector-backend-policy.md`](vector-backend-policy.md)
  para conhecer os critérios que devem justificar sua adição.
* **Ações mais ricas do curador.** O curador fornecido prepara apenas uma página de relatório;
  trabalhos futuros podem adicionar propostas individuais de mesclagem/substituição/correção de links,
  enquanto mantêm exclusões e reescritas semânticas condicionadas a revisão.
* **Superfícies de leitura mais ricas para a UI web.** O navegador wiki somente leitura
  multi-espaço de trabalho fornecido em `ai-memory-web` (`/web`: lista de projetos, árvore
  de páginas, visualização de páginas, histórico de páginas com um diff unificado no lado do servidor, uma linha
  do tempo de sessões paginada, um grafo de links renderizado no servidor em `/web/graph` e
  `/web/w/<workspace>/<project>/graph`, busca de projetos por meio do classificador
  híbrido do `memory_query`
  com rastreamento de revocação, o painel `/web/health` exclusivo da raiz, a visão geral
  de workstreams gerenciados exclusiva da raiz em `/web/workstreams` e a página de triagem
  `/web/pending` exclusiva da raiz,
  cujos botões de aprovação e rejeição realizam POST para as rotas existentes
  `/admin/pending-writes/*`). Ele permanece somente leitura por design: a wiki é um
  registro de autoria de máquina, e uma superfície de edição no navegador violaria o
  invariante sobre o qual todo o armazenamento se apoia (#482). O `clippy.toml` do crate proíbe
  os métodos de escrita e destrutivos de `Wiki` ali, de modo que um caminho de escrita falha em
  `cargo clippy -D warnings`. O histórico lê todas as versões em um caminho (`ReaderPool::page_versions`,
  `page_version_body`, ambos indexados por espaço de trabalho, projeto e caminho), para que uma versão
  substituída por uma escrita concorrente permaneça visível (invariante #16). A expiração por TTL ainda
  se aplica: uma versão expirada não é listada nem legível por id, e uma página
  cuja versão ativa expirou não exibe histórico, já que a varredura de esquecimento exclui
  todas as linhas em seu caminho. O grafo de links é um SVG circular mais uma tabela de arestas;
  layouts maiores e agrupamento interativo continuam em aberto. Consulte
  [`docs/frontend-api.md`](frontend-api.md#10-lacunas-conhecidas-e-não-objetivos-deliberados).
* **Lacunas na medição de recuperação.** O harness LongMemEval-S é fornecido em
  `evals/` (modos de pipeline, componente e decomposição; resultados em
  [`docs/benchmarks/`](benchmarks/README.md)), ao lado do framework de avaliação
  de revocação
  ([`crates/ai-memory-consolidate/tests/suite/recall_eval.rs`](../../crates/ai-memory-consolidate/tests/suite/recall_eval.rs)).
  Ainda não medido: `belief_authority_weight`, que necessita de um
  corpus consolidado por LLM porque páginas de sessão sem LLM não carregam
  `page_evidence`.

## Ordem de leitura

* Este arquivo: resumo operacional, você está aqui.
* [`docs/design-decisions.md`](design-decisions.md): a especificação completa da v1.
* [`docs/research-karpathy-llm-wiki.md`](research-karpathy-llm-wiki.md):
  o que "fiel a Karpathy" significa.
* [`docs/research-agentmemory.md`](research-agentmemory.md),
  [`research-basic-memory.md`](research-basic-memory.md),
  [`research-cognee.md`](research-cognee.md),
  [`research-ecc.md`](research-ecc.md),
  [`research-codebase-memory-mcp.md`](research-codebase-memory-mcp.md):
  trabalhos anteriores estudados.
* [`docs/auto-improvement-loop.md`](auto-improvement-loop.md):
  pesquisa do ciclo de aprendizagem inspirado no Hermes Agent e limites de segurança.
* [`docs/issues-*.md`](.): modos de falha concretos que projetamos para
  evitar.
* [`CLAUDE.md`](../../CLAUDE.md): regras operacionais por sessão fixadas
  em conversas do Claude Code.
