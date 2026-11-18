# Autoaprimoramento

## A versão de 60 segundos (leia primeiro)

Quando um provedor de LLM está configurado, o servidor revisa periodicamente
sessões recém-concluídas e propõe pequenas edições na wiki, como
um novo gotcha, a promoção de uma regra ou uma correção em uma página de conceito obsoleta. Toda proposta é
validada (esquema, patamar mínimo de confiança, limites de tamanho, portal de avaliação) e preparada (staged)
em `_pending/auto-improve/` com um arquivo auxiliar (sidecar) legível por humanos, e depois
**aprovada automaticamente por padrão** através do caminho normal de escrita da wiki.

O que isso significa para você:

- **Instalação individual, sem LLM**: isso nunca é executado. Nada a decidir.
- **Individual com LLM**: o padrão é adequado. As propostas são pequenas,
  delimitadas, auditadas (cada uma fica registrada no histórico de gravações pendentes com
  citações de evidências e uma pontuação de confiança) e reversíveis pelo histórico
  da wiki. Execute `ai-memory pending-writes list` por uma semana se quiser
  adquirir confiança; essa listagem (e o SQLite) é a fonte da verdade para o status
  de uma proposta. Os arquivos sidecar em `_pending/auto-improve/` são um snapshot
  legível por humanos congelado no momento da preparação; eles não acompanham uma proposta
  até ela ser aprovada/aplicada/rejeitada.
- **Servidor compartilhado / de equipe**: defina `[auto_improve] require_approval =
  true`. Em um servidor no qual várias pessoas confiam, um LLM não deve
  aplicar automaticamente edições que ninguém revisou. As propostas então aguardam em
  `_pending/` até que um humano aprove cada uma.
- **Custo**: uma chamada de LLM delimitada por sessão concluída que passe pelos
  filtros preliminares (mínimo de observações/duração); os tamanhos de prompt
  e de saída são delimitados em `[auto_improve]`.

O sidecar de uma proposta preparada típica se parece com: o caminho de destino, a
operação (criar ou corrigir), uma confiança como `0.86`, a justificativa,
citações delimitadas de evidências da sessão e o corpo ou as edições exatas.
Isso é suficiente para aprovar ou rejeitar sem abrir qualquer outra coisa.

O restante desta página contém a pesquisa de design original acrescida das
notas de implementação, mantidas para aprofundamento.

---

> Status: pesquisa mais notas de implementação em produção. O servidor agenda
> o autoaprimoramento para sessões recém-concluídas em cada projeto quando um provedor de LLM está
> configurado. O autoaprimoramento manual via CLI/admin/MCP continua disponível para
> execuções direcionadas e atualização de pendências. Ambos os caminhos registram propostas validadas na
> trilha de auditoria de gravações pendentes e as aprovam automaticamente pelo caminho normal de escrita da wiki
> por padrão. Administradores podem definir `[auto_improve] require_approval = true` para
> revisão manual.

## Resumo Executivo

Vale a pena entregar um equivalente ao loop de autoaperfeiçoamento do Hermes Agent no ai-memory
como um caminho de preparação disponível por padrão e sujeito a revisão. A wiki atual
já captura conhecimento durável útil: decisões, gotchas, conceitos, regras,
notas e resumos de sessões. O que falta é um revisor cuidadoso que consiga identificar lições duráveis e aplicar pequenas correções na wiki
através do caminho de revisão/auditoria existente sem alterar o contexto ativo do agente
ou promover silenciosamente resíduos fracos de sessões a regras.

O formato de produto seguro é:

1. Manter a captura automática de observações e a consolidação de sessões como estão.
2. Adicionar uma etapa de revisão em segundo plano que crie edições pendentes na wiki para sessões
   recém-concluídas quando um provedor de LLM estiver configurado.
3. Registrar propostas e aplicá-las pelo caminho de aprovação/auditoria por padrão,
   com aprovação manual disponível como opção do administrador.
4. Manter uma etapa lenta e separada de manutenção para desduplicação, revisão de páginas obsoletas
   e limpeza de ciclo de vida.

Não copie diretamente o sistema de skills locais de agentes do Hermes. A unidade durável do ai-memory
é a página da wiki do projeto, não um pacote `SKILL.md`. Os alvos análogos são
`gotchas/`, `decisions/`, `concepts/`, `procedures/`, `_rules/`, pequenas
páginas de estado em `_slots/`, e páginas de revisão pendente em `_pending/`. As Agent Skills
gerenciadas do ai-memory instaladas com o roteamento são uma exceção restrita de empacotamento de prompt:
arquivos estáticos que ensinam aos agentes quando chamar ferramentas MCP, e não páginas de memória durável
ou saídas de autoaprimoramento.

## Conclusões sobre o Hermes

O Hermes possui dois loops de aprendizado distintos.

### Revisão Pós-Turno

O loop imediato roda após um turno bem-sucedido quando contadores de cadência disparam.
A revisão de memória baseia-se em turnos de usuário. A revisão de skills baseia-se em iterações
de chamadas de ferramentas. Os padrões observados são 10 turnos de usuário para revisão de memória e 10
iterações de ferramentas para revisão de skills.

Propriedades importantes de implementação:

