# Operações de ciclo de vida

Referência para comandos destrutivos / que alteram estado no ai-memory.
Leia isto antes de executar qualquer coisa que modifique wiki + db, especialmente
em uma máquina de laboratório pessoal (homelab) onde erros são mais difíceis de desfazer.

## TL;DR - matriz de segurança

| Comando | Seguro com o servidor **em execução**? | Apaga dados? | Reversível? | Observações |
|---|---|---|---|---|
| `purge-project --confirm` | ✅ sim | os dados do único projeto, **além de** qualquer observação carimbada em um projeto diferente por uma das sessões deste projeto (em cascata, independentemente do `project_id` da própria observação), e anula (não exclui) a referência de sessão em qualquer handoff em um projeto diferente que as sessões deste projeto tenham criado ou aceitado | não | Exclui a raiz da wiki com namespace por UUID e os segmentos brutos do workstream. Recusa com `409` enquanto um workstream gerenciado sob o projeto mantiver um lease de execução ativo; `--force` substitui. Exclusão lógica por padrão; `--compact` adicionalmente reconstrói os índices FTS e executa `VACUUM` (veja abaixo). Sem `--confirm`, ele pré-visualiza as mesmas contagens, incluindo as entre projetos, antes de recusar (veja abaixo). |
| `purge-session --session-id --confirm` | ✅ sim | os dados da única sessão, **além de** qualquer observação carimbada em um projeto diferente por esta sessão (em cascata, independentemente do `project_id` da própria observação), e anula (não exclui) a referência de sessão em qualquer handoff em um projeto diferente que esta sessão tenha criado ou aceitado | não | Exclui uma sessão por UUID: sua linha, suas observações, os handoffs que ela **criou**, sua página `sessions/<id>.md` e cada versão substituída, seus embeddings e suas execuções de auto-aprimoramento. Escopo estrito: uma sessão que não pertença ao espaço de trabalho/projeto nomeado resulta em um `404` e nada é excluído. Handoffs que a sessão apenas *aceitou* são mantidos: esse texto pertence à sessão que o escreveu. Exclusão lógica por padrão; `--compact` adicionalmente reconstrói os índices FTS e executa `VACUUM` (veja abaixo). Sem `--confirm`, ele pré-visualiza as mesmas contagens, incluindo as entre projetos, antes de recusar (veja abaixo). |
| `handoffs --expire-all --confirm` | ✅ sim | não (apenas mudança de estado) | não (mas nada é destruído) | Marca cada handoff **aberto** no escopo como `expired` para que pare de ser oferecido a um agente. Linhas, resumos e proveniência são mantidos e permanecem visíveis no log de auditoria. Ao contrário da varredura automática, ele **não** poupa handoffs manuais ou de outro diretório. Um backlog restante é composto exatamente por esses, portanto, respeitar as isenções não limparia nada. `--older-than-days N` mantém bastões recentes. Escopo do proprietário: nunca toca no bastão de outro usuário. |
| `rename-project --from --to` | ✅ sim | não | sim (renomear de volta) | Atualização exclusiva de coluna em `projects.name`. O diretório em disco é indexado por `project_id` (UUID), portanto, a renomeação nunca move um arquivo. |
| `repair-split --project-id --from-project-id` | ✅ sim | não | parcialmente (renomear de volta; a identidade permanece movida) | Move a identidade de repositório de um projeto vazio para o projeto que contém as sessões do checkout e arquiva o vazio (#1993). Recusa quando o vazio tem sessões, páginas ou uma execução ativa. |
| `/admin/rename-workspace` | ✅ sim | não | sim (renomear de volta) | Atualização exclusiva de coluna em `workspaces.name`; atualiza manifestos de escopo `_meta.md` e cria pontos de verificação na árvore da wiki. |
| `/admin/delete-workspace` | ✅ sim | o espaço de trabalho e cada projeto filho, **além de** qualquer observação carimbada em um espaço de trabalho diferente por uma das sessões deste espaço de trabalho (em cascata, independentemente do `workspace_id` da própria observação), e anula (não exclui) a referência de sessão em qualquer handoff em um espaço de trabalho diferente que as sessões deste espaço de trabalho tenham criado ou aceitado | não | Executa a admissão `purge_workspace` primeiro, exclui linhas do SQLite em uma única cascata, remove o diretório do espaço de trabalho com chave UUID e segmentos brutos do workstream gerenciado, relata falhas parciais do sistema de arquivos e despacha notificação de espelho após o trabalho durável. Exclusão lógica por padrão; `"compact": true` adicionalmente reconstrói os índices FTS e executa `VACUUM` (veja abaixo). `"dry_run": true` pré-visualiza as mesmas contagens, incluindo as entre espaços de trabalho, sem excluir nada (veja abaixo). |
| `move-project --confirm` | ✅ sim | origem apenas no caso de mesclagem (um webhook `purge_project` com política `Reject` ainda pode abortar a desmontagem de origem deixando tudo intacto) | não | Destino novo → **movimentação real** sem perdas (recarimbar `workspace_id`, manter `project_id`, renomear o diretório): sessões/observações/handoffs + histórico sobrevivem. Destino com um projeto de mesmo nome → **mesclagem com cópia e expurgo (copy+purge merge)**: apenas as páginas mais recentes migram. Um projeto de destino existente com o mesmo nome torna isso uma **mesclagem com cópia e expurgo**, o que exclui as sessões, observações, handoffs e versões de página mais antigas da origem: agora também precisa de `--allow-lossy-merge`, e sem ele o servidor responde `409` nomeando as contagens que descartaria (#1713). |
| `merge-project [--confirm]` | ✅ sim | não | não | Reparação de identidade sem perdas no mesmo espaço de trabalho. O padrão é uma simulação (dry run) na transação do escritor. `--confirm` recarimba cada linha que carrega histórico e remove a origem somente após os arquivos de destino serem instalados; sessões abertas e execuções gerenciadas ativas recusam. Corpos no mesmo caminho são anexados em uma nova versão de destino sinalizada como obsoleta (stale) enquanto ambos os históricos permanecem acessíveis. |
| `move-session <id> --to --confirm` | ✅ sim | não | sim (mover de volta) | Recarimba uma sessão (ou cada sessão que toca `--from-project`) em outro projeto: `sessions`, `observations`, seus `handoffs`, tarefas de consolidação, execuções/reivindicações de auto-aprimoramento, candidatos a prompt de perfil, chaves de ingestão e sua página `sessions/<id>.md`, uma transação por sessão; o arquivo da página move-se com ela (`--pages move`, padrão) ou é aposentado para regeneração. Sem `--confirm`, é uma simulação real (com rollback). Recusa com `409` uma sessão aberta ou uma tarefa de consolidação pendente, a menos que `--force` seja usado. |
| `repair-backfill-timestamps --project --confirm` | ✅ sim | não | sim, se o operador manteve a linha de auditoria ou o relatório de antes/depois da CLI (nada mais) | Corrige `sessions.started_at`/`ended_at` para sessões que um `backfill` mais antigo importou antes de carregar o horário do próprio evento da transcrição. A CLI lê as transcrições locais do operador em modo somente leitura e envia tuplas candidatas `(session_id, started_at, ended_at)` para `POST /admin/repair-session-times`, que valida cada uma contra `(workspace, project)` (um candidato fora desse escopo é `not_found` e não é tocado), só reescreve uma linha cujo `started_at` seja posterior ao próprio término do candidato (`not_flattened` caso contrário, portanto, uma sessão capturada corretamente por hook ou uma reexecução é uma não-operação), recusa horários negativos/invertidos/com datas futuras e nunca atribui um horário de término a uma sessão ainda aberta. Sem `--confirm`, o servidor valida e relata dentro de uma transação revertida (rollback), portanto é uma simulação real; uma execução confirmada grava uma linha em `audit_log` por solicitação com os horários de antes/depois. Toca apenas nas duas colunas de timestamp de linhas já no escopo; `observations` e páginas (incluindo o frontmatter da própria página da sessão) não são tocadas. |
| `backup --to` | ✅ sim | não | n/a | Transmite um tarball compactado com gzip a partir do `sqlite3 .backup` online do servidor mais a árvore da wiki. Seguro junto ao escritor ativo. |
| passagem do livro-razão de eventos no `forget-sweep` (`[decay] ledger_retention_months`) | ✅ sim | *arquivos* antigos do livro-razão de eventos de hooks (`log-YYYY-MM.md`) do disco, nada no banco de dados | nenhum comando do ai-memory restaura um livro-razão (`restore-page` restaura apenas páginas); o conteúdo permanece no histórico do git da wiki quando um ponto de verificação o capturou, recuperável com git simples até que o repositório seja compactado | **Desativado por padrão** (`0`). Executa dentro da varredura de esquecimento, portanto `--dry-run` o pré-visualiza: o relatório lista `ledgers_prunable` e `ledger_bytes_prunable`. Online por meio da API de administração. Apenas um arquivo cujo nome seja `log-YYYY-MM.md` **e** cujo conteúdo comece com uma entrada de hook do livro-razão se qualifica; `log.md`, o mês atual e o mês anterior nunca são excluídos, qualquer que seja a configuração. Cada remoção consulta a cadeia de admissão bloqueante (operação `delete_event_ledger`); um webhook com política `reject` mantém o arquivo e o relatório o contabiliza em `ledgers_undeleted`. |
| `reclaim-ledger-versions` | ✅ sim | apenas *versões* de página do livro-razão substituídas anteriores à #660 | não (a versão mais recente da página permanece) | Simulação (dry-run) por padrão; precisa de `--confirm` para excluir. Online por meio do ator escritor, seguro junto ao escritor ativo. Filtrado por conteúdo: apenas versões não recentes e que não sejam de declínio de `log.md`/`log-YYYY-MM.md` cujo corpo comece com uma entrada de hook do livro-razão são removidas; uma página real que apenas compartilha o nome não é tocada. `--compact` adicionalmente reconstrói o FTS e executa `VACUUM` para recuperar os bytes liberados. |
| `checkpoints` | ✅ sim | não | n/a | Lista os pontos de verificação recentes do git da wiki. Somente leitura. |
| `restore-page --path (--from REV \| --page-id UUID)` | ✅ sim | substitui a versão de uma página markdown | sim (restaurar outra versão retida) | Restaura uma página do histórico git da wiki ou uma versão de página retida do SQLite, reindexa-a e grava um ponto de verificação pós-restauração. `--page-id` alcança o histórico de origem preservado por `merge-project`; ele não restaura outro estado do banco de dados. |
| MCP `memory_restore_page` | ✅ sim | não (grava uma nova versão atual; a substituída permanece acessível) | sim (restaurar outro ponto de verificação) | Lista os pontos de verificação de uma página ou grava um de volta por meio do caminho `memory_write_page`. Apenas administrador, como `restore-page`. Veja [abaixo](#mcp-memory_restore_page). |
| `backup-agents --to` / `restore-agents --from <tarball>` | ✅ sim (lado do cliente; nunca toca o servidor ou seu diretório de dados) | não | sim (`restore-agents --apply` mantém cópias com carimbo de data/hora dos arquivos substituídos) | Recursos de configuração do agente, não wiki/banco de dados: configurações do MCP (censuradas a menos que `--include-secrets`), configurações de hooks, instruções, skills, plugins. `restore-agents` é uma simulação (dry run) por padrão; `--apply` grava no disco e `--force` substitui destinos existentes ou diferentes. Recursos que não são do MCP são restaurados literalmente e podem conter segredos, portanto, use `--apply` apenas em um arquivo em cuja origem você confia. |
| `restore --from <tarball>` | ❌ **pare o servidor primeiro** | substitui o diretório de dados | não (sem backup prévio) | Recusa se algum processo `ai-memory` irmão estiver ativo (proteção do sysinfo). Prepara e verifica o arquivo antes de trocá-lo, de modo que uma restauração com falha deixa `wiki/` e `db/` como estavam. Mantém um `config.toml` existente e salva o do backup como `config.toml.from-backup`, a menos que `--with-config` seja passado. Do `.git` do wiki ele restaura apenas os dados do git (objetos, refs, logs, HEAD, o índice) e grava uma configuração de git nova; hooks, a configuração arquivada e qualquer outra coisa que o git executaria ou seguiria ficam de fora e são listados. |
| `reset --confirm` | ❌ **pare o servidor primeiro** | sim, todos os dados | não | Recusa se algum processo `ai-memory` irmão estiver ativo (proteção do sysinfo). |
| `reindex` | ❌ **pare o servidor primeiro** | sem exclusão da wiki; requer um banco de dados limpo | apenas com backup prévio do banco de dados | Reconstrói páginas/links/FTS a partir de `wiki/` usando manifestos `_meta.md`. Recusa se o SQLite já tiver linhas, para que o estado obsoleto apenas no banco de dados não possa sobreviver silenciosamente. |

`purge-project`, `rename-project` e `move-project` também aceitam `--project-id
<uuid>` em vez do nome (`project_id` nos endpoints de administração), para um projeto
cujo nome é ambíguo: uma linha gêmea legada vazia deixada por uma migração anterior
pode carregar o nome antigo de outro projeto, e a busca por nome a recusa.
`list-projects` imprime o id de cada projeto. O id é procurado dentro do
espaço de trabalho que o comando nomeia. Nos endpoints de administração, enviar tanto o nome quanto
o id, ou nenhum dos dois, resulta em `400`, assim como um UUID malformado, e um id que não nomeia nenhum
projeto nesse espaço de trabalho resulta em `404`. A CLI recusa ambas as flags juntas antes
de enviar qualquer coisa, e sem nenhuma delas ela resolve o projeto da mesma forma que outros
comandos fazem: o `project` do marcador `.ai-memory.toml`, depois o remoto git, depois
o nome do diretório. `--confirm`, a pré-visualização e todas as outras proteções se comportam
da mesma forma que com um nome.

Esse destino derivado é uma descontinuação, não uma promessa: `purge-project --confirm`
sem `--project`/`--project-id` imprime o projeto que resolveu e o
comando que o nomeia, e será recusado no 4.0, porque a única coisa que
um expurgo não deve fazer é adivinhar entre "o checkout em que estou" e "o
projeto que eu pretendia" (#1642). Uma flag explicitamente vazia já é recusada: `--project
""` costumava passar na verificação "foi nomeado" e depois resolver como se não tivesse sido
passada, que foi como uma variável não definida em um script atingiu uma exclusão real.

Comandos que afetam o estado são roteados pela API HTTP de administração, exceto `reset`,
`restore` e `reindex`, que são operações de ciclo de vida diretas no disco que
fundamentalmente não podem ser executadas enquanto outro processo mantiver o escritor SQLite WAL. Veja
[AGENTS.md, invariante 9](../../AGENTS.md#cross-cutting-invariants-do-not-violate)
para a proteção de processo ativo.


## O que significa "excluído" (`purge-session`, `purge-project`, `delete-workspace`)

`purge-session` responde a *"esqueça esta conversa"*: após sua execução, a
sessão desaparece da API, das contagens de `status` e da busca.
Ela remove tanto versões de resumo anteriores quanto posteriores identificadas como pertencentes a
essa sessão; versões escritas à mão no mesmo caminho permanecem, incluindo um arquivo de
wiki ativo posterior.

```bash
ai-memory purge-session \
  --workspace default --project my-app \
  --session-id 0199f3d2-1c4e-7a10-9f3b-2b0c5d8e7a11 \
  --confirm
```

É uma **exclusão lógica**, e a distinção importa se você estiver atendendo a uma
solicitação de apagamento regulatório em vez de apenas fazer uma limpeza:

| | padrão | `--compact` |
|---|---|---|
| Acessível por meio da API / ferramentas MCP | não | não |
| Retornado pela busca (FTS) | não | não |
| Arquivo Markdown ativo da wiki | não, melhor esforço | não, melhor esforço |
| Bytes ainda presentes em `memory.sqlite` | **sim**, em páginas livres | não |
| Texto ainda no histórico git da wiki | **sim** | **sim** |
| Presente em backups realizados antes do expurgo | **sim** | **sim** |
| Custo | uma transação | reescreve todo o banco de dados |

`--compact` reconstrói os índices FTS5 afetados e depois executa `VACUUM` para liberar as páginas
livres. A reconstrução é necessária porque uma exclusão comum deixa os tokens dentro
dos segmentos do índice, e um `VACUUM` sozinho não os limpa. Ele precisa de espaço livre em disco de aproximadamente o tamanho do próprio banco de dados e leva
minutos em um repositório grande, razão pela qual é opcional (opt-in).

O `VACUUM` reescreve cada página por meio do log de gravação antecipada (write-ahead log) do SQLite, de modo que o arquivo `-wal`
ao lado de `memory.sqlite` cresce até aproximadamente o tamanho do banco de dados enquanto ele é executado.
`compact`, `reclaim-ledger-versions --compact` e os expurgos com `--compact`
portanto executam `PRAGMA wal_checkpoint(TRUNCATE)` logo após o `VACUUM`, e o
`-wal` retorna a zero bytes. Se um leitor mantiver um snapshot antigo, o ponto de verificação
não consegue terminar: o comando ainda tem sucesso, o `ai-memory` registra um `warn` com as
contagens de `busy`, `log` e `checkpointed`, e o `-wal` mantém seu tamanho até um
ponto de verificação posterior após a saída desse leitor (o escritor espera até 5 segundos pelos
leitores antes de desistir). A conexão do escritor também define `journal_size_limit`
como 64 MiB, que é o tamanho para o qual o `-wal` é reduzido após qualquer ponto de verificação. Não
é um limite máximo: uma transação em execução, ou um leitor que bloqueia o ponto de verificação, pode
empurrar o arquivo além disso. Em `SIGINT`, `SIGTERM`, Ctrl-C ou no fim da entrada stdio, `serve` fecha suas conexões
de leitura e executa um último ponto de verificação, aguardando no máximo 5 segundos por ele; uma
leitura ainda em execução mantém sua conexão até terminar, e o ponto de verificação é de
melhor esforço. Um `SIGKILL` deixa o `-wal` como estava e a próxima inicialização o
reproduz.

**`--compact` não é apagamento forense.** O repositório git da wiki armazena o conteúdo
das páginas em seus objetos *e em suas mensagens de commit*, e qualquer backup realizado antes
do expurgo ainda contém tudo. Remover os bytes do arquivo SQLite ativo
vale a pena ser feito por seus próprios méritos; não descreva isso a um usuário como uma
garantia de que o conteúdo é irrecuperável, porque não é.

### Pré-visualização sem `--confirm`

Sem `--confirm`, a CLI primeiro solicita ao servidor uma pré-visualização
(`"dry_run": true` na solicitação, exatamente como o campo do próprio `purge-project`).
`dry_run` sempre prevalece sobre `confirm` (`{"confirm": true, "dry_run": true}`
ainda assim apenas pré-visualiza), de modo que uma solicitação de pré-visualização nunca se torne destrutiva por
acidente.

A pré-visualização executa as mesmas buscas e contagens que um expurgo confirmado usa para decidir
o que excluir, com o mesmo 404 para uma sessão fora do escopo nomeado, incluindo
duas contagens entre projetos para o que o expurgo desta *sessão* exclui ou deixa
órfão colateralmente em um projeto *diferente* por meio de seu próprio id
(`collateral_observations_deleted`, `collateral_handoffs_denulled`, o mesmo formato que a pré-visualização do `purge-project` relata um nível acima, na granularidade de projeto em vez de
sessão), sem nunca emitir o `DELETE`. Ela nunca executa a
exclusão e a reverte. Como nada é excluído, `removed_paths` na
resposta lista os caminhos das páginas da wiki que um expurgo confirmado *removeria* (não caminhos já removidos), e `files_deleted`/`files_failed` estão sempre vazios, já que nenhum
arquivo é tocado; nem o registro de descarte (tombstone) `purged_sessions` nem a linha de `audit_log`
são gravados, e nenhum ponto de verificação é criado. A resposta traz
`"dry_run": true`. A CLI imprime:

```
Would purge session from default/my-app: 1063 observations, 0 handoffs, 1 pages, 0 auto-improve runs.
```

(com uma cláusula final "Plus N observations in other projects via this session" /
"Plus N handoffs ..." quando qualquer uma das contagens entre projetos não for zero;
observe que o próprio id da sessão nunca é impresso: o chamador já o tem, e
este comando existe para fazer uma sessão deixar de existir), recusando em seguida com
a mensagem existente de "destructive and irreversible" e saída diferente de zero. A pré-visualização adiciona informações à recusa e nunca
substitui o `--confirm`.

Uma pré-visualização também pula a chamada de admissão bloqueante que um expurgo confirmado faz
antes de excluir qualquer coisa (`admit_purge_session`): nada foi decidido ainda, portanto
não há nada sobre o que um webhook com política `Reject` possa agir. Isso significa que uma pré-visualização `200`
de pré-visualização não é uma garantia: esse mesmo webhook só é executado no caminho
confirmado e ainda pode recusar o expurgo real posteriormente.

Se o servidor estiver inacessível, atingir o tempo limite (alguns segundos, atualização do token de autenticação
incluída), ou for anterior a este campo (um `400` simples), a CLI faz um fallback
silencioso para a recusa simples sem linha de pré-visualização. Um `404`/`403` (ou qualquer
outro status inesperado) imprime o próprio erro do servidor antes da recusa,
já que o operador perguntou o que aconteceria e o servidor tem uma
resposta real.

Um handoff que esta sessão apenas *aceitou* (não criou) mantém sua linha e
seu texto em ambos os casos, confirmado ou pré-visualizado, e perde apenas seu
ponteiro `accepted_by_session` (`ON DELETE SET NULL`) quando a linha da sessão
realmente desaparece; ele nunca é contado em `handoffs_deleted`. Mais dois locais onde o
id de uma sessão expurgada é referenciado não são contados nem pré-visualizados hoje,
como um acompanhamento conhecido: `agent_messages.from_session_id` /
`claimed_by_session` (a caixa de mensagens entre projetos, V64) e o `auto_improve_runs.session_id`
de outro projeto, ambos `ON DELETE SET NULL` e fora do escopo
dos dois campos `collateral_*` desta pré-visualização, que cobrem apenas observações
e handoffs: o mesmo conjunto que a própria pré-visualização de `purge-project` cobre, um nível
acima.

### O mesmo vale para `purge-project` e `delete-workspace`

Cada linha acima se aplica inalterada ao `ai-memory purge-project --compact` e a
`POST /admin/delete-workspace` com `{"compact": true}`. A tabela é uma
propriedade de *qualquer* exclusão do SQLite, não de um único comando: linhas somem, bytes permanecem em
páginas livres até que o arquivo seja reescrito.

O texto de ajuda para esses dois comandos costumava prometer "TODOS os seus dados", o que soava como remoção em nível de byte que eles nunca
realizaram (#540). Ambos agora descrevem o mesmo limite e ambos aceitam a mesma
opção (opt-in).

Um detalhe extra se aplica no escopo de projeto e espaço de trabalho. As linhas de `workstream_events` de um
workstream gerenciado saem pela cascata
`projects → workstreams → workstream_events` em vez de um
`DELETE` direto contra essa tabela, e uma cascata não dispara o gatilho `AFTER DELETE`
que removeria a linha de `workstream_events_fts`. A compactação,
portanto, reconstrói todos os três índices FTS (`pages_fts`,
`observations_fts` e `workstream_events_fts`), não apenas os dois que um expurgo de sessão
precisa. Reconstruir apenas os índices que um dado chamador "deveria" ter tocado
é o que deixa o texto da transcrição de um agente gerenciado no arquivo depois que um operador
solicitou sua recuperação.

Expurgos de sessão mantêm a proteção de mutação da wiki durante toda a exclusão do banco de dados e
a limpeza de arquivos. Gravações de páginas em andamento e reindexações do observador terminam antes que o
expurgo comece; novas gravações aguardam até que a limpeza seja concluída. Isso também serializa o
expurgo com movimentações de projeto/sessão da wiki. Webhooks de admissão são executados antes dessa proteção.
Falhas na limpeza de arquivos ainda deixam o expurgo do banco de dados confirmado e são relatadas
em `files_failed`; essa coordenação não fornece reversão atômica em caso de travamento (crash-atomic).

A proteção é adquirida antes de o expurgo ser enviado ao ator escritor, portanto também
cobre a espera pelo que quer que essa fila única já esteja drenando e, com
`compact: true`, o `VACUUM` que é executado após a confirmação da exclusão. Um expurgo em um
servidor ocupado, portanto, retém as mutações da wiki por mais tempo do que a própria exclusão.
Os pontos de verificação git criados antes e depois do expurgo ficam fora da proteção: eles o
delimitam, não tiram um snapshot dele.

A reindexação verifica os registros de descarte (tombstones) de expurgo de sessão na mesma transação SQL de sua
gravação de página. Um evento do observador ou varredura de diretório que começou antes de um expurgo
não pode usar uma verificação desatualizada para restaurar um resumo deixado por uma falha na limpeza de arquivos.
Uma substituição manual já preservada pelo expurgo permanece reindexável:
a página em disco não deve nomear a sessão expurgada como sua proprietária, e uma versão
ativa não pertencente deve continuar existindo nesse caminho. Um resumo legado remanescente
sem `session_id` não tem essa versão sobrevivente e permanece excluído.
O observador, sua passagem de reconciliação e o `reindex` usam a mesma verificação.

### Contenção de escopo

O id da sessão nunca é uma autoridade por si só. Cada instrução é filtrada em
`workspace_id` e `project_id`, bem como em `session_id`; a sessão deve
pertencer ao escopo nomeado ou a chamada resulta em `404` sem nada excluído, e as
páginas derivadas são excluídas por **id** em vez de por caminho, porque dois projetos podem conter o mesmo `sessions/<uuid>.md`, e excluir por caminho levaria o outro
junto. O `/admin/purge-session` executa a cadeia de admissão antes que qualquer linha seja
tocada, portanto um webhook com `failure_policy = reject` ainda pode abortar toda a
operação enquanto os dados estiverem intactos. Após a confirmação da transação SQL, o
servidor remove apenas os caminhos de página retornados sob essa mesma raiz de projeto com chave UUID;
a resposta mantém `removed_paths` para o expurgo lógico do banco de dados e relata a
limpeza real em `files_deleted` / `files_failed`. Se a remoção no sistema de arquivos falhar,
o expurgo do banco de dados não é revertido, a chamada ainda retorna 200 com `files_failed`
preenchido e observadores assíncronos de `purge_session` recebem `partial_failure: true`.


## O que significa "isolamento de projeto" aqui

Os dados de cada projeto residem sob uma raiz isolada com chave UUID em disco:

```
<wiki_root>/
├── .git/
├── <workspace_id>/
│   ├── _meta.md                 # workspace name for rebuilds
│   └── <project_id>/
│       ├── concepts/
│       ├── decisions/
│       ├── gotchas/
│       ├── sessions/
│       ├── _rules/
│       ├── _meta.md             # project name, path, optional identity keys
│       ├── log-YYYY-MM.md      # rolling event log, one file per month
│       └── bootstrap.md
└── <other_workspace_id>/
    └── <other_project_id>/
        └── ...
```

O **nome do projeto** mutável (o legível por humanos `distrobox-gaming`
ou `.config` que você vê em `/web/`) nunca aparece em nenhum caminho de disco; o
UUID estável de **project_id** sim. A coluna `projects.name` do SQLite mapeia
nome → id. Dois projetos podem ter exatamente o mesmo `pages.path` (por exemplo,
ambos têm `decisions/0001.md`) sem colidir em disco; a
estrutura com namespaces garante o isolamento estrutural.

O histórico git tem raiz em `<wiki_root>` (um repositório, todos os projetos
como subárvores). Um `git log` dentro do diretório da wiki mostra alterações
em todos os projetos; diffs por projeto também são possíveis via
`git log -- <workspace_id>/<project_id>/`.

Cada diretório de espaço de trabalho também carrega `<workspace_id>/_meta.md`, e cada
diretório de projeto carrega `<workspace_id>/<project_id>/_meta.md`. Esses pequenos
manifestos apenas com frontmatter armazenam nomes legíveis (mais `repo_path` para projetos),
de modo que um banco de dados SQLite limpo possa ser reconstruído apenas a partir da árvore da wiki indexada por UUID.

## Comando por comando

### `purge-project`

```bash
ai-memory purge-project --workspace default --project my-project --confirm

# …and to reclaim the freed bytes as well (slow; rewrites the whole database):
ai-memory purge-project --workspace default --project my-project --confirm --compact
```

Assim como `purge-session`, esta é uma exclusão lógica, a menos que `--compact` seja fornecido;
veja [O que significa "excluído"](#o-que-significa-excluído-purge-session-purge-project-delete-workspace).

O que acontece, em ordem:

1. O servidor pesquisa `(workspace_id, project_id)` por nome. Retorna 404
   se algum estiver ausente.
2. Recusa com 409 quando um workstream gerenciado sob o projeto ainda
   mantiver um lease de execução **ativo** (`managed_runs.state = 'active'` E
   `lease_expires_at` no futuro). `workstreams` é excluído em cascata de
   `projects` e `managed_runs` é excluído em cascata de `workstreams`, portanto
   o expurgo excluiria a linha de lease de um agente em execução: seu heartbeat
   falharia então com `409 managed run lease is not active` pelo
   restante da sessão e a transcrição nunca alcançaria o livro-razão.
   Um lease caducado (wrapper que travou) **não** bloqueia o expurgo.
   `--force` expurga mesmo assim.
3. Conta linhas que serão excluídas em cascata (`pages`, `sessions`,
   `observations`, `handoffs`, `page_embeddings`, além de `workstreams`
   e `managed_runs`).
4. Executa um único `DELETE FROM projects WHERE id = ?`: as chaves estrangeiras com
   `ON DELETE CASCADE` de V01 + V05 se propagam para todas as tabelas
   dependentes em uma única transação.
5. A limpeza do sistema de arquivos em melhor esforço remove tanto a raiz da wiki com namespace por UUID
   quanto cada diretório de segmento `<data_dir>/raw/workstreams/<workstream_id>/`.
6. Retorna um resumo: `{label, pages_deleted, sessions_deleted, …,
   workstreams_deleted, managed_runs_deleted, workstream_ids,
   files_deleted: [<project_root>, <raw_workstream_dir>, ...],
   files_failed: [...]}`.

`workstream_ids` permanece no relatório para fins de auditoria. Cada diretório de segmento
bruto correspondente é removido no servidor e aparece em
`files_deleted`; uma remoção com falha aparece em `files_failed` junto com falhas na
limpeza da wiki.

#### Pré-visualização sem `--confirm`

Sem `--confirm`, a CLI primeiro solicita ao servidor uma pré-visualização
(`"dry_run": true` na solicitação). `dry_run` sempre vence `confirm`:
`{"confirm": true, "dry_run": true}` ainda assim apenas pré-visualiza, da mesma forma que
`reclaim-ledger-versions` trata seu próprio campo `dry_run`, de modo que uma solicitação de pré-visualização
nunca possa se tornar destrutiva por acidente.

A pré-visualização executa as mesmas buscas e contagens que as etapas 1 a 3 acima usam para decidir
o que um expurgo confirmado excluiria (o mesmo 404 em um escopo desconhecido, o mesmo
`409` em um lease de execução gerenciada ativa sem `--force`) e retorna essas
contagens, incluindo as duas contagens entre projetos (uma observação excluída, ou a
referência de sessão de um handoff anulada, em um projeto diferente daquele nomeado;
veja a linha da matriz acima), sem nunca emitir o `DELETE` na etapa 4. Ela
não executa a exclusão para revertê-la: em um projeto grande isso custaria
tanto tempo do ator escritor quanto um expurgo real (cada captura de hook enfileirada atrás
dela paga por isso), sem nenhum benefício além de apenas contar. Como a etapa 4 nunca
é executada, a limpeza do sistema de arquivos da etapa 5 também nunca é executada (`files_deleted` /
`files_failed` estão sempre vazios), e nem o registro de descarte `purged_scopes`
nem a linha de `audit_log` da transação da etapa 4 são gravados; nenhum
ponto de verificação é criado. A resposta traz `"dry_run": true`. A CLI imprime:

```
Would purge default/my-project: 3 pages, 1 sessions, 1063 observations, 0 handoffs, 3 embeddings, 0 workstreams, 0 managed runs.
```

(com uma cláusula final "Plus N observations in other projects via their sessions"
/ "Plus N handoffs ..." quando qualquer uma das contagens entre projetos não for zero),
recusando em seguida com a mensagem existente de "destructive and irreversible"
e saída diferente de zero. A pré-visualização adiciona informações à recusa e nunca
substitui o `--confirm`.

Uma pré-visualização também pula a chamada de admissão bloqueante que um expurgo confirmado faz
antes de excluir qualquer coisa (`admit_purge_project`): nada foi decidido ainda,
portanto não há nada sobre o que um webhook com política `Reject` ou de proteção de escopo possa agir.
Isso significa que uma pré-visualização `200` não é uma garantia: esse mesmo webhook só é executado
no caminho confirmado e ainda pode recusar o expurgo real posteriormente.

Se o servidor estiver inacessível, atingir o tempo limite (alguns segundos, atualização do token de autenticação
incluída), ou for anterior a este campo (um `400` simples), a CLI faz um fallback
silencioso para a recusa simples sem linha de pré-visualização, de modo que nenhum código de saída
ou formato de erro de script existente seja alterado; apenas um expurgo confirmado é
destrutivo. Um `404`/`409`/`403` (ou qualquer outro status inesperado) imprime o
próprio erro do servidor antes da recusa, já que o operador perguntou o que
aconteceria e o servidor tem uma resposta real.

Modos de falha:

- **Nome do espaço de trabalho ou projeto não encontrado** → 404, nenhuma mutação.
- **Flag de confirmação omitida** → 400, nenhuma mutação.
- **Execução gerenciada ativa, sem `--force`** → 409 nomeando os workstreams, nenhuma
   mutação. Conclua ou cancele a sessão, ou execute novamente com `--force`
   (o agente em execução deixa de conseguir salvar seu histórico).
- **Falha parcial em `remove_dir_all`** (por exemplo, permissões) → as linhas do banco de dados
   já sumiram, mas `files_failed` é preenchido. Executar novamente
   o comando com os mesmos argumentos é idempotente; a segunda chamada
   retorna 404 (projeto já excluído).

Por que isso é seguro com o servidor em execução:

- A exclusão em cascata no banco de dados é uma única transação; o ator escritor a serializa
  em relação a quaisquer outras gravações.
- A exclusão em disco toca apenas no subdiretório com chave UUID do projeto,
  com o qual nenhum outro projeto compartilha arquivos. Nenhuma concorrência com o
  observador mesmo no meio da gravação: na pior das hipóteses, o observador emite eventos de
  exclusão para arquivos que acabamos de remover, os quais ele ignora (nenhuma linha no banco de dados
  para reindexar).

### `rename-project`

```bash
ai-memory rename-project --workspace default --from old-name --to new-name
```

O que acontece:

1. Pesquisa `(workspace_id, project_id)` pelo nome atual. 404 se não
   encontrar.
2. Valida o novo nome: não vazio, sem `/`, sem espaços em branco no
   início ou fim. 422 em entrada inválida.
3. `UPDATE projects SET name = ? WHERE id = ?`. Violação de UNIQUE
   no índice `(workspace_id, name)` → 422 com "name taken".
4. Atualiza o `_meta.md` do projeto com chave UUID e cria ponto de verificação na wiki.
5. Retorna `{workspace, from, to, pages, checkpoint}` mais
   `manifest_warning` quando a atualização pós-confirmação falhar.

Zero arquivos são movidos no disco porque o caminho em disco é indexado por
`project_id`, não pelo nome. A URL da interface web `/web/w/<ws>/<proj-name>/…`
apenas resolve para o mesmo `project_id` após a atualização da coluna.
Este comando também não renomeia um checkout de origem nem reescreve nenhum localizador de sessão nativa do agente.
Consulte o [comportamento de renomeação de workstream
gerenciado](managed-workstreams.md#renomeações-de-projeto-e-de-diretório) antes de
renomear fisicamente um checkout que possui sessões nativas.
Após uma renomeação bem-sucedida na CLI, o cliente também reindexa a chave de seu link local de checkout em `show`.
Uma solicitação direta a `/admin/rename-project` não pode atualizar registros de clientes de outras máquinas;
o próximo `run` gerenciado bem-sucedido a partir de um checkout atualiza
seu link.

Modos de falha:

- **Nome `to` já existe neste espaço de trabalho** → 422.
- **`to` inválido (vazio, barra, espaços em branco)** → 422.
- **Origem `from` não encontrada** → 404.
- **Atualização de manifesto/ponto de verificação falhou após a confirmação** → 200 com
  `manifest_warning`; a renomeação no SQL permanece confirmada e o backfill
  de manifesto na inicialização pode reparar os metadados do disco. As respostas de inicialização, gravação de página,
  mesclagem de move-project, `merge-project`, criação de move-session e execução gerenciada apresentam
  o mesmo formato de aviso quando a resolução de escopo realizou a promoção.
  O aviso é retido se a operação promovida falhar posteriormente: o MCP o coloca em
  `error.data.manifest_warning`, enquanto erros HTTP de administração/execução gerenciada mantêm o
  `manifest_warning` no nível superior. Seus comandos da CLI o imprimem em stderr; chamadores
  da API o recebem em JSON.

### `/admin/rename-workspace`

Renomeia um espaço de trabalho atualizando `workspaces.name`; caminhos em disco continuam indexados por
`workspace_id`, portanto nenhum arquivo de página é movido. Após a renomeação no SQLite, o manipulador
atualiza manifestos de escopo `_meta.md` com `Wiki::backfill_scope_manifests()` e
retorna `manifests_refreshed` mais um ponto de verificação pós-renomeação quando a árvore da wiki
for alterada.

Se a atualização do manifesto falhar após a renomeação no SQLite ter sido confirmada, a renomeação
ainda retorna `200 OK` com `manifests_refreshed: 0` e uma string `manifest_warning`
em vez de relatar um 500 enganoso. A renomeação no banco de dados é autoritativa nesse
ponto; operadores podem executar novamente uma atualização de manifesto ou restaurar a partir do ponto
de verificação emitido se precisarem reparar desvios no `_meta.md`.

Modos de falha:

- **Origem `from` não encontrada** → 404.
- **Nome `to` já existe ou é inválido** → 422.
- **Atualização de manifesto falhou após a confirmação** → 200 com `manifest_warning` e
  renomeação confirmada no banco de dados.

### `/admin/delete-workspace`

Exclui uma linha de espaço de trabalho e todos os projetos/páginas/sessões/workstreams gerenciados
filhos por meio da exclusão em cascata de `workspace_id`. A rota é protegida por
`force: true` para espaços de trabalho não vazios e segue a ordem de operação destrutiva
usada pelos expurgos de projeto. Ela aceita a mesma recuperação opcional (opt-in) que os
outros dois comandos destrutivos (`{"compact": true}`) e, caso contrário, é uma
exclusão lógica; veja
[O que significa "excluído"](#o-que-significa-excluído-purge-session-purge-project-delete-workspace).

1. Pesquisa o espaço de trabalho sem criar escopos ausentes.
2. Executa a admissão bloqueante `op=purge_workspace`. Um webhook com política reject aborta
   antes que linhas do banco de dados ou arquivos sejam removidos.
3. Cria um ponto de verificação pré-exclusão se a árvore da wiki tiver alterações pendentes.
4. Exclui o espaço de trabalho em uma transação do ator escritor, contando
   `projects_deleted`, `pages_deleted`, `sessions_deleted`,
   `observations_deleted`, `handoffs_deleted`, `embeddings_deleted`,
   `workstreams_deleted`, `managed_runs_deleted`, mais as duas contagens entre
   espaços de trabalho (`collateral_observations_deleted`, `collateral_handoffs_denulled`, o mesmo formato que a pré-visualização de `purge-project` relata um nível abaixo,
   na granularidade de projeto em vez de espaço de trabalho) antes de emitir o `DELETE`.
5. Remove `<wiki_root>/<workspace_id>` e cada diretório afetado
   `<data_dir>/raw/workstreams/<workstream_id>` do disco. A
   resposta relata `workstreams_deleted`, `managed_runs_deleted` e os
   `workstream_ids` limpos ao lado dos resultados do sistema de arquivos.
6. Despacha notificações de espelho `purge_workspace` não bloqueantes após o trabalho
   durável. Se a exclusão no banco de dados foi confirmada, mas a remoção em disco falhou, a resposta
   inclui `files_failed` e `ctx.partial_failure: true` no webhook.
7. Cria um ponto de verificação pós-exclusão se a árvore da wiki tiver sido alterada.

Modos de falha:

- **Espaço de trabalho não encontrado** → 404, nenhuma mutação.
- **Espaço de trabalho não vazio sem `force: true`** → 409, nenhuma mutação.
- **Webhook `purge_workspace` com política Reject falha** → 500, nenhuma mutação no banco de dados/disco.
- **Remoção no sistema de arquivos falha após a confirmação no SQL** → 200 com `files_failed`
  preenchido e `partial_failure: true` em notificações de espelho assíncronas; a limpeza
  manual do caminho relatado é necessária.

#### Pré-visualização com `"dry_run": true`

`"dry_run": true` no corpo da solicitação sempre vence (`{"force": true,
"dry_run": true}` ainda assim apenas pré-visualiza), de modo que uma solicitação de pré-visualização nunca
possa se tornar destrutiva por acidente, o mesmo padrão que `purge-project` usa para
seu próprio campo `dry_run`. A pré-visualização executa as contagens da etapa 4
(incluindo as duas entre espaços de trabalho) contra as mesmas linhas que uma exclusão
confirmada removeria, usando o mesmo `409` para um espaço de trabalho não vazio
sem `force` e o mesmo `404` para um espaço de trabalho desconhecido, e
retorna antes de emitir o `DELETE`. As etapas 2, 3, 5, 6 e 7 nunca são executadas
sob `dry_run`: nenhuma chamada de admissão, nenhuma remoção de diretório da wiki, nenhum despacho
de espelho, nenhum ponto de verificação; `files_deleted`/`files_failed` estão sempre vazios e
`pre_checkpoint`/`checkpoint` estão sempre ausentes. A resposta traz
`"dry_run": true`. Não há subcomando de CLI para `delete-workspace` atualmente,
portanto, esta pré-visualização é apenas por HTTP.

Os dois campos `collateral_*` cobrem apenas observações e handoffs, o mesmo
conjunto que a pré-visualização do próprio `purge-project` cobre. `agent_messages` (V64, a
caixa de mensagens entre espaços de trabalho) é afetada da mesma forma entre espaços de trabalho e
não é contada nem pré-visualizada hoje, como um acompanhamento conhecido: uma mensagem PARA a caixa
de correio de um espaço de trabalho *diferente* é excluída sumariamente se tiver sido enviada A PARTIR DO
espaço de trabalho que está sendo excluído (`from_workspace_id` é `ON DELETE CASCADE`, sem
consideração pelo próprio `to_workspace_id` da mensagem), e ponteiros `from_session_id`/
`claimed_by_session` em mensagens em outros locais são anulados, não excluídos,
quando referenciam uma sessão que residia no espaço de trabalho excluído (`ON DELETE
SET NULL`).

### `merge-project`

Use isto apenas quando duas linhas de projeto em um espaço de trabalho forem a mesma cópia
de trabalho lógica, frequentemente após `audit-contamination` relatar uma colisão
de ortografia por maiúsculas/minúsculas ou acentuação:

```bash
# Exact dry run: no files or rows change.
ai-memory merge-project --workspace default --from Renato --into renato

# Apply after inspecting the counts.
ai-memory merge-project --workspace default --from Renato --into renato --confirm
```

O comando é restrito ao root em um servidor multiusuário, como qualquer operação
em `/admin/*`. Os nomes podem ser substituídos independentemente por `--from-project-id` e
`--into-project-id`; cada id é resolvido dentro de `--workspace`, portanto um id de
outro espaço de trabalho retorna 404 e não realiza nenhuma mutação.

O escritor verifica ambos os conjuntos de páginas mais recentes sob a transação. Ele recusa se
a origem tiver uma sessão aberta ou um lease de execução gerenciada ativa não expirado. Uma
colisão de mesmo caminho produz uma nova versão cujo corpo é o corpo de destino, um
marcador `<!-- ai-memory:merged-from workspace/source -->` e o corpo de origem.
O frontmatter de destino prevalece; `expires_at`/`stale_after` sobrevivem apenas quando ambos
os lados estavam expirando. A nova página mantém o horário de criação anterior e recebe
feedback `stale` nomeando a origem, de modo que `memory_lint` continua relatando-a
até que uma pessoa ou agente reconcilie a prosa combinada. Cada versão anterior de destino e
origem permanece sob o caminho de destino e fica disponível para histórico e
ferramentas de restauração.

O SQLite confirma os históricos de página, sessões, observações, handoffs,
embeddings, workstreams e índices auxiliares de projeto em uma única transação
do escritor. A trava de mutação da wiki tira um snapshot da árvore de destino antes de instalar
os arquivos mesclados e a restaura se a transação for recusada; após uma confirmação, ela
remove a árvore de origem agora vazia, atualiza manifestos de escopo e cria ponto de verificação
para o resultado.

### `move-project`

```bash
ai-memory move-project --from-workspace default --project my-project \
  --to-workspace other-workspace --confirm
```

Move um projeto para um espaço de trabalho **diferente**. Diferente de `rename-project`
(uma atualização de coluna no mesmo espaço de trabalho), este cruza o limite do espaço de trabalho.
O destino decide qual das duas estratégias é executada, relatada como
`moved_via` na resposta:

**1. Destino novo → `"true-move"` (sem perdas, o caso comum).**
Quando o espaço de trabalho de destino **não** possui nenhum projeto com o mesmo nome, a movimentação é
uma recarimbagem de baixo nível:

1. Resolve a origem `(from_workspace, project)`. 404 se não encontrar.
2. Rejeita `from_workspace == to_workspace` (use `rename-project`) → 422.
3. Obtém ou cria a linha do **espaço de trabalho** de destino (não um novo projeto).
4. Adquire a barreira de mutação exclusiva da wiki e executa os webhooks de admissão `op=move_project`
   com nomes de origem em `ctx.workspace` / `ctx.project` e
   nomes de destino em `ctx.destination_workspace` /
   `ctx.destination_project`. Um webhook com política reject aborta antes que arquivos ou linhas
   do banco de dados sejam movidos.
5. Enquanto ainda mantiver essa barreira, verifica se o diretório de destino ainda está
   ausente e, em seguida, executa `fs::rename` no diretório do projeto
   `<wiki>/<from_ws>/<proj>` → `<wiki>/<to_ws>/<proj>` (atômico dentro de uma
   única raiz de wiki).
6. Recarimba `workspace_id` em todas as tabelas de domínio para o projeto em
   **uma transação**, mantendo o mesmo `project_id`
   (`projects`, `pages`, `sessions`, `observations`, `handoffs`, `audit_log`,
   estado de auto-aprimoramento, tarefas de consolidação de SessionEnd, `workstreams`
   gerenciados, mensagens entre projetos pendentes em ambas as extremidades, entidades, feedback
   de página, registros de descarte de expurgo e as evidências de perfil). Sessões, execuções e
   eventos nativos de workstream permanecem vinculados
   por meio de `workstream_id`; `page_embeddings` e `links` permanecem vinculados
   por meio de `page_id`, de modo que nenhuma dessas linhas precise de uma nova carimbagem direta.

Após uma movimentação real bem-sucedida na CLI, ou uma movimentação com cópia e expurgo concluída, o cliente
reindexa a chave de seu link local de checkout em `show`. Links de destino existentes prevalecem durante uma
mesclagem. Chamadores diretos da API de administração deixam registros locais de clientes intactos; uma
execução gerenciada posterior bem-sucedida (`run`) repara o link relevante.

A ordem é **renomeação PRIMEIRO, confirmação no SQL por ÚLTIMO**, de modo que o **banco de dados nunca esteja à frente do
disco**: uma falha na renomeação não toca em nada; um travamento entre as duas etapas deixa
no máximo um diretório órfão no destino com o banco de dados ainda totalmente na origem
(recuperável), nunca uma linha do banco de dados apontando para um arquivo ausente. Uma falha no SQL renomeia
o diretório de volta, portanto a movimentação é do tipo tudo-ou-nada, a menos que o sistema de arquivos também recuse
a reversão, caso em que o erro indica o reparo manual. Gravações/reindexações de página
em andamento no processo utilizam o lado compartilhado da mesma barreira de mutação e validam o
par `(workspace_id, project_id)` antes de tocar no disco, para que gravações obsoletas da origem
falhem sem criar arquivos órfãos após a movimentação.

Isto é O(1) (uma transação + uma renomeação), não recalcula nenhum embedding e preserva tudo: sessões, observações, handoffs e todo o
histórico de substituição viajam com o projeto.

**Proteção de sessão ativa.** O servidor recusa (409) mover o projeto que o
roteador de hooks publicou como o projeto *ativo* (a próxima observação de uma
sessão ativa carregaria um `workspace_id` agora obsoleto). Passe `--force` /
`force: true` para ignorar. Isso ainda é seguro: a movimentação republica o ponteiro
ativo, e o validador de par da wiki mais o gatilho de inserção de `(workspace_id, project_id)`
(V18) rejeitam gravações obsoletas de forma limpa, para que o roteador resolva novamente em vez de
corromper ou criar arquivos no espaço de trabalho antigo.

**2. O destino já possui um projeto com o mesmo nome → `"copy-purge"`
(mesclagem).** Dois `project_id`s distintos não podem ser recarimbados em um só (isso
colidiria em `UNIQUE (workspace_id, name)`), portanto as páginas mais recentes da origem
são copiadas para o projeto de destino existente por meio de
`Wiki::write_page` (higienização, nova resolução de links, FTS e, na
implantação, os webhooks de admissão/espelho git são todos disparados), os embeddings de origem
são transferidos literalmente, e somente então a origem é expurgada
(`merged_into_existing: true`, `source_purged: true`).

`--force` substitui apenas a proteção de projeto ativo do roteador de hooks. Ele nunca
exclui um lease de workstream gerenciado ativo durante este caminho destrutivo; conclua
ou cancele essa execução antes de tentar novamente. Uma movimentação real mantém o mesmo projeto e
os mesmos ids de lease, portanto não precisa dessa proteção adicional.

Copiar antes de expurgar significa que qualquer falha na cópia aborta **antes** do expurgo,
deixando a origem intacta. Um arquivo de origem ilegível é ignorado e também
bloqueia o expurgo (`source_purged: false`) para que uma nova execução corrigida seja segura
(executar novamente é idempotente; páginas copiadas apenas substituem). Uma **execução gerenciada
ativa** sob a origem bloqueia a etapa de expurgo da mesma forma: a movimentação retorna
409 informando quantas páginas já foram copiadas, e a origem permanece intacta
até que a sessão termine e a movimentação seja executada novamente.

**Conflitos no mesmo caminho (`on_conflict`).** Quando o caminho de uma página de origem já
existe no destino com corpo, frontmatter, título, nível (tier) ou bit fixado (pinned) diferente,
a política decide (páginas idênticas são sempre uma substituição sem efeito (no-op)
no mesmo caminho):

- **`block`** (padrão): aborta toda a movimentação com 409, listando os
  caminhos em conflito; a origem é deixada intacta. O padrão seguro para uma
  operação destrutiva: nada é sobrescrito ou dividido silenciosamente. O operador
  resolve os conflitos ou executa novamente com uma política explícita.
- **`overwrite`**: a página de origem substitui a página de destino no
  mesmo caminho (a versão anterior do destino torna-se histórico).
- **`duplicate`**: mantém ambas. A página de origem fica em
  `<stem>-from-<src_workspace_slug>.md`, depois `-2`, `-3`, … em
  colisões adicionais. O literal `-from-` é a constante `DEDUP_FROM_TOKEN`
  em `crates/ai-memory-mcp/src/admin.rs`; se você algum dia
  alterar um, altere o outro. Wikilinks apontando para o caminho original
  não são reescritos, portanto o caminho sem perdas `true-move` continua sendo
  a maneira de preservar caminhos e links.

Todo conflito (substituição/duplicação) é listado na matriz `conflicts` da resposta
(`path` → `moved_to`). Defina a política via `--on-conflict` na CLI
ou `"on_conflict": "block" | "overwrite" | "duplicate"` no corpo JSON
para chamadores diretos de `/admin/move-project`. Se a resolução de um destino existente com suporte
à identidade promover seu nome legado primeiro, a resposta incluirá
`manifest_warning` quando a atualização de manifesto/ponto de verificação dessa promoção confirmada
falhar; a CLI a imprime.

**O que NÃO migra (apenas no caso de mesclagem):** o caminho `copy-purge` copia
apenas as páginas mais recentes da origem e seus embeddings. O expurgo então descarta
tudo o mais que o projeto de origem possui, o mesmo conjunto que `purge-project`
exclui: suas `sessions`, `observations` e `handoffs` (o log de captura episódica bruta),
seus workstreams gerenciados com suas sessões nativas, execuções, eventos e segmentos
`raw/workstreams/`, as mensagens entre projetos que enviou ou recebeu, o estado de auto-aprimoramento
e de colheita de perfil, tarefas de consolidação de sessão, chaves de deduplicação de ingestão, entidades,
feedback de página, registros de descarte de expurgo por sessão e concessões de projeto. As páginas movidas
iniciam uma nova cadeia de substituição (o verdadeiro histórico de páginas reside no espelho git da wiki).
A trilha de auditoria append-only permanece, com referências aos ids de origem removidos.
O caminho `true-move` não possui tal perda.

> **Ressalva operacional: mover o projeto no qual a sessão atual grava.**
> Os hooks de ciclo de vida carimbam uma observação delimitada a cada evento compatível do ciclo de vida de ferramentas no
> projeto da sessão. Se você mover esse mesmo projeto no meio da sessão, o próximo
> hook recria a origem (no estilo `scratch`) sob o espaço de trabalho antigo.
> Antes de mover um projeto ativo, aponte o `.ai-memory.toml` do repositório para o
> espaço de trabalho de **destino** primeiro, para que novos eventos de hook já cheguem lá
> e a movimentação seja uma operação limpa e sem contenção.

Modos de falha:

- **Faltando `--confirm`** → 400.
- **`from_workspace == to_workspace`** → 422 (use `rename-project`).
- **Projeto de origem não encontrado** → 404.
- **Diretório do espaço de trabalho de destino já existe** (apenas true-move)
  → 409 com corpo `WikiError::DestinationExists`: o destino possui conteúdo em disco para o mesmo par de UUIDs `(workspace, project)`
  sem uma linha correspondente no banco de dados; recusa e permite que o operador
  reconcilie manualmente.
- **Conflito de mesmo caminho na política de bloqueio (block)** (apenas na mesclagem copy-purge) → 409
  com `{"error": "...", "conflicts": [paths...]}` listando cada
  caminho conflitante. Execute novamente com `on_conflict=overwrite` ou
  `on_conflict=duplicate` para prosseguir.
- **Falha de admissão no true-move ou na nova carimbagem do SQL** → 500 e nenhuma
  movimentação confirmada. Se ocorrer uma falha dupla rara de rollback após o
  diretório ser movido, mas antes da confirmação no SQL, o erro inclui o reparo
  manual exato.

### `move-session`

```bash
# One session, page and history included (dry run: no --confirm)
ai-memory move-session 0192b6a1-4c2e-7d3f-8a5b-1234567890ab --to NAS_general
# Apply
ai-memory move-session 0192b6a1-4c2e-7d3f-8a5b-1234567890ab --to NAS_general --confirm
# Every session of a stray project, into another workspace, page regenerated later
ai-memory move-session --from-project tmp --to NAS_general --to-workspace home \
  --pages regenerate --confirm
```

Move uma sessão, ou cada sessão que toca em `--from-project`, para outro
projeto, no mesmo ou em outro espaço de trabalho. Use quando uma sessão foi
capturada sob o projeto errado (um `cd` para `/tmp` antes do roteamento fixo (sticky),
um subagente iniciado em um diretório temporário, um repositório clonado com um nome
temporário) e suas observações devem residir com o projeto a que se referem.
Diferente de `reorg`, que re-deriva o projeto de cada sessão a partir de seu `cwd`,
este nomeia o destino explicitamente e deixa o restante do repositório em paz.
Sessões já enraizadas no destino apenas têm suas linhas perdidas reacomodadas:
a linha de `sessions` permanece, cada linha delas que ainda resida em
outro escopo é reunida no destino, e o relatório diz
`session_moved: false` com as contagens. A forma em lote, portanto, enxerga uma
sessão por meio de uma linha de `sessions` na origem OU observações carimbadas
nela, que é o que esvazia um balde fantasma que contém apenas observações de
sessões enraizadas em outros lugares (roteamento no meio da sessão antes do sticky).

**Solicitação.** `POST /admin/move-session` aceita ou
`{"session_id": "<uuid>", "workspace"?, "project", "pages"?, "confirm"?,
"force"?, "create"?}` ou a forma em lote `{"from_workspace"?, "from_project",
"workspace"?, "project", "pages"?, "confirm"?, "force"?, "create"?}`.
`workspace` (o destino) assume como padrão o espaço de trabalho de origem;
`from_workspace` assume como padrão `default`. A CLI resolve `--from-project`
como qualquer outro argumento de escopo (marcador, senão literal); `--to` e
`--to-workspace` permanecem literais.

**O que se move, por sessão, em UMA transação:** a linha de `sessions`, cada
linha de `observations` da sessão (incluindo qualquer uma que tenha caído em outro
escopo), os `handoffs` que ela produziu (`from_session_id`), suas
`session_consolidation_jobs`, `auto_improve_runs` e
`auto_improve_scheduler_claims`, suas `profile_candidates` de prompt de perfil
(`source_kind = 'prompt' AND source_ref = 'session:<id>'`), suas
`ingest_keys` e sua página `sessions/<id>.md`:

- `--pages move` (padrão): cada versão da página é recarimbada no
  destino e o arquivo é renomeado para o diretório do projeto de destino,
  de modo que a página com curadoria e seu histórico de substituição acompanhem a
  sessão. Recusado com `409` quando o destino já possui uma página mais recente
  (ou um arquivo de página) nesse caminho; tente novamente com `--pages regenerate` ou resolva
  a página de destino primeiro.
- `--pages regenerate`: as versões de origem são aposentadas (`is_latest = 0`),
  o `summary_page_id` da sessão é limpo quando apontava para elas, e o
  arquivo é removido, de modo que a próxima consolidação da sessão grave uma nova
  página no destino. O arquivo precisa sair: um arquivo de página deixado em
  disco sem nenhuma linha recente é reindexado como uma nova página mais recente pela próxima
  passagem de reconciliação.

A linha de auditoria (`op = move_session`) traz o operador quando a solicitação
foi atribuída. O que NÃO se move: `sessions.cwd` (verdade histórica; a
resposta carrega `cwd_warning` quando seu nome base não é o projeto de destino,
já que novas sessões iniciadas lá ainda resolvem por nome base, a menos que um
marcador `.ai-memory.toml` fixe o projeto), outras páginas gravadas durante a
sessão (decisões, gotchas: páginas não são rastreadas por sessão), handoffs que a
sessão *aceitou*, e `auto_improve_proposals` (elas não possuem `session_id`;
elas têm como alvo páginas no escopo em que foram preparadas). `entities` e
`page_feedback` também não são recarimbados, ao contrário de `move-project`, que
move todo o projeto e recarimba ambos.

**Ordem das operações (por sessão):** valida o destino (404 a menos que
`create`), rejeita um lote cuja origem seja o destino (422; a forma
única com o próprio projeto da sessão é uma reacomodação), executa as proteções (a
proteção de sessão aberta não se aplica a uma reacomodação, cuja linha não se move),
depois, sob a
barreira de mutação exclusiva da wiki, dispara webhooks de admissão `op=move_session`
(nomes de origem em `ctx.workspace`/`ctx.project`, nomes de destino em
`ctx.destination_*`), move o arquivo e recarimba o SQLite. O disco vai primeiro
e o SQL por último, como em `move-project`: uma falha no armazenamento renomeia o arquivo de volta,
portanto a movimentação é tudo-ou-nada, a menos que o sistema de arquivos também recuse a
reversão, caso em que o erro nomeia o reparo manual. Uma execução com `--confirm`
cria um ponto de verificação da wiki antes e depois; um lote cria um único par para
toda a execução.

**Simulação (dry run) por padrão.** Sem `--confirm`, o servidor executa a mesma
transação e a reverte, de modo que as contagens na resposta são exatas
(`dry_run: true`), e a CLI imprime o comando exato para aplicar. Nada
é gravado e nenhuma linha de auditoria é deixada; com `--create` contra um destino
que ainda não existe, a simulação também não o cria: ela relata
`would_create_project: true` com as contagens do que seria movido (a CLI
diz "would create project"), e apenas a execução com `--confirm` o cria.

**Proteções (409 a menos que `--force`).** Uma sessão que ainda está aberta (sem término
de sessão registrado) ainda pode receber eventos; uma tarefa de consolidação `pending`/`running`
gravaria a página sob o escopo antigo; a forma em lote também recusa
o projeto que o roteador de hooks publicou como ativo, assim como `move-project`.
`--force` prossegue. Uma reacomodação (a linha da sessão já reside no
destino) pula a proteção de sessão aberta: a linha não se move, portanto uma
sessão aberta apenas tem suas linhas perdidas reunidas; a proteção de tarefas ainda se aplica. Observe que forçar uma sessão aberta deixa o ponteiro ativo por sessão do roteador de hooks
no escopo antigo até que ele expire;
o roteamento fixo (sticky) continua então seguindo a linha da sessão, que agora é o
destino.

**Forma em lote.** Cada sessão do escopo de origem move-se uma de cada vez, cada
uma em sua própria transação. O lote para na primeira recusa e o corpo do erro
relata `moved`/`total` e as sessões já movidas (elas permanecem
movidas). Uma simulação do lote mostra todo o plano primeiro.

Resposta (`MoveSessionReport`): `session_id`, `dry_run`, `session_moved`
(`false` para uma reacomodação), `from`/`to`
(`{workspace, project}`), `summary` (`observations`, `handoffs`,
`consolidation_jobs`, `auto_improve_runs`, `auto_improve_claims`,
`page_versions_moved`, `pages_regenerated`), `page` (`moved` |
`regenerated` | `already in destination` para uma reacomodação cujas linhas de página residem todas
no destino | `none` quando a sessão não tem página em nenhum lugar),
`cwd`, `cwd_warning`, `would_create_project` (simulação apenas com `create`),
`pre_checkpoint` (apenas quando a árvore da wiki tinha alterações não confirmadas antes da
movimentação), `checkpoint` (apenas quando a movimentação alterou a árvore) e
`manifest_warning` quando `--create --confirm` resolve um destino legado baseado em identidade,
confirma sua promoção, mas não consegue atualizar/criar ponto de verificação de `_meta.md`.
O lote apresenta o mesmo aviso e envolve os relatórios em `{dry_run,
would_create_project?, from, to, total, moved, sessions: [...]}`.

Modos de falha:

- **Nem `session_id` nem `from_project`, ou ambos** → 400.
- **Sessão ou destino não encontrado** → 404 (passe `create` / `--create`
  para criar o destino).
- **Origem do lote é igual ao destino** → 422 (a forma única com o
  próprio projeto da sessão é uma reacomodação, 200 com `session_moved: false`).
- **Sessão aberta (a menos que a linha já resida no destino), tarefa de consolidação pendente
  ou projeto de origem ativo (lote)** → 409 sem
  `force`.
- **Página mais recente ou arquivo de página já em `sessions/<id>.md` no
  destino** (`--pages move`) → 409; nada é movido.
- **Falha de armazenamento após o arquivo ser movido** → 500, arquivo renomeado de volta; o
  erro nomeia o reparo manual se essa reversão também falhar.

**Ele drena todos os escopos, não apenas aquele que você nomeou.** Linhas dependentes são
correspondidas apenas pelo id da sessão, portanto uma sessão cujas observações foram espalhadas
entre projetos (roteamento no meio da sessão pré-`sticky`, por exemplo) é reunida por
completo. Esse é o objetivo do comando, mas significa que uma movimentação pode esvaziar um
projeto que você não mencionou, e ela **não é reversível de forma limpa**: mover a
sessão de volta mais tarde retorna todas as linhas para um único projeto, e a divisão
original por linha é perdida.

A simulação, portanto, nomeia cada escopo que drenaria, com contagens:

```text
  gathering observations out of 2 scopes into default/acme-api:
    default/scratchpad: 3 observation(s)
    default/tmp: 1 observation(s)
  Note: this empties every scope listed above, not only the one named as the
  source. Moving the session back later returns all rows to a single project —
  the split shown here is not restored.
```

`POST /admin/move-session` relata a mesma lista como `source_scopes`. Leia-a
antes de confirmar.

### `reclaim-ledger-versions`

```bash
# Dry run — reports how many superseded ledger versions (and bytes) would go:
ai-memory reclaim-ledger-versions
# Apply, and rewrite the database to actually free the disk:
ai-memory reclaim-ledger-versions --confirm --compact
```

Antes do #660, o indexador da wiki armazenava cada reescrita de um livro-razão de eventos OKF
(`log.md`, `log-YYYY-MM.md`) como uma nova versão de página. Em um repositório ocupado, essas
versões substituídas dominam o banco de dados (um relatório: ~95% de um banco de dados de 45 GB).
Este comando online exclui exatamente esse resíduo: uma versão de página é removida apenas
quando **não** é a mais recente, não é uma versão de declínio por retenção, seu caminho corresponde
ao formato do livro-razão **e** seu próprio corpo começa com uma entrada de hook de livro-razão `## [timestamp]`, para que uma página genuína que apenas por acaso se chame `log-2026-09.md`
nunca seja tocada, e nenhuma página ativa/mais recente seja afetada. A exclusão é executada por meio do
único ator escritor em uma transação; linhas derivadas (embeddings, links,
feedback) são excluídas em cascata, e o ponteiro anterior de substituição da versão mais recente mantida é
anulado em vez de excluído em cascata, de modo que a página sobrevivente permaneça acessível.

É uma simulação (dry-run) por padrão e requer `--confirm` para gravar. Assim como `purge-*`, a
exclusão lógica por si só não reduz o arquivo: passe `--compact` para reconstruir o
índice FTS e executar `VACUUM`. `--drop-latest` (também filtrado por conteúdo) recupera
adicionalmente a versão mais recente do livro-razão quando você não precisa mais do livro-razão na wiki.

### Varredura do livro-razão de eventos (`forget-sweep`, opcional)

```bash
# Preview: which ledgers and how many bytes the sweep would delete.
ai-memory forget-sweep --dry-run
# Enable it (config.toml) — the sweep then deletes them on its schedule too:
#   [decay]
#   ledger_retention_months = 12
```

Os livros-razão de eventos de hooks (`log-YYYY-MM.md`, um por projeto por mês) são uma
trilha de auditoria somente de acréscimo (append-only) que nenhuma superfície do ai-memory lê de volta: o indexador,
a consolidação, a migração OKF e a exportação os ignoram. Em um projeto movimentado, um
único mês pode conter centenas de megabytes. Com `[decay]
ledger_retention_months = N` (variável de ambiente `AI_MEMORY_DECAY__LEDGER_RETENTION_MONTHS`)
a varredura de esquecimento exclui o livro-razão de um projeto assim que `N` meses civis UTC inteiros
tiverem se passado após o término de seu mês. `0` (o padrão) mantém todos os livros-razão; um
valor negativo é rejeitado na inicialização.

O que ele excluirá e o que não excluirá:

- Um arquivo se qualifica apenas quando seu nome for `log-YYYY-MM.md` **e** seu conteúdo
  começar como um livro-razão (a mesma filtragem de conteúdo que `reclaim-ledger-versions` e o
  indexador usam). Uma página que uma pessoa nomeou como `log-2025-01.md` nunca é excluída.
- O mês atual e o mês anterior nunca são excluídos, qualquer que seja o valor de `N`:
  o anexador de hooks ainda pode gravar neles.
- `log.md` (o livro-razão não rotacionado) nunca é excluído.
- Apenas o diretório do projeto varrido é examinado. A varredura executada para um
  projeto nunca toca nos livros-razão de outro projeto.
- Uma exclusão passa pela mesma verificação de admissão bloqueante que uma exclusão de página: webhooks
  inscritos na operação `delete_event_ledger` são consultados antes do unlink,
  e um webhook com política `reject` mantém o arquivo. A varredura então o contabiliza em
  `ledgers_undeleted` no relatório e segue em frente (sendo repetida pela próxima varredura),
  exatamente como a exclusão recusada de uma página é mantida e repetida. (Os *acréscimos* por evento
  aos livros-razão ainda contornam a cadeia, como antes.)

Como ele exclui: por meio de `Wiki::delete_event_ledger`, que adquire a proteção de mutação
da wiki, verifica novamente o nome e a barreira de conteúdo, executa a verificação de admissão
bloqueante descrita acima e somente então remove o arquivo; a varredura
nunca chama `std::fs` diretamente. O anexador de hooks não adquire
essa proteção, portanto um acréscimo que ocorra no instante entre a verificação e a
remoção seria excluído com o arquivo; isso só pode afetar um mês fechado há pelo
menos `N` meses, o qual não recebe nada exceto um evento muito tardio (e esse
evento recria um arquivo pequeno). O banco de dados nunca é modificado: livros-razão não possuem
linhas em `pages` desde o #660, e quando existe uma linha legada para um livro-razão excluído, o
relatório a contabiliza em `ledger_page_rows_left` e a deixa em paz — execute
`reclaim-ledger-versions` para essas.

A varredura exclui o arquivo do disco. O repositório git da wiki mantém os
blobs que um ponto de verificação já capturou; excluir um livro-razão não diminui
o `wiki/.git` (isso exige que o repositório seja compactado, o que o ai-memory não
faz).

### `repair-backfill-timestamps`

```bash
# Dry run: report what would change for this project
ai-memory repair-backfill-timestamps --project my-app
# Apply
ai-memory repair-backfill-timestamps --project my-app --confirm
```

Corrige `sessions.started_at`/`ended_at` para sessões que um `backfill` mais antigo
já importou. O `backfill` costumava datar cada sessão importada no momento da importação
em vez de usar os horários dos próprios eventos da transcrição, nivelando todo o
histórico importado em um único dia; uma alteração complementar corrige novas importações, e este
comando repara sessões que um `backfill` anterior à correção já gravou.

A CLI é a única parte que toca no sistema de arquivos: ela relê as transcrições
locais do operador no harness em modo somente leitura (reutilizando a própria descoberta
de sessões do `backfill`, de modo que analisa exatamente as sessões que o próprio `backfill`
importaria para este checkout), e para cada uma calcula o timestamp do primeiro e do último evento da transcrição.
Ela resolve o id de sessão de cada transcrição da mesma forma que o roteador de hooks faz (`SessionId::from_native`:
um id nativo em UUID como está, qualquer outro id nativo transformado em hash para um UUID v5 determinístico),
enviando em seguida a lista de tuplas candidatas `(session_id, started_at, ended_at)` para
`POST /admin/repair-session-times`, dividida em blocos de até 2.000 candidatos por
solicitação (uma transação cada) quando o lote local for maior. O servidor é
a única parte que valida e grava.

**Solicitação.** `POST /admin/repair-session-times` aceita `{"workspace",
"project", "sessions": [{"session_id", "started_at_us", "ended_at_us"?}, ...],
"confirm"?}`. `sessions` é limitado a 2.000 entradas por solicitação para que uma solicitação de cliente
malformada ou excessivamente grande não aumente a transação do ator escritor de forma
indefinida; exceder o limite gera um 400 antes de qualquer pesquisa de escopo.

**Validação, por candidato, contra a linha que esta mesma transação lê para
ele (nunca a crença do chamador sobre ela):**

- **Contenção de escopo**, exatamente como em `purge-session`: um `session_id` que
  não pertença a `(workspace, project)` (escopo incorreto ou inexistente) é relatado como `skipped: {reason: "not_found"}` e não é tocado. Os dois casos
  nunca são distinguidos, de modo que um id existente em um projeto diferente não seja
  revelado ao chamador.
- **Um candidato sem alteração (no-op) é `"unchanged"`, não contado como reparado.** Verificado
  antes do julgamento da assinatura de nivelamento abaixo, de modo que uma repetição exata seja
  sempre uma não-operação, independentemente de a linha parecer nivelada ou não.
- **A própria assinatura do bug é o critério de entrada, não o que o chamador afirma.** Uma linha só
  é reescrita quando seu `started_at` atual estiver DEPOIS do próprio término do candidato
  (ou início, quando o candidato não possuir término), que é o formato real de
  "o backfill datou isso no momento da importação". Uma linha que já esteja nesse ponto ou antes
  dele é `"not_flattened"` e deixada intacta: este endpoint repara o bug específico do
  backfill, não é uma primitiva genérica de "definir horários de sessão" que reescreveria
  indiscriminadamente os horários reais de uma sessão capturada corretamente por hooks. Isso também torna uma
  reexecução contra uma sessão já reparada, ou que um `backfill` corrigido importou em primeiro lugar,
  uma verdadeira não-operação.
- **Horários válidos**: `started_at_us` e `ended_at_us` (quando fornecidos) devem ser
  positivos, e `started_at_us` não deve ser posterior ao término que a linha terá de fato
  assim que o candidato for aplicado (seu próprio novo término ou, quando este permanecer
  não gravado, qualquer término que a linha já tinha); caso contrário,
  `"invalid_time"` ou `"inverted_times"`.
- **Nunca no futuro**: qualquer horário com mais de cinco minutos após o momento "atual" é
  `"future_time"`. Margem para desvio de relógio (clock skew), não um alvo de correção real.
- **Nunca encerra uma sessão aberta**: quando o `ended_at` atual da sessão for
  `NULL`, o `ended_at_us` do candidato é silenciosamente retido; `started_at` ainda é aplicado, e o relatório marca essa sessão como `end_kept_open: true`
  em vez de ignorá-la por completo.

**Simulação (dry run) por padrão.** Sem `confirm: true`, o servidor valida e
calcula a gravação exata que seria feita dentro de uma transação que ele então reverte, de modo que
o relatório (`repaired`, `skipped`, horários de antes/depois) seja o resultado literal,
não uma estimativa (mesmo padrão do `move-session`). `--confirm` aplica por meio do
único `WriterHandle`, uma transação por solicitação (bloco).

**O que ele altera.** Apenas `sessions.started_at`/`ended_at` de linhas já existentes no
escopo solicitado, nunca `observations`, páginas ou qualquer outra tabela, e
nunca uma linha fora de `(workspace, project)`. A identidade da sessão (`sessions.id`
e sua tupla de escopo de 3 elementos) nunca é regravada; esta é uma correção de duas
colunas de carimbo de data/hora em linhas que já existem, não uma movimentação ou exclusão.
`observations.created_at` e os carimbos de data/hora do frontmatter da própria página `sessions/<id>.md`
ainda mantêm os horários antigos do dia da importação após um reparo. Apenas as
duas colunas da linha de `sessions` são corrigidas; regenerar a página da sessão
(por exemplo, via uma nova consolidação) é o que colocaria seu frontmatter em conformidade.

**Reversibilidade.** Um lote confirmado grava uma linha em `audit_log` (`op =
"repair_session_times"`) por solicitação com os horários de antes/depois de cada sessão reparada
em `detail`, e o relatório legível por humanos da CLI imprime os mesmos valores de antes →
depois por sessão. Um operador que tenha guardado qualquer um deles pode restaurar os valores anteriores
manualmente; não há desfazer automático.

### `checkpoints`

```bash
ai-memory checkpoints
```

Lista commits recentes do git da wiki, do mais novo para o mais antigo. O OID curto é suficiente para
`restore-page`, mas a saída JSON inclui o OID completo:

```bash
ai-memory checkpoints --json
```

Para que serve:

- Encontrar o ponto de verificação logo antes de uma gravação, exclusão, expurgo, movimentação ou
  restauração de página incorreta.
- Inspecionar o histórico da wiki sem precisar acessar o repositório `wiki/.git` do servidor via terminal.

A inicialização cria um commit único `upgrade baseline: existing wiki tree before recovery
checkpoints` para diretórios de dados existentes cujo repositório da wiki tenha zero commits.
Instalações novas e vazias continuam sem commit até que haja conteúdo para salvar.

### `restore-page`

```bash
ai-memory restore-page --workspace default --project my-project \
  --path notes/foo.md --from <checkpoint>

# Restore a retained page version after merge-project moved its history
ai-memory restore-page --workspace default --project canonical \
  --path notes/foo.md --page-id <page-version-uuid>
```

Cada flag de escopo omitida é resolvida a partir do diretório atual; um valor explícito
não vazio tem precedência sobre esse padrão. Uma flag explicitamente vazia ou contendo
apenas espaços em branco é recusada antes de qualquer solicitação, para que uma variável de shell não definida
não possa restaurar acidentalmente uma página no projeto do checkout.

O que acontece:

1. O servidor resolve `(workspace, project)` sem criar nada automaticamente.
2. O servidor valida o caminho da página.
3. O servidor cria um ponto de verificação da árvore atual da wiki primeiro (`pre-restore-page ...`) quando
   houver alterações não confirmadas.
4. O servidor lê o blob markdown exato para aquele projeto/página do git
   em `--from`, ou a versão retida do SQLite identificada por `--page-id` após
   verificar que ela agora pertence ao projeto e caminho solicitados. Ele compartilha
   o bloqueio do mesmo caminho com gravações e reindexação do observador, incluindo a leitura do
   frontmatter atual usado para preservar `generated.at`, depois grava o conteúdo
   e faz upsert de uma nova linha de página mais recente para que busca, links e `/web` concordem com o disco.
5. O servidor grava um ponto de verificação pós-restauração (`restore-page ...`) quando a árvore ativa
   for alterada.

Modos de falha:

- **Nome do espaço de trabalho ou projeto não encontrado** → 404, nenhuma mutação.
- **Caminho de página inválido** → 422, nenhuma mutação.
- **Ponto de verificação ou arquivo não encontrado** → 500 com o erro do git/libgit2; qualquer
  ponto de verificação pré-restauração permanece como rastro de auditoria.
- **UUID da versão da página está malformado** → 400 antes de um ponto de verificação ou mutação.
- **Versão da página ausente ou pertence a outro projeto/caminho** → 404.
- **Resumo da sessão pertence a uma sessão expurgada** → 409 para qualquer uma das origens;
  nenhuma linha de página é adicionada e qualquer arquivo recém-instalado é removido. Resumos
  legados sem `session_id` também são recusados, a menos que uma substituição manual
  ativa sobreviva nesse caminho. Uma página manual ativa atual permanece
  restaurável. Uma versão manual retida mais antiga por si só não autoriza uma
  restauração: sem uma substituição ativa, ela não pode ser distinguida de um
  resumo legado sem `session_id` (#701). O MCP `memory_restore_page`
  aplica a mesma proteção às suas restaurações de ponto de verificação.
- **Markdown histórico está malformado ou não é UTF-8** → 500, o arquivo ativo não é
  substituído.
- **SQLite rejeita a versão restaurada** (por exemplo, o caminho histórico
  colide com uma página atual sob conversão de maiúsculas/minúsculas) → 500, o arquivo anterior é
  restaurado ou um arquivo recém-criado é removido. Se essa reversão também falhar,
  o erro relata explicitamente um estado inconsistente que requer reparo.

O que ele não recupera:

- Sessões, observações, handoffs, usuários, linhas de auditoria, contadores de acesso e
  embeddings. Esses residem apenas no SQLite e requerem um `backup` / `restore` completo
  se você precisar revertê-los.

### MCP `memory_restore_page`

O equivalente voltado para agentes do `restore-page`. Sem `rev`, ele lista os pontos de verificação
do git que alteraram uma página, do mais novo para o mais antigo, incluindo o que a
excluiu; com `rev`, ele regrava essa versão como a nova versão atual
da página.

Ele difere do `restore-page` na forma como o texto entra novamente. O corpo
e o frontmatter do ponto de verificação (`title`, `tier`, `tags`, `pinned`, `expires_at`, `kind`,
`entities`, `abstract`, `relations`, `applies_to`, `enforced_by`,
`source_files`) passam pelo
mesmo caminho que `memory_write_page`: o higienizador, a cadeia de admissão, a atribuição
de gravação e a substituição se aplicam, e a versão que ele substitui permanece
acessível. Metadados que `memory_write_page` recusaria são recusados aqui também,
antes que qualquer coisa seja gravada. Outras chaves de frontmatter não são transferidas.
O escopo é resolvido uma única vez, sem criar nada, e a listagem, a leitura
e a gravação usam todas esse único projeto.

Antes de gravar, `memory_restore_page` verifica o frontmatter original do ponto de verificação,
incluindo `session_id`, contra o registro de descarte (tombstone) de expurgo de sessão.
O resumo de uma sessão expurgada é recusado com um erro de parâmetros inválidos,
deixando disco, índice e git inalterados. Uma página manual ativa atual no
mesmo caminho permanece restaurável; uma versão manual retida mais antiga sem uma
substituição ativa é recusada porque é indistinguível de um resumo legado
sem `session_id`. Listar pontos de verificação e ler o histórico permanecem
disponíveis para administradores.

Quem pode chamá-lo: um administrador, a mesma autoridade de `/admin/restore-page`. O histórico
do git mantém todas as versões que a wiki ativa descartou, incluindo páginas que um root
excluiu, páginas que um expurgo de sessão removeu e páginas expiradas que a varredura de esquecimento
excluiu definitivamente, portanto a listagem pode apontar um texto que uma restauração deve recusar. Um usuário
do banco de dados ou operador via proxy é recusado para ambos, mesmo em um projeto que possa gravar
e até mesmo para seu próprio slot por usuário. Em um servidor que não diferencia operadores
uns dos outros (loopback anônimo ou um token bearer root sem usuários), o chamador é root
e nada muda. Sobre stdio não há camada de autenticação, e o chamador já
possui o diretório de dados.

Limites:

- Apenas páginas de projeto. Páginas gravadas com `scope: "profile"` residem no
  perfil do operador e não são endereçáveis aqui; restaure-as com
  `restore-page` no projeto do perfil.
- A listagem percorre no máximo 5.000 commits a partir de `HEAD`, fora do tempo de execução assíncrono,
  portanto uma página intocada por mais tempo do que isso não lista pontos de verificação. Use
  `ai-memory checkpoints` e `restore-page --from` para históricos mais antigos.
- Um `expires_at` restaurado é mantido como gravado. Se já estiver no passado, a página
  é ocultada novamente de imediato e removida pela próxima varredura de esquecimento; restaure uma
  página expirada com `memory_write_page` e um novo prazo de expiração.

### Livros-razão de eventos de hooks não estão no histórico git da wiki

Os livros-razão de eventos brutos de hooks (`log.md` e `log-YYYY-MM.md` sob cada projeto)
são logs somente de acréscimo que podem atingir centenas de megabytes. O git armazena cada nova
versão de um arquivo como um blob completo, e o servidor nunca empacota ou limpa o repositório
da wiki, de modo que cada ponto de verificação após um acréscimo de hook adicionava outra cópia
do livro-razão: o `wiki/.git` de uma instalação cresceu para 3,49 GB. Nada lê o histórico do
livro-razão de volta a partir do git: `restore-page` e `checkpoints` operam em páginas, e
`backup` copia o diretório de dados a partir do disco.

Desde esta alteração, um ponto de verificação deixa os livros-razão fora do commit. Um arquivo conta como
um livro-razão apenas quando seu nome tem o formato de livro-razão **e** seus primeiros 8 KiB começam
com uma entrada de log de hook (`## [timestamp] ...`, sob frontmatter opcional), o
mesmo teste que o indexador e o `reclaim-ledger-versions` usam. Uma página que simplesmente é
chamada `log-2026-09.md` permanece versionada. O arquivo do livro-razão em si nunca é tocado.
Um livro-razão que uma versão anterior já confirmou é removido do índice do git
(`git rm --cached`) pelo primeiro ponto de verificação que o vir, de modo que instalações existentes
parem de crescer sem nenhuma intervenção sua. A verificação é executada quando um ponto de verificação prepara
arquivos e faz uma leitura de prefixo limitada (no máximo 8 KiB) por verificação, nunca o
arquivo inteiro; um livro-razão anexado entre essa leitura e o commit simplesmente não entra
no commit.

O que isso **não** faz: o histórico já gravado mantém seus blobs antigos, portanto
`wiki/.git` não encolhe sozinho, e pontos de verificação criados antes da atualização
ainda contêm versões antigas do livro-razão. O servidor nunca reescreve ou faz coleta de lixo
desse histórico. Reduzi-lo é uma tarefa do operador e é destrutiva: cada
id de commit muda, de modo que OIDs anotados de `ai-memory checkpoints` deixam de resolver,
e `git gc --prune=now` torna os blobs antigos irrecuperáveis. Pare o servidor, faça um
`backup` primeiro e só então, como exemplo:

```bash
cd <data-dir>/wiki
# Let the upgraded server take one checkpoint first, so HEAD no longer tracks the
# ledgers, and copy them aside anyway: a history rewrite resets the working tree.
# The globs match by name only. If you keep an ordinary page named like a
# ledger, they also delete that page's history, so narrow them first.
git filter-repo --force --invert-paths \
  --path-glob '*/log.md' \
  --path-glob '*/log-[0-9][0-9][0-9][0-9]-[0-9][0-9].md'
git reflog expire --expire=now --all
git gc --prune=now --aggressive
```

Inicie o servidor novamente depois disso. O `git filter-repo` é uma ferramenta separada e não
é instalado com o ai-memory.

### `backup`

```bash
ai-memory backup --to /tmp/ai-memory-backup.tar.gz
```

O que acontece no servidor:

1. A API de backup online do SQLite copia o banco de dados WAL ativo para um arquivo temporário,
   um snapshot garantidamente consistente sem parar o escritor.
2. O servidor compacta em tar-gzip o snapshot + a árvore da wiki + `<data_dir>/config.toml`
   quando esse arquivo existir.
3. O corpo da resposta É o tarball compactado com gzip
   (`Content-Type: application/gzip`).

A CLI grava o corpo da resposta em `--to`. Para um usuário de homelab,
esta é a jogada padrão de "tirar um snapshot antes de fazer algo perigoso":
execute `ai-memory backup` primeiro e depois prossiga.

Este tarball é um **snapshot de dados** online, não uma imagem completa do host. Ele
intencionalmente exclui `auth-tokens/`, os arquivos `auth-token` e `auth-header`
do lado dos hooks, `raw/`, logs, modelos e uma configuração passada de fora do diretório de dados
com `--config`. O `<data_dir>/config.toml` incluído é arquivado
literalmente e pode ele próprio conter credenciais em linha de root/proxy/recuperação, o
pepper de chave de API ou cabeçalhos confidenciais de LLM. Sempre proteja o snapshot como um
artefato portador de segredos. Ele também contém código executável: uma configuração pode nomear comandos
que o servidor executa (`[auto_improve.eval]`), enviar memória consolidada para outro
host (URLs e cabeçalhos de provedores de LLM, webhooks de admissão) e enfraquecer a redação
(`[sanitize]`), de modo que o `restore` o adota apenas quando solicitado (veja abaixo). Excluir
os arquivos separados de credenciais limita o material de autenticação
copiado; omitir `raw/` evita concorrência com o livro-razão de workstream gerenciado
ativamente anexado. O snapshot do SQLite ainda contém os eventos pesquisáveis do
workstream gerenciado, mas perde sua cópia de origem imutável.

Para uma migração para outro host, leve estes itens ao lado do snapshot por meio de
um backup criptografado separadamente ou com controle de acesso:

- o arquivo de configuração que o serviço realmente carrega, especialmente
  `[auth].token_pepper`; unidades empacotadas do systemd o mantêm fora do diretório de dados;
- `auth-tokens/` se os tokens root bearer existentes precisarem continuar funcionando (caso contrário, execute
  `ai-memory auth enable` e reconecte os clientes com o substituto);
- `raw/` quando o livro-razão de origem imutável do workstream gerenciado precisar ser mantido.

Não copie os arquivos de credenciais de hook como a configuração de conexões do destino. Execute
`install-hooks --apply` e `install-mcp --apply` lá para que caminhos, URLs de servidores
e credenciais correspondam ao novo host. Trate tanto o snapshot comum quanto o
material separado como confidenciais: o conteúdo da wiki e o SQLite podem conter dados privados
de sessão, embora o arquivo exclua arquivos bearer.

Os bancos de dados atuais trazem um verificador Argon2id para o pepper da chave de API. Se um
banco de dados restaurado contiver credenciais de API nativas, mas o destino carregar um
pepper diferente, o `serve` recusa em vez de iniciar com chaves que nunca poderão se
autenticar. Um banco de dados aberto pela primeira vez por uma versão anterior a esse verificador
não pode provar qual pepper criou seus hashes existentes; para esses snapshots mais antigos,
restaurar a configuração externa original permanece obrigatório. A restauração
recusa tal banco de dados até que o operador forneça esse pepper original e
adicione `--accept-legacy-api-key-pepper`, o que sela um verificador no banco de dados
preparado antes que os dados ativos sejam alterados.

Restaurar um backup segue o processo inverso:

```bash
# Stop the server first.
docker compose -f ~/deploy/ai-memory/docker-compose.yml down
# Restore (sysinfo refuses if the container is still running).
ai-memory restore --from /tmp/ai-memory-backup.tar.gz --data-dir /var/opt/docker/utils/ai-memory/data --force
# Start back up.
docker compose -f ~/deploy/ai-memory/docker-compose.yml up -d
```

A flag `--data-dir` aponta a CLI para o caminho no lado do host do
volume docker (já que `restore` é executado diretamente no disco, não por meio da
API HTTP de administração).

### `restore`

```bash
ai-memory restore --from <tarball> --data-dir <path> --force [--with-config]
```

Operação direta em disco. Recusa se qualquer outro processo `ai-memory` estiver
ativo (usa `sysinfo` para varrer a tabela de processos).

Ordem das operações:

1. Verifica se o diretório de dados está vazio (ou se o usuário passou `--force`).
2. Extrai o tarball em um diretório de preparação ao lado dos dados ativos
   (`<data_dir>/.restore-staging-<stamp>/`), validando cada entrada e
   lendo o fluxo gzip até seu trailer de checksum e tamanho. Um trailer ausente
   ou inválido falha aqui mesmo quando cada entrada do tar for legível.
3. Exige um snapshot existente de `db/memory.sqlite` com histórico de migrações
   aplicadas do ai-memory e abre o banco de dados preparado para que migrações pendentes sejam executadas
   e o snapshot do SQLite seja verificado, ainda sem tocar nos dados ativos.
   Um banco de dados sem páginas é válido; um banco de dados ausente, de zero bytes ou não inicializado
   é recusado em vez de inicializado em uma substituição vazia.
   Um banco de dados legado com chaves de API, mas sem verificador de pepper, para aqui, a menos que
   o pepper original esteja configurado e `--accept-legacy-api-key-pepper` tenha sido passado. Quando
   a configuração ativa estiver sendo mantida (etapa 4) e o banco de dados contiver chaves de API, o
   pepper configurado deve corresponder àquele com o qual as chaves foram hasheadas, ou a
   restauração para aqui também. Isso também se aplica quando o arquivo compactado não possui
   `config.toml`, mesmo com `--with-config`.
4. Troca (swap): renomeia a `wiki/` e o `db/` ativos para o lado, renomeia as cópias preparadas
   para suas posições e depois exclui os dados anteriores. Cada movimentação é uma renomeação no mesmo
   sistema de arquivos; se uma falhar, as movimentações já feitas são revertidas. O `config.toml` do
   arquivo de backup, quando houver um, substitui a configuração ativa apenas com
   `--with-config` ou quando o diretório de dados não tiver nenhum. Caso contrário, a configuração ativa
   permanece e a arquivada é salva como `<data_dir>/config.toml.from-backup`
   (`0600` no Unix), substituindo uma cópia anterior existente.
5. Imprime um resumo, incluindo para onde foi a configuração do backup.

O `config.toml` de um backup pode executar comandos e redirecionar dados. Restaure-o, com
`--with-config` ou adotando `config.toml.from-backup`, apenas a partir de um arquivo em que
você confia; revise-o primeiro caso contrário.

Os dados ativos, portanto, permanecem intocados até que o arquivo se prove utilizável.
Uma restauração que falhe nas etapas 2–3 deixa `wiki/` e `db/` exatamente como
estavam, o que importa porque uma restauração geralmente é tentada quando nenhuma outra
cópia existe.

Modos de falha:

- **Servidor ainda em execução** → encerra com "another ai-memory process is
  alive (pid X); stop it before restoring" (mesma redação que `reset`).
- **Diretório de dados não vazio + sem `--force`** → encerra com "data dir not
  empty; pass `--force` to overwrite".
- **Tarball truncado ou corrompido, uma entrada fora da estrutura permitida ou
  um snapshot que o binário atual não consegue abrir** (por exemplo, um backup feito
  por uma versão mais recente) → encerra com o erro; os `wiki/` e
  `db/` existentes são deixados como estavam.
- **Snapshot do banco de dados ausente, vazio ou não inicializado** → encerra antes da
  troca. Uma restauração requer o `db/memory.sqlite` do arquivo e seu histórico de migrações
  aplicadas; ela nunca inicializa um novo banco de dados para preencher dados ausentes.
- **Histórico de migrações tem uma lacuna** → uma migração anterior não foi registrada,
  embora uma posterior tenha sido aplicada. O banco de dados preparado é recusado, exatamente como na
  inicialização normal. Mantenha o backup e relate o histórico para revisão;
  `--force` não contorna essa verificação nem repara a migração ausente.
- **O pepper da configuração ativa está ausente ou não corresponde às chaves de API do backup**
  (configuração ativa mantida) → defina o `[auth].token_pepper` ativo com o valor
  original. Se o backup contiver `config.toml`, restaurá-lo com
  `--with-config` é outra opção. Sem esse arquivo no arquivo compactado, leve
  a configuração original separadamente. Os dados ativos são deixados como estavam.
- **Backup legado possui chaves de API, mas nenhum verificador de pepper** → restaure o
  `[auth].token_pepper` original por meio da configuração/ambiente atual e execute novamente com
  `--accept-legacy-api-key-pepper`. Esta flag confirma a proveniência; ela não pode
  determinar se um pepper desconhecido está correto.
- **Uma renomeação na troca falha e não pode ser revertida** → encerra com uma
  mensagem `INCONSISTENT STATE` nomeando o
  diretório `<data_dir>/.restore-previous-<stamp>/` que ainda contém o
  `wiki/` e `db/` pré-restauração, para serem movidos de volta manualmente.

### `reset`

```bash
ai-memory reset --confirm
```

Operação direta em disco. Recusa se qualquer processo irmão `ai-memory` estiver
ativo. Remove o conteúdo de `wiki/`, `db/` e `raw/` sob o
diretório de dados configurado. `config.toml` é preservado.

Proteção de sysinfo idêntica à de `restore`. O caso de uso é "apagar e começar
do zero", tipicamente ao mudar de versão principal com uma migração que quebra
compatibilidade, ou ao inicializar uma nova instalação sobre um diretório de
dados antigo.

Para uma implantação em docker onde os dados residem em um bind mount no host,
você também pode simplesmente executar `rm -rf <host-path>/*` após parar o
contêiner, mas `ai-memory reset` é o caminho multiplataforma que
funciona independentemente de o diretório de dados ser local, montado via bind ou em um
volume nomeado.

### `reindex`

```bash
ai-memory reindex --data-dir <path>
```

Se a reindexação relatar um manifesto de escopo `_meta.md` ausente, o erro inclui seu caminho
exato. Restaure o banco de dados original, inicie e pare o servidor atual uma vez para que seu
backfill de inicialização grave os manifestos ausentes e tente novamente contra um banco de dados limpo.

Operação de ciclo de vida direta em disco. Recusa se qualquer processo irmão `ai-memory` estiver
ativo, e também recusa se o SQLite já contiver linhas. `reindex` é um
caminho de reconstrução a partir dos arquivos, não um reparo de índice sujo no local.

Use quando a wiki em markdown estiver intacta, mas você intencionalmente quiser uma nova
linhagem de migrações do SQLite:

1. Pare o servidor ou contêiner.
2. Faça um backup do diretório de dados atual.
3. Mova ou remova `<data-dir>/db/memory.sqlite` e seus arquivos irmãos WAL/SHM.
4. Execute `ai-memory reindex --data-dir <data-dir>`.
5. Execute `ai-memory embed` após reiniciar se precisar que os embeddings sejam reconstruídos.

O que é reconstruído:

- Espaços de trabalho e projetos a partir de `_meta.md`, preservando os UUIDs codificados nos
  nomes dos diretórios da wiki. Manifestos de projeto opcionalmente contêm a identidade completa do
  repositório com host e sua origem; o manifesto também registra
  `canonical_name` e `legacy_name` como asserções de consistência. O reindex valida o par e as
  asserções, derivando então as chaves de compatibilidade indexadas a partir da identidade.
- Linhas de páginas mais recentes, links de páginas e FTS a partir de arquivos markdown.

O que não é reconstruído:

- Sessões, observações, handoffs, usuários/tokens, linhas de auditoria, contadores de acesso
  e embeddings. Esses são estados exclusivos do banco de dados; mantenha um backup se precisar deles.

Cada diretório de escopo traz o manifesto `_meta.md` do qual `reindex` lê o
nome do espaço de trabalho/projeto. Manifestos de projeto com suporte a identidade também contêm
campos tipados opcionais `identity`, `identity_source`, `canonical_name` e
`legacy_name`. Manifestos mais antigos sem esses
campos continuam válidos e reconstroem projetos sem identidade. O manifesto é gravado
com a primeira página do escopo, de modo que um projeto que apareça pela primeira vez enquanto o servidor está
em execução é reconstruível a partir desse momento, sem necessidade de reinicialização. O backfill
de inicialização ainda é executado a cada boot e repara uma árvore gravada por uma versão anterior,
ou uma cujos manifestos tenham sido removidos manualmente. Se o `reindex` relatar um manifesto ausente,
inicie o servidor uma vez contra esse diretório de dados e deixe o backfill gravá-lo,
depois pare o servidor e reindexe novamente.

## Fluxos de trabalho do operador

### "Começo do zero" (apagar tudo)

Para uma implantação em docker / bind-mount onde os dados residem no host:

```bash
ssh homelab
cd ~/deploy/ai-memory
docker compose down
sudo rm -rf /var/opt/docker/utils/ai-memory/data/*
docker compose up -d
```

Ou via CLI a partir de qualquer máquina (mais lento, mas portável):

```bash
docker stop ai-memory   # so sysinfo guard passes
ai-memory reset --confirm   # against the same data dir
docker start ai-memory
```

### "Snapshot antes de operação arriscada"

```bash
ai-memory backup --to "/tmp/ai-memory-$(date +%Y%m%d-%H%M).tar.gz"
# … do the risky thing …
# … oh no something broke …
docker compose down
ai-memory restore --from /tmp/ai-memory-2026-05-23-1530.tar.gz --force
docker compose up -d
```

### "Descartar um projeto experimental, manter todo o resto"

```bash
ai-memory purge-project --project experimental --confirm
# Sibling projects (ai-memory, distrobox-gaming, …) untouched.
```

### "Renomear um projeto após mover seu diretório"

```bash
ai-memory rename-project --from old --to new
# Future sessions in /path/to/new will append to the same project
# (the hook router stamps by basename(cwd) = "new"); past
# observations stay under that project too because the project_id
# is stable.
```

### "Reanexar uma sessão capturada sob o projeto errado"

```bash
ai-memory move-session <session-id> --to my-project          # dry run
ai-memory move-session <session-id> --to my-project --confirm
# Or empty a stray project into the right one, then drop the husk:
ai-memory move-session --from-project tmp --to my-project --confirm
ai-memory purge-project --project tmp --confirm
```

### Histórico git da wiki ilegível após travamento

Todo identificador de repositório que o servidor abre carrega `core.fsyncObjectFiles = true`
de `wiki/.git/ai-memory.gitconfig`, um arquivo que a CLI do git nunca lê, de modo que a libgit2
sincroniza cada objeto e ref antes de renomeá-lo para sua posição, enquanto o `git status` na
wiki fica livre do aviso de descontinuação do git para essa chave. Definir
`core.fsyncObjectFiles = false` em `wiki/.git/config` desativa isso.
Antes disso, uma perda de energia ou travamento do kernel poderia manter uma atualização de ref enquanto
os objetos que ela nomeia nunca foram gravados: arquivos de comprimento zero sob `.git/objects`,
`HEAD` apontando para um deles e cada ponto de verificação posterior falhando com
`failed to parse loose object: invalid header`. As páginas no disco não são
afetadas; apenas o histórico delas é interrompido. Na inicialização, o servidor registra um `ERROR`
("wiki git history is unreadable") quando `HEAD` não resolve para um commit
legível.

A árvore de trabalho é a fonte da verdade, portanto o reparo aponta o branch de volta para
o commit intacto mais novo e permite que o próximo ponto de verificação registre tudo desde então.
Pare o servidor primeiro, depois a partir de `<data_dir>/wiki`:

```bash
git fsck --full                        # names the empty objects and the bad ref
mkdir -p ../wiki-empty-objects         # move them out, don't delete them
find .git/objects -type f -empty -exec mv {} ../wiki-empty-objects/ \;
# Newest reflog entry whose commit and tree still read back:
for oid in $(awk '{print $2}' .git/logs/HEAD | tac); do
  git cat-file -e "$oid^{tree}" 2>/dev/null && echo "$oid" && break
done
git update-ref refs/heads/main <oid>   # the branch `git symbolic-ref HEAD` names
rm .git/index && git reset -q          # rebuild the index; pages are untouched
git reflog expire --stale-fix --expire=never --all
git fsck --full                        # clean (a dangling blob is harmless)
```

Inicie o servidor novamente; o próximo ponto de verificação confirma as páginas gravadas após
esse commit.

## Por que isso importa: o incidente da wiki plana (flat-wiki)

Antes do layout de disco por projeto (commits até `e7b9a17`), a
wiki era plana: `wiki/<page-path>` independentemente do projeto. Dois
projetos com o mesmo `pages.path` compartilhavam um arquivo em disco. O
manipulador de `purge-project` então iterava e excluía esses arquivos,
destruindo páginas pertencentes ao projeto irmão. As linhas do banco de dados para o
irmão sobreviveram (a FK é delimitada por `project_id`), mas cada clique
em `/web/` retornava 404 porque o arquivo em disco havia sumido.

O paliativo enviado foi uma verificação `path_still_referenced` antes de cada
exclusão. A correção adequada chegou em `e7b9a17`: raízes de disco por projeto
tornam colisões de caminhos estruturalmente impossíveis, portanto tanto o paliativo quanto a
classe subjacente de bug desapareceram e as operações de ciclo de vida são seguras por construção.

É também por isso que `rename-project` é gratuito: o caminho em disco é indexado
pelo `project_id` substituto, não pelo nome mutável. A renomeação toca em uma
coluna; nada se move.