| Propriedade | Comportamento no Hermes | Lição para o ai-memory |
|---|---|---|
| Contexto ativo | A revisão roda após a entrega da resposta. | Nunca competir com a tarefa ativa do usuário. |
| Mutação de prompt | Gravações no meio da sessão atualizam o disco, mas não alteram o prompt ativo em cache. | O aprendizado em segundo plano não deve reescrever o contexto atual do agente. |
| Herança em tempo de execução | A bifurcação herda provedor, modelo, autenticação, prompt do sistema em cache, id de sessão e configuração de ferramentas. | Evitar desvios de modelo/cache ao gerar trabalho auxiliar de revisão. |
| Restrição de ferramentas | A bifurcação mantém o esquema de ferramentas original para paridade de cache de prefixo e impõe uma lista permitida em tempo de execução para ferramentas de memória e skills. | A segurança deve ser aplicada mecanicamente no momento do despacho/gravação, não apenas pelo texto do prompt. |
| Plugins de memória externa | A bifurcação é criada com `skip_memory=True` para não pré-carregar/sincronizar provedores externos. | Não permitir que o harness de revisão polua sistemas de memória independentes. |
| Aprovações perigosas | A revisão em segundo plano instala um callback de aprovação que nega automaticamente. | Uma revisão em daemon nunca deve travar em um prompt interativo. |
| Compressão | A compressão da revisão fica desativada. | A revisão não deve concorrer com o ciclo de vida da sessão principal. |
| Visibilidade | Ações de revisão bem-sucedidas são resumidas para o usuário como saída de revisão de autoaperfeiçoamento. | Alterações autônomas na memória necessitam de proveniência explícita. |

Os prompts pós-turno são intencionalmente agressivos quanto à captura de procedimentos
reutilizáveis, correções de usuários e técnicas não triviais. Eles também incluem
filtros negativos: não registrar falhas transitórias de configuração, alegações negativas de que uma
ferramenta está quebrada, narrativas de tarefas pontuais ou falhas que foram resolvidas antes de
a conversa terminar.

### Aprovação de Escrita

O Hermes possui uma barreira opcional de aprovação de escrita para memória persistente e skills.
O padrão é desativado, preservando o comportamento existente. Quando ativado:

| Caso | Comportamento |
|---|---|
| Memória em primeiro plano, CLI interativa | Solicita confirmação inline quando possível. |
| Memória em primeiro plano, sem canal de prompt | Prepara no armazenamento pendente. |
| Memória em segundo plano | Prepara no armazenamento pendente. |
| Gravações de skills do Hermes | Sempre prepara, pois arquivos de skills podem ser grandes. |
| Usuário nega gravação inline de memória | Bloqueia, não prepara. |
| O mecanismo de prompt falha | Prepara em vez de descartar silenciosamente a gravação. |

A validação roda antes da preparação, portanto gravações inválidas são rejeitadas imediatamente
em vez de serem enfileiradas para aprovação e falharem mais tarde.

Isso se mapeia muito de perto ao ai-memory. Edições na wiki são mais próximas de skills do Hermes do que de
pequenas entradas de memória: podem ser grandes, duráveis e moldar o projeto. A preparação (staging)
deve ser o padrão para gravações autônomas de aprendizado no ai-memory.

### Curador

O curador mais lento do Hermes é um loop de manutenção para skills criadas por agentes. Ele é
disparado por inatividade em vez de um daemon de cron. Padrões observados no código e
na documentação:

| Configuração | Padrão |
|---|---:|
| `interval_hours` | 168 horas, 7 dias |
| `min_idle_hours` | 2 horas |
| `stale_after_days` | 30 dias |
| `archive_after_days` | 90 dias |

O comportamento na primeira execução é deliberadamente conservador. Uma nova instalação define
`last_run_at` e adia a primeira execução real por um intervalo completo. Os usuários podem rodar
um relatório manual primeiro.

Propriedades importantes do curador:

| Propriedade | Comportamento no Hermes | Lição para o ai-memory |
|---|---|---|
| Escopo gerenciado | Primariamente skills criadas por agentes, rastreadas em `.usage.json`. | Separar páginas de autoria do usuário de páginas autônomas. |
| Limite destrutivo | Arquivar é a ação destrutiva automática máxima. Sem exclusão automática. | Preferir substituição ou exclusão suave (soft delete). |
| Objetos fixados | Skills fixadas ignoram transições automáticas. | Páginas fixadas e slots invariantes devem ser protegidos. |
| Relatórios | Grava `run.json` legível por máquina e `REPORT.md` legível por humanos. | Toda execução de manutenção deve deixar um artefato de auditoria. |
| Backups | Cria snapshots antes de execuções com mutações e oferece suporte a reversão. | Commits git na wiki ajudam, mas relatórios de aprovação ainda devem ser explícitos. |
| Modo relatório | Produz saída apenas de relatório. | Relatórios não destrutivos devem ser cidadãos de primeira classe para manutenção. |
| Consolidação | Mescla skills restritas em termos abrangentes com resumo estruturado. | O ai-memory deve consolidar páginas duplicadas/restritas separadamente da captura de lições recentes. |

## Conclusões sobre a Wiki do ai-memory em Produção

A wiki do homelab implantado foi amostrada em 15-06-2026. No momento da amostragem,
ela continha 1 espaço de trabalho, 38 projetos e 204 páginas mais recentes.

Distribuição de páginas por prefixo de caminho:

| Prefixo | Contagem | Avaliação |
|---|---:|---|
| `sessions/` | 57 | Histórico episódico útil, mas também a principal fonte de ruído. |
| `gotchas/` | 40 | Sinal alto; modos de falha e correções concretas. |
| `decisions/` | 37 | Sinal alto; justificativas duráveis. |
| `concepts/` | 36 | Sinal alto; conhecimento de arquitetura e de domínio. |
| `notes/` | 17 | Misto; inclui fatos úteis e marcadores de testes preliminares (smoke). |
| `_rules/` | 8 | Sinal alto quando concisas e atuais. |
| `bootstrap.md` | 7 | Resumo inicial útil. |
| `_slots/` | 2 | Útil para estado atual, mas o risco de obsolescência é real. |

Páginas representativas de alto sinal:

| Página | Por que é útil |
|---|---|
| `ai-memory/gotchas/cli-is-always-http-client.md` | Captura uma regra de arquitetura durável, por que existe, exceções e modos de falha anteriores. |
| `ai-memory/concepts/karpathy-wiki-pattern.md` | Explica o modelo conceitual por trás do produto. |
| `.config/notes/marvin-server-nfs-drop-rootcause.md` | Causa raiz e correção concretas com detalhes suficientes para evitar redescoberta. |
| `nes-to-sms/gotchas/vram-budget.md` | Restrição específica de domínio que importará em trabalhos futuros. |
| `akitaonrails-hugo/decisions/blog-content-sourcing.md` | Decisão curta com justificativa e orientações de implementação. |
| `llm-coding-benchmark/gotchas/hallucinated-apis.md` | Gotcha detalhado e verificado, com exemplos e correções. |

Páginas representativas de baixo sinal:

| Página | Por que não deve ser promovida |
|---|---|
| `.config/sessions/0b9f6071-...md` | Sessão de três segundos sem nenhuma atividade. |
| `.config/sessions/1f8ffad8-...md` | Teste único com `echo claude-bash-ok` sem saída capturada. |
| `.config/sessions/cf81e9c3-...md` | Tentativas repetidas de smoke em bash; útil apenas como histórico de diagnóstico. |
| `.config/sessions/914f9f80-...md` | Prompt do usuário foi apenas `config`; nenhum trabalho substantivo. |
| `sabadell/sessions/8feda9e6-...md` | Página heurística de fim de sessão com uma única observação. |
| `ai-memory/notes/livetest-v011-release.md` | Marcador válido de smoke de release, mas não uma lição geral. |

O sistema atual já está criando as famílias corretas de páginas duráveis. A
oportunidade de autoaprimoramento é, portanto, a promoção e a limpeza seletivas, e não um
novo substrato de memória.

## Recomendação

Construir o autoaprimoramento como um caminho de revisão agendado e disponível por padrão que registra
a proveniência das propostas antes de gravar as páginas de destino na wiki. Execuções manuais via CLI/admin/MCP
usam o mesmo pipeline para revisão direcionada ou atualização histórica. O agendamento e a
aprovação são separados: o agendador decide quando revisar sessões recém-concluídas,
e a política de aprovação decide se as propostas validadas são aplicadas
imediatamente ou mantidas pendentes para humanos.

O recurso entregue é um revisor de aprendizado baseado em auditoria:

1. O autoaprimoramento agendado fica habilitado quando há um provedor de LLM configurado e
   `[auto_improve.scheduler] enabled = true`. Execuções manuais não afetam
   o agendador.
2. Lê observações/páginas de sessão armazenadas, páginas recentes e páginas existentes relevantes
   da wiki. As exclusões de captura ficam a montante do armazenamento: conteúdo excluído
   não pode ser recuperado por `include_raw_fallback` porque nunca foi armazenado.
3. Produz uma proposta estruturada contendo pequenas criações ou atualizações de páginas.
4. Armazena a proposta em uma fila de revisão pendente com evidências e diffs.
5. Aplica propostas aprovadas através de `Wiki::apply_batch`, webhooks de admissão,
    permissões de autenticação, registro em log de auditoria e o ator gravador único.

A autoaprovação é o padrão, mas ainda assim registra propostas preparadas e as aplica
através do mesmo caminho de aprovação. Administradores que desejarem uma fila humana definem
`[auto_improve] require_approval = true`; administradores que não quiserem nenhuma revisão automática definem
`[auto_improve.scheduler] enabled = false`.

Alvos de alto impacto também podem ser protegidos por um portal de avaliação (eval gate) executável fornecido pelo operador.
O `[auto_improve.eval]` vem desativado por padrão; quando ativado, propostas cujos
caminhos correspondam aos prefixos configurados (padrão `_rules` e `procedures`) são enviadas
ao comando configurado após a validação da revisão de LLM e antes da preparação ou
autoaprovação. O comando recebe JSON no stdin com metadados da proposta mais
os corpos antes/depois e deve retornar JSON no formato
`{ "score_before": 0.72, "score_after": 0.76, "passed": true }`. Erros no comando,
tempos limite esgotados, JSON inválido, `passed = false`, ausência de `passed` ou variações de pontuação abaixo de
`min_delta` rejeitam apenas a proposta visada em questão. Propostas não visadas ignoram
o portal. Se todas as propostas falharem na avaliação, a execução ainda é preparada com zero propostas e
os candidatos rejeitados, de modo que o buffer de rejeição possa se lembrar da tentativa com falha.
O servidor gerencia essa política executável: nem a CLI nem uma requisição direta de admin
podem substituí-la. Hooks nunca executam o comando de avaliação. Consulte
[`auto-improve-eval-gates.md`](auto-improve-eval-gates.md) para o contrato completo de stdin /
stdout e scripts de pontuação de exemplo.

A mesma execução também produz `eval_results` estruturados no relatório de revisão
e no JSON de configuração da execução preparada. Propostas aceitas transportam o `eval_result` correspondente
nas evidências do seu sidecar pendente. Os registros incluem valores de `eval_id` gerados pelo servidor
distintos dos IDs de execução de revisão, sucesso/rejeição/falha/tempo esgotado observados,
digests de requisição/corpo, motivos higienizados e delimitados e um digest
de invocação do verificador. O digest de invocação identifica a configuração em vez dos
conteúdos executáveis; relatórios externos continuam sendo evidências atribuídas. Campos ausentes
em relatórios/sidecars mais antigos são aceitos. A desserialização de relatórios descarta resultados de avaliação
em vez de confiar em observações fornecidas. Veja o documento sobre portais de avaliação acima para
os limites exatos de digest e proveniência.

## Destinos de Página Propostos

| Destino | Usar para | Observações |
|---|---|---|
| `gotchas/<topic>.md` | Armadilhas reproduzíveis, causas raiz, peculiaridades de ferramentas, abordagens com falha com correção duradoura. | Exige evidências e uma correção ou mitigação. |
| `decisions/<topic>.md` | Escolhas que alteraram arquitetura, fluxo de trabalho, dependências, implantação ou políticas. | Inclui decisão, justificativa, consequências. |
| `concepts/<topic>.md` | Conhecimento estável sobre o domínio ou arquitetura do projeto. | Preferir síntese em vez de cronologia da tarefa. |
| `procedures/<topic>.md` | Fluxos de trabalho reutilizáveis, procedimentos operacionais e padrões repetidos de várias etapas. | Usar quando o valor estiver na sequência, não apenas na causa raiz ou justificativa. |
| `_rules/<topic>.md` | Instruções explícitas de sempre/nunca para agentes futuros. | Também deve acionar a dica de lint existente para atualizar `AGENTS.md` ou `CLAUDE.md`. |
| `_slots/current-focus.md` | Estado mutável do projeto a curto prazo. | Tratar como estado, não verdade durável; sobrescrever em vez de anexar. |
| `notes/<topic>.md` | Fatos úteis que não se encaixam nas categorias acima. | Evitar usar notas como depósito indiferenciado. |
| `_pending/auto-improve/<id>.md` | Propostas preparadas e diffs legíveis por humanos. | Este é o armazenamento de propostas, não conhecimento durável aprovado. |

Não crie novas páginas de sessão a partir do loop de autoaprimoramento. Páginas de sessão
já se originam da consolidação de fim de sessão.

## Correções de Usuários

Um usuário que rejeita, corrige, interrompe ou desfaz o trabalho do agente e diz
o porquê apontou exatamente o erro que o próximo agente tende a repetir. O
revisor transforma isso em uma página. Uma preferência permanente ou proibição vai para
`_rules/`, como uma correção quando uma página em `_rules/` sobre o tópico já existir. Uma
abordagem que falhou por motivo técnico vai para `gotchas/`. Em ambos os casos, a
página informa o que evitar, o motivo do usuário e o que fazer em vez disso, e suas
evidências citam o usuário literalmente.

O revisor é instruído a reconhecer uma correção pelo seu significado, não por uma
lista de palavras-chave, de forma que o idioma em que o usuário escreve não importa. Apenas
as palavras do próprio usuário contam, e estas vêm de observações de `user-prompt`. Texto
do agente, saída de ferramentas, conteúdo de arquivos e mensagens de commit nunca contam, mesmo quando
afirmam citar o usuário, e o agente corrigindo a si mesmo também não conta.
Uma recusa sem motivo é ignorada, assim como uma escolha pontual,
como uma simples renomeação.

Limites:

- Instalações sem LLM nunca executam o revisor, portanto nenhuma correção é aprendida lá.
- Com o padrão `require_approval = false`, uma correção mal interpretada que
  passe pela validação (`min_confidence` e o portal de avaliação opcional) é gravada
  diretamente em `_rules/`, e um projeto que ativa o resumo de inicialização o carrega
  em cada nova sessão até que alguém o reverta. Servidores compartilhados devem
  manter `require_approval = true`.
- Uma página aprovada automaticamente não alimenta o perfil interprojetos; uma
  página aprovada por humanos sim, em qualquer idioma. A leitura de prompts feita pelo próprio
  perfil ainda pré-filtra frases com marcadores em inglês e português.

## Filtros Negativos

O prompt de revisão deve rejeitar explicitamente estes itens como aprendizado durável:

| Filtro | Por quê |
|---|---|
| Sessões sem atividade | Adicionam ruído à recuperação. |
| Testes de smoke de comando único | Geralmente evidência operacional, não conhecimento reutilizável. |
| Marcadores de lançamento (release) | Mantenha como notas se necessário, mas não promova a regras/gotchas. |
| Binários ausentes, credenciais transitórias ou estado de configuração | Tornam-se restrições falsas e obsoletas. |
| Alegações negativas genéricas sobre ferramentas | `tool X is broken` solidifica-se em recusas futuras depois que a ferramenta for corrigida. |
| Narrativas de tarefas pontuais | Páginas de sessão já preservam a cronologia. |
| Correções sem motivo que se generalize | Um simples "não" ou uma única renomeação é detalhe de tarefa, não uma regra. |
| Falhas transitórias resolvidas | Capture o padrão de repetição ou correção, não a falha temporária. |
| Status visível apenas para o usuário | Use handoff ou `_slots/current-focus.md`, não páginas semânticas duráveis. |

## Invariantes de Segurança

Qualquer implementação deve preservar estas invariantes:

1. Execuções agendadas e manuais via CLI/admin/MCP registram propostas e autoaprovam por
   padrão.
2. O agendamento e a aprovação permanecem separados: desativar o agendador não
   exige aprovações manuais, e exigir aprovações não interrompe o agendamento.
3. O disparo automático em SessionEnd fica desativado por padrão.
4. Nunca alterar o prompt da sessão ativa nem o contexto de handoff já prefixado.
5. Nunca executar dentro da latência dos hooks. Hooks permanecem fire-and-forget e delimitados.
6. Nunca violar o isolamento de espaço de trabalho/projeto. Use `ScopeResolver` ou seus métodos auxiliares
   explícitos para todo caminho de leitura e escrita.
7. Nunca ignorar a autenticação. Use `AuthLevel::authorize(Capability::...)` para todas as interfaces
   de administração e escrita.
8. Nunca gravar arquivos na wiki diretamente a partir de um manipulador ou worker de segundo plano. Use
   `Wiki::write_page`, `Wiki::apply_batch` ou métodos auxiliares destrutivos existentes.
9. Nunca excluir automaticamente páginas semânticas. Use substituição, propostas pendentes ou a
   varredura de retenção existente para páginas episódicas.
10. Nunca reescrever páginas fixadas ou slots invariantes, a menos que a proposta cite uma
    contradição direta e seja aprovada explicitamente.
11. Incluir evidências de origem para toda edição proposta.
12. Atribuir propostas autônomas a um ator `auto_improve` distinto para que logs de auditoria,
    webhooks de admissão e telas de revisão possam distinguir mudanças sugeridas por máquina
    de gravações feitas por usuários/root.
13. Delimitar custo de modelo, tamanho de entrada, tamanho de saída e número de mutações de página
    propostas por execução.
14. Gravar uma linha de auditoria legível por máquina para cada execução e arquivos auxiliares (sidecars)
    legíveis por humanos sempre que houver propostas.

A política de captura é um limite de armazenamento do lado do cliente, não um filtro aplicado pelo
revisor. Os revisores só podem consumir o que foi armazenado; consulte
[Exclusões de captura](marker-file.md#exclusões-de-captura).

## Contrato de Atualização para Usuários Existentes

O autoaprimoramento disponível por padrão não deve trazer surpresas para instalações existentes:

1. Pastas existentes de wiki de projetos não necessitam de migração. Configurações antigas ainda podem
   conter uma chave `[auto_improve] mode = ...`; o ai-memory atual ignora essa
   chave legada. Os operadores podem remover a linha quando for conveniente.
2. O acionamento no fim da sessão permanece desativado; o agendador delimitado em segundo plano é executado
   fora da latência do hook e aguarda pelo intervalo configurado após cada
   ciclo (tick) não sobreposto em todos os projetos.
3. O agendador inicializa uma marca d'água de primeira execução por espaço de trabalho/projeto e
   registra uma reivindicação por sessão antes do trabalho agendado de LLM. Sessões históricas não
   são revisadas automaticamente na atualização, e revisões agendadas que falharam não são
   repetidas indefinidamente. O autoaprimoramento manual continua sendo o caminho para atualizar sessões
   antigas ou que falharam no agendamento.
4. O armazenamento de propostas pendentes deve usar migrações aditivas e idempotentes que
    preservem todos os arquivos de wiki e linhas de sessão/observação existentes.
5. Blocos instalados existentes de `CLAUDE.md`/`AGENTS.md` continuam válidos. Os operadores passam
   a usar as novas instruções de recuperação proativa executando `ai-memory install-instructions`
   ou pedindo a um agente que atualize o pacote de roteamento do ai-memory. A substituição
   baseada em marcadores deve permanecer idempotente, e os arquivos gerenciados de Agent Skills devem ser
   atualizados a partir dos mesmos ativos pertencentes ao binário que o snippet resumido.
6. Mutações em páginas de destino devem passar primeiro pela preparação da proposta e devem manter
   a atribuição de aprovação separada do ator autônomo
   `auto_improve` da proposta.

## Esboço de Configuração

Os nomes exatos podem mudar, mas a estrutura deve ser explícita e conservadora:

```toml
[auto_improve]
require_approval = false      # true leaves proposals pending for manual review
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

[auto_improve.scheduler]
enabled = true                # false disables background review only
interval_secs = 3600          # 0 disables background review only
max_sessions_per_tick = 1       # per project; ticks process projects sequentially
min_session_age_secs = 600
```

### Quais páginas o revisor pode realmente ler

`patchable_page_prefixes` decide os **corpos de página** de quais pastas são enviados para
o revisor. Todo o restante no projeto chega até ele através da lista de páginas recentes
como uma única linha (caminho, título, tipo, data de atualização) sem conteúdo.

Isso é importante porque o revisor não pode evitar duplicar aquilo que não consegue ler.
Um projeto que mantém suas invariantes em `decisions/` ou `gotchas/`, sem nenhuma
página em `_rules/`, envia **nenhum corpo de página**, e o modelo continuará
propondo regras que já estão escritas, muitas vezes com alta confiança,
porque a alegação realmente é bem evidenciada pela sessão.

O padrão é o par histórico, para que instalações existentes não sejam alteradas. Adicione
as pastas onde o seu conhecimento durável realmente reside:

```toml
patchable_page_prefixes = ["_rules/", "procedures/", "decisions/", "gotchas/"]
```

Prefixos são comparados contra o início do caminho da página e devem terminar com `/`,
para que `decisions/` não coincida também com `decisions-archive/`. Uma lista vazia desativa
completamente o contexto do corpo da página; uma string vazia na lista é ignorada em vez de
coincidir com todas as páginas.

Observe que `max_patchable_pages` ainda limita quantas das páginas correspondentes são
enviadas, e a lista de páginas recentes de onde são extraídas é ordenada por recência; logo, uma
página durável que não tenha sido atualizada recentemente pode ficar fora dela.

### Regras permanentes no escopo `_global`

Uma regra escrita uma vez no escopo reservado `_global` já vale para todo projeto, mas o
revisor lê só o projeto revisado e pode propor a mesma regra de novo como página do projeto.
Ative `include_global_titles = true` para listar também as páginas do escopo `_global` ao
revisor, uma linha cada (caminho, título, tipo, data de atualização), sem o corpo:

```toml
[auto_improve]
include_global_titles = true
```

A validação então rejeita uma proposta de página inteira cujo caminho ou título coincida com
uma página do `_global`, com o motivo `duplicate_global_path` ou `duplicate_global_title`.
Essas rejeições entram no contexto de rejeições das revisões seguintes como qualquer outra. A
lista é só leitura: uma página do `_global` nunca é alvo de correção, e a revisão do próprio
escopo `_global` não acrescenta nada, porque as páginas dele já são dele. Só o `_global` é
lido. As leituras padrão de todo leitor já o incluem, então nenhuma página de outro projeto
chega ao revisor. A opção vem desligada, e desligada o prompt não muda. O revisor por sessão
e o passe de experiência entre sessões seguem a mesma opção.


`[auto_improve.scheduler]` controla se e com que frequência o servidor inicia a
revisão em segundo plano. `[auto_improve] require_approval` controla se propostas validadas
são aplicadas automaticamente ou deixadas pendentes. Elas intencionalmente não
implicam uma à outra.

`max_rejection_context` e `rejection_context_days` limitam o resumo do buffer persistente
de rejeição incluído em prompts futuros do revisor. O buffer é delimitado por
`workspace_id` + `project_id` e armazena rejeições humanas, conflitos/falhas de aprovação
e candidatos rejeitados pelo validador/modelo quando trazem uma justificativa.
Uma aprovação que falhe porque um webhook de admissão com política `reject`
estava inacessível, expirou o tempo, atingiu limite de taxa ou respondeu 5xx ainda marca a proposta como
`failed`, mas não é armazenada: uma interrupção técnica não diz nada sobre a proposta.

## Formato da Proposta

A saída do LLM deve ser JSON estruturado, validado antes que qualquer coisa seja preparada:

```json
{
  "summary": "short human summary",
  "proposals": [
    {
      "operation": "create_or_update",
      "path": "gotchas/example.md",
      "title": "Example gotcha",
      "kind": "gotcha",
      "confidence": 0.82,
      "rationale": "why this is durable",
      "evidence": [
        {"page": "sessions/abc.md", "quote": "bounded quote"}
      ],
      "body_markdown": "# Example gotcha\n\n..."
    }
  ],
  "rejected_candidates": [
    {
      "reason": "single-command smoke test",
      "evidence": "sessions/xyz.md"
    }
  ]
}
```

A validação deve rejeitar propostas com evidências ausentes, prefixo de caminho incorreto,
corpos excessivamente grandes, tentativas de alterar páginas protegidas, operações não aceitas
ou confiança abaixo do limite configurado.

## Experiência do Usuário (UX) na Revisão Pendente

A primeira UX em produção é explícita e controlada por auditoria. O agendador e
execuções manuais via CLI/admin/MCP registram propostas pendentes validadas e, em seguida, as aprovam automaticamente
por padrão através do caminho de mutação da wiki. Com `require_approval = true`,
`pending-writes` as aplica ou rejeita mais tarde.

| Comando ou rota | Propósito |
|---|---|
| Agendador em segundo plano | Revisa sessões recém-concluídas após a marca d'água da primeira execução e aplica ou prepara propostas validadas de acordo com a política de aprovação. |
| `ai-memory auto-improve --session-id <id>` | Revisa manualmente uma sessão e aplica ou prepara propostas validadas pelo caminho de aprovação do autoaprimoramento. |
| `ai-memory auto-improve-report --workspace <w> --project <p> [--days N] [--limit N] [--stage]` | Relatório somente leitura de telemetria de execuções recentes de autoaprimoramento, resultados de propostas, taxas terminais e descobertas por padrão. `--stage` cria exatamente uma página de relatório de telemetria pendente para auditoria/aprovação. |
| `memory_auto_improve` | Revisa manualmente a sessão mais recente concluída sem execução de autoaprimoramento persistida, ou executa novamente de forma explícita uma sessão indicada, e aplica ou prepara propostas validadas pelo mesmo caminho. Uma execução vazia registra uma dispensa preliminar para que a próxima chamada implícita avance. |
| `ai-memory curator` | Revisão de manutenção baseada em regras, apenas relatório. |
| `ai-memory curator --stage` | Prepara exatamente uma página de relatório do curador para aprovação em pending-writes. |
| `ai-memory pending-writes list` | Exibe alterações preparadas na wiki. |
| `ai-memory pending-writes diff <id>` | Exibe o diff em markdown. |
| `ai-memory pending-writes approve <id>` | Aplica através do caminho normal de mutação da wiki. |
| `ai-memory pending-writes reject <id>` | Descarta proposta com trilha de auditoria. |
| `GET {web_slug}/pending` | Página de navegador exclusiva do root. Lista propostas pendentes em todos os projetos, com filtro por projeto e ordenação. Aprovar e rejeitar enviam POST para as rotas `/admin/pending-writes/{id}/approve\|reject` com o cookie de sessão e o cabeçalho CSRF. Um navegador conectado com o bearer token de root via HTTP Basic, antes de a autenticação por senha ser configurada, só pode ler: a página mostra os comandos `pending-writes` de cada proposta em vez dos botões. |

Propostas pendentes devem ser visíveis como markdown sob `_pending/auto-improve/`
para que humanos possam revisá-las no fluxo de trabalho da wiki/Obsidian. O SQLite ainda pode manter
o estado da proposta, status de aprovação, metadados de evidências e linhas de auditoria, mas o
artefato de revisão em si deve ser inspecionável e versionado como o restante
da wiki.

Em implantações que distinguem operadores, propostas preparadas manualmente registram
a identidade qualificada do operador e impõem uma proposta pendente por destino
*por operador*. Propostas não atribuídas do agendador, do curador e de telemetria permanecem
no repositório compartilhado. Uma colisão ignora apenas aquela proposta, preserva suas
irmãs e aparece na resposta do comando ou da API com o destino e o
motivo; isso não deve se tornar uma execução parcial silenciosa.

Como isso agora é uma interface de ferramentas MCP, os snippets de prompt padrão, as
Agent Skills gerenciadas e os testes de regressão garantem que `memory_auto_improve` apareça na
superfície combinada de roteamento de prompts. Snippets existentes instalados em `CLAUDE.md`/`AGENTS.md`
são atualizados de forma idempotente quando o operador executa
`ai-memory install-instructions` ou solicita que um agente atualize o pacote de roteamento
do ai-memory.

### Nota de atualização para instalações existentes

Pastas de wiki de projetos existentes não necessitam de migração. O armazenamento de propostas pendentes é uma
migração de banco de dados do lado do servidor e um diretório auxiliar (sidecar).

Configurações de servidores mais antigos podem conter uma chave `[auto_improve] mode = ...` agora ignorada.
Nenhuma migração de dados é necessária; remova a linha legada quando for conveniente para evitar
confusões.

## Estrutura do Loop de Manutenção

Mantenha o equivalente ao curador separado do revisor de aprendizado pós-sessão.

O loop de manutenção deve cuidar de:

1. Títulos duplicados ou quase duplicados.
2. Páginas restritas que devem ser mescladas em um conceito/gotcha mais amplo.
3. Estado obsoleto em `_slots/current-focus.md`.
4. Páginas episódicas que a fórmula de retenção marcar como frias.
5. Referências cruzadas quebradas e candidatas a contradição já identificadas pelo
   `memory_lint`.

O loop de manutenção começa operando apenas como relatório. `ai-memory curator --stage` prepara
uma página de relatório normal em `notes/curator-<date>.md`; aprová-la registra apenas
o relatório e não executa as ações de manutenção recomendadas. Posteriormente ele
poderá preparar propostas de mesclagem ou substituição. Ele não deve excluir páginas semânticas
automaticamente.

## Fases de Implementação

### Fase 1: Revisor Dry-Run

Status: implementado para preparação de propostas via CLI/admin/MCP mais autoaprovação padrão.

Adicionar um revisor em nível de biblioteca que consome uma sessão concluída mais o contexto existente
da wiki e retorna propostas validadas. O runtime armazena linhas de propostas
pendentes mais arquivos auxiliares (sidecars) primeiro, e então as aprova pelo caminho de mutação da wiki,
a menos que `require_approval = true` esteja configurado.

O revisor implementado foi projetado para projetos existentes com históricos grandes:
ele trata a página consolidada `sessions/<id>.md` como fonte primária quando ela
existe, e então adiciona uma amostra determinística e delimitada de observações brutas selecionadas
do contexto de início/fim, prompts do usuário, eventos de alta importância, palavras-chave de erro/correção/decisão
e pontos de verificação espaçados uniformemente. A validação rejeita evidências ausentes,
caminhos não aceitos, confiança baixa, corpos excessivos, caminhos ou títulos existentes duplicados,
e normaliza um H1 ausente prefixando o título da proposta antes da validação
final.

#### O contexto do revisor é delimitado (e como ele pode deixar de notar páginas duráveis existentes)

O revisor não vê todas as páginas. Dois limites moldam o conhecimento durável que ele
pode notar que já existe e, portanto, o que ele pode repropor de forma redundante:

- **Apenas os *corpos* de páginas sob `patchable_page_prefixes` são carregados**
  (`load_patchable_pages`). O padrão é `_rules/` e `procedures/`; páginas
  duráveis de outras famílias (`decisions/`, `gotchas/`, `concepts/`, `notes/`) não
  são enviadas integralmente ao revisor, a menos que sua pasta seja adicionada. Propostas de
  correção permanecem limitadas a `_rules/` e `procedures/` de qualquer maneira.
- **A lista de "páginas recentes" é ordenada por recência, uma linha por página** (caminho,
  título, tipo, data de atualização), extraída do briefing do projeto. Com
  `include_global_titles` ligado, uma segunda lista no mesmo formato traz até 100 páginas do
  escopo reservado `_global`. Por ser uma lista plana
  delimitada, um projeto com muitas páginas exibe ao revisor apenas as atualizadas mais
  recentemente. Para manter esses slots para o conhecimento durável, o revisor
  de autoaprimoramento exclui páginas de `sessions/` do *seu* contexto de páginas recentes
  (páginas de sessão nunca são alvos válidos de propostas e, de outra forma, dominariam
  a lista). Essa exclusão tem escopo restrito apenas ao revisor; o briefing do SessionStart
  e o `memory_briefing` continuam incluindo páginas de sessão, que é o seu lugar.

A consequência prática: uma página durável que resida fora dos prefixos configurados
e não esteja entre as páginas mais recentes pode ser invisível para o
revisor, de forma que a mesma lição pode ser proposta novamente. Duplicatas do revisor são
capturadas adiante (a validação rejeita uma proposta cujo caminho/título duplique uma
página existente que ele *possa* ver, e a preparação impõe uma proposta pendente por
alvo), mas uma proposta contra uma página durável que o revisor nunca viu não é
evitada no momento da revisão.

Melhorias postergadas (trabalho futuro, não nesta linha):

- **Desduplicação pelo vizinho mais próximo de embeddings**, substituindo a lista plana ordenada por recência
  por uma recuperação das páginas semanticamente mais próximas da sessão sob revisão, para que
  páginas duráveis relevantes surjam independentemente da recência.

Testes:

1. Sessões vazias/sem atividade não produzem propostas.
2. Sessões de smoke de comando único não produzem propostas.
3. Uma sessão com uma causa raiz duradoura produz uma proposta de gotcha.
4. Uma sessão com uma regra explícita de usuário produz uma proposta em `_rules/`.
5. Evidência ausente rejeita a proposta.

### Fase 2: Gravações Pendentes na Wiki

Status: implementado para preparação via CLI/admin, list/show/diff, aprovação e rejeição.

O armazenamento de propostas pendentes duráveis reside sob `_pending/auto-improve/` como
arquivos auxiliares (sidecars) não indexados mais linhas no SQLite, com comandos de list/diff/approve/reject
e linhas de auditoria. A aprovação é aplicada através dos limites de mutação da wiki existentes,
com o ator `auto_improve` preservado na proveniência da proposta.

Apenas o diretório `_pending/` na raiz do projeto é armazenamento reservado de propostas e
excluído da varredura de migração do OKF. Um caminho aninhado como
`notes/_pending/legacy.md` continua sendo uma página comum da wiki e ainda deve migrar.

Testes:

1. Propostas pendentes sobrevivem a reinicializações.
2. A aprovação grava arquivos e linhas de índice atomicamente.
3. A rejeição nunca grava um arquivo na wiki.
4. Propostas para páginas protegidas são rejeitadas antes da preparação.
5. Escopos parciais entre espaços de trabalho falham de forma fechada.
6. O markdown de propostas pendentes é criado em `_pending/auto-improve/` e nunca
   indexado como conhecimento durável aprovado.
7. Metadados de aprovação/auditoria preservam a atribuição da proposta a `auto_improve` e o
   ator aprovador separadamente.

### Fase 3: Agendador em Segundo Plano

Status: implementado como agendador no servidor, não como um gatilho de hook em SessionEnd.
O agendador fica ativado por padrão quando existe um provedor de LLM, aguarda após
cada ciclo não sobreposto e revisa sessões recém-concluídas em todos os
projetos após a marca d'água persistida de primeira execução de cada projeto. Ele registra uma
reivindicação por sessão antes de chamar o LLM, para que a revisão agendada ocorra no máximo uma vez por
sessão concluída, a menos que um administrador execute o autoaprimoramento manualmente de novo.

Testes:

1. Configuração com agendador desativado não faz nada, enquanto execuções manuais continuam funcionando.
2. Agendador ativado revisa apenas sessões posteriores à marca d'água.
3. Revisão saturada ou com falha mantém intacto o comportamento normal de fim de sessão e não
   tenta a mesma sessão repetidamente para sempre.
4. Linhas de execução incluem modelo/provedor, escopo, propostas, rejeições, configuração e
   metadados de disparo do agendador.

### Fase 4: Curador de Manutenção

Adicionar um relatório agendado e separado que use lint existente, contadores de acesso,
pontuação de retenção, links e metadados de página para propor mesclagens/substituições.

Testes:

1. A primeira execução adia ou relata antes que qualquer comportamento de mutação seja possível.
2. Páginas fixadas e slots invariantes são ignorados.
3. Páginas semânticas nunca sofrem exclusão definitiva (hard-delete).
4. Mesclagens propostas identificam páginas de origem e destino com evidências.

## Escolhas de Design Resolvidas

1. Propostas pendentes devem ser uma estrutura de primeira classe na wiki sob `_pending/`,
   com o SQLite retendo o estado e os metadados de auditoria.
2. Lições procedimentais devem ter uma família de páginas `procedures/` em vez de serem
   forçadas para dentro de `gotchas/` ou `concepts/`.
3. Propostas autônomas devem ser atribuídas a um ator `auto_improve` distinto,
   com a atribuição de aprovação rastreada separadamente.
4. O patamar mínimo de confiança deve ser configurável e calibrado com
   propostas aplicadas em projetos reais antes que qualquer disparo autônomo futuro seja cogitado.

## Conclusão Atual

O Hermes valida a ideia e também demonstra por que os limites importam. Um agente
capaz de gravar na memória por si só não é o que o torna útil. A parte útil é
um loop delimitado, observável e auditável que transforma trabalho repetido em conhecimento
durável enquanto mantém a execução da tarefa ativa isolada.

Para o ai-memory, o limite correto atual é a revisão agendada mais o armazenamento de propostas
pendentes sob `_pending/auto-improve/`. A política de aprovação é separada:
a autoaprovação padrão mantém a wiki compilando para frente, enquanto
`require_approval = true` oferece aos administradores uma fila humana sem desativar o
loop de aprendizado.
