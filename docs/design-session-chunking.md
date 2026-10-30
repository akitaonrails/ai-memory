# Design: processamento de sessões em chunks

**Status: implementado, com smoke local verificado.** O processamento é assíncrono e durável, com ativação explícita por configuração, desligada por padrão. A seção 13 registra as verificações executadas. Os valores iniciais passaram pela validação dos envelopes e pelo provider controlado; custo e fidelidade com um modelo real não foram medidos.

## 1. Objetivo e limites

Processar todo o corpo das observações armazenadas de uma sessão em chamadas limitadas ao LLM, combinar os resultados e publicar pelo caminho atual de consolidação ou aprovação de propostas. Cobrir consolidação de uma página e de várias páginas e a revisão de sessão do auto-improve.

A garantia é de cobertura da entrada armazenada: cada intervalo de texto do snapshot deve participar de uma chamada concluída com resposta estruturada válida. Isso não garante que o resultado do modelo preserve todos os fatos. A combinação ainda é uma síntese e precisa ser avaliada quanto a omissões, contradições e invenções.

Os limites anteriores à leitura das observações continuam existindo. Conteúdo excluído pela política de captura, sanitizado ou truncado antes da persistência não pode ser recuperado pelo chunker. Este trabalho não aumenta limites de captura, não lê arquivos do checkout para completar observações e não altera a privacidade da ingestão.

Também ficam fora deste trabalho: trocar o modelo automaticamente, alterar o runner do Ollama, criar um provider por tarefa, refazer bootstrap, dream, experience ou a classificação de perfil para usar chunks. Quando necessário, essas tarefas participam apenas da admissão compartilhada das chamadas do servidor.

Não há PRD ou ADR específico aprovado para esta iniciativa. Este documento registra o desenho implementado e seus limites; não substitui os contratos de segurança existentes.

## 2. Diagnóstico do código anterior

Os pontos abaixo motivaram o desenho. Com a flag desligada, esses caminhos seguem como o fluxo legado.

- `crates/ai-memory-consolidate/src/consolidator.rs`: `consolidate_session` e `consolidate_session_multi` fazem uma chamada lógica por execução, sujeita a retries. A projeção limita a seleção a 256 observações e o corpo a 3.000 caracteres por observação.
- `crates/ai-memory-consolidate/src/auto_improve.rs`: `run_auto_improve_review` seleciona observações, limita corpos e chama validação e eval antes de entregar um relatório para staging. `input_budget_sampled` informa seleção parcial.
- `crates/ai-memory-consolidate/src/projection.rs`: a seleção por pontuação, o descarte por orçamento e o truncamento final são etapas distintas. Aumentar apenas o orçamento não elimina os outros cortes.
- `crates/ai-memory-store/src/reader.rs`: `SessionObservationSnapshot` mantém uma transação durante uma closure síncrona. Essa transação não deve ficar aberta enquanto se aguarda o LLM.
- `crates/ai-memory-store/src/session_consolidation.rs`: a geração atual usa contagem de observações. Uma contagem não identifica o conteúdo de um snapshot nem detecta todas as combinações de remoção e inserção.
- `crates/ai-memory-consolidate/src/bootstrap.rs` e `V60__bootstrap_chunk_progress.sql`: há precedente para processamento sequencial e checkpoints. O armazenamento é específico de bootstrap e não tem os vínculos necessários com sessão e escopo.
- `crates/ai-memory-cli/src/commands/serve.rs`: workers distintos compartilham o provider. O worker de consolidação existente drena trabalhos pendentes mesmo sem novos enqueues de SessionEnd.

Nos logs examinados durante o diagnóstico, uma consolidação com 89.816 tokens ainda processava a entrada perto do timeout de 300 segundos. Um auto-improve começou a processar seu prompt perto do fim de seu próprio prazo, depois de esperar pelo runner. Chunking precisa controlar tanto o tamanho das chamadas quanto sua admissão.

Referências de projeto: [arquitetura](ARCHITECTURE.md), [auto-improve](auto-improvement-loop.md), [limites de configuração](config-properties.md), [segurança](security-boundaries.md), [operações de ciclo de vida](lifecycle-ops.md), [resíduo literal](design-verbatim-residue.md).

## 3. Ativação e compatibilidade

Configuração pública, em `ChunkingSettings` (`ai-memory-core`):

```toml
[chunking]
enabled = false
max_input_tokens = 8192
max_calls_per_run = 128
```

- `enabled` é a chave de ativação para o processamento chunked de consolidação e auto-improve. Não há ativação implícita por tamanho de sessão, timeout ou erro do provider.
- `max_input_tokens` é o orçamento aproximado de uma chamada do modo chunked, incluindo instruções, esquema e contexto. O payload efetivo respeita também o limite da operação. O valor de 8.192 tokens vale para os prompts intermediários compactos; o carregamento rejeita valores abaixo de 2.048 com a flag ligada.
- `max_calls_per_run` limita o total de tentativas de chamadas ao provider, inclusive map, reduce, final e retries, em uma execução lógica. O contador é persistido no trabalho; reiniciar ou tentar novamente não o zera. O valor 128 ainda precisa de validação de custo e escala. Atingir o limite produz o estado `budget_exceeded`, nunca sucesso com cobertura parcial. O carregamento rejeita `0` com a flag ligada. Uma nova solicitação equivalente com `max_calls` maior reenfileira o trabalho `budget_exceeded`; o limite não entra na chave de equivalência.
- Com `enabled = false`, as outras duas chaves não são validadas, então um valor antigo não quebra uma instalação que nunca optou pelo modo.

Ao habilitar, o servidor valida o caminho completo (map, reduce e o envelope final de cada operação) contra o orçamento e falha na inicialização com a chave que precisa mudar. O envelope final usa um teto de saída próprio de 8.192 tokens; não herda os 32 mil da consolidação legada nem o piso de 8.350 do builder legado.

Com a flag ausente ou `false`:

1. Os caminhos atuais continuam como eram, incluindo seleção/truncamento, prompts, retornos, dry-run, tratamento de erros e cadência.
2. Nenhum trabalho novo é criado, o worker não inicia e a admissão compartilhada não é instalada no caminho do provider.
3. As operações assíncronas novas não são anunciadas nem aceitas. Chamadas diretas de início falham de forma explícita (404 no admin, erro de ferramenta no MCP, estado desativado na CLI); não há fallback silencioso para o fluxo síncrono.
4. Os defaults de `[consolidation]` e `[auto_improve]` e a interface atual dos seus clientes permanecem.

Com a flag ligada, `memory_consolidate`, `memory_auto_improve`, o enqueue de SessionEnd e o scheduler de auto-improve rodam pelo mesmo trabalho durável, mantendo o formato de resposta atual. A chamada síncrona dirige o trabalho na própria tarefa e, se outro worker detém o lease, consulta o estado a cada segundo. Dry-runs continuam no caminho legado, que não chama o modelo. Os limites de `[consolidation]` e `[auto_improve]` valem como tetos por chamada e não descartam o excedente da sessão. O orçamento próprio evita que uma instalação com `consolidation.max_input_tokens = 100000` monte chunks desse tamanho.

Ao reiniciar com a flag desligada, o servidor pausa todo trabalho ativo (`queued`, `running`, `reducing`, `publishing`), guardando a fase anterior em `paused_from`. Nada é publicado durante a pausa. Se a publicação final já havia começado, os efeitos e IDs confirmados ficam preservados e a retomada trata só a cauda pendente, sem rollback atômico. Os trabalhos nunca são convertidos para o fluxo legado. Com a flag ligada de novo, o worker retoma os pausados, desde que os fingerprints continuem compatíveis. A consulta de estado continua disponível para qualquer trabalho existente.

Enquanto houver um trabalho equivalente não terminado (`queued`, `running`, `reducing`, `publishing`, `paused` ou `budget_exceeded`) para a mesma sessão, operação e bucket de dono, o caminho legado se recusa a rodar. A chamada síncrona retorna erro explícito (409 no admin), o enqueue de SessionEnd adia a geração com a mesma mensagem e nenhum ID de trabalho substitui o resultado. Sem trabalho equivalente, o caminho legado segue como antes. A limpeza de retenção remove trabalhos parados por mais de 7 dias, e só então o caminho legado volta a valer para aquela sessão.

As chaves pertencem ao caminho único de `Config::load()`; alterações exigem reiniciar o servidor, sem hot reload. TOML, env, defaults e validações estão no template e em `docs/config-properties.md`. O exemplo não autoriza mudar a configuração local do operador.

## 4. Manifesto e planejamento dos chunks

### 4.1 Snapshot durável

Uma leitura curta e escopada identifica as observações do trabalho. O manifesto (`processing_job_observations`) guarda ordem, ID, tamanho e SHA-256 do corpo de cada observação, sem copiar o corpo. A identidade do trabalho (`equivalence_key`) cobre escopo, sessão, operação, bucket de dono, opções e os fingerprints de contexto e de manifesto. Não se usa `COUNT(*)`, timestamp nem máximo de rowid.

Nenhuma transação de leitura fica aberta durante chamadas de rede. Os corpos são relidos por `read_processing_sources`, que confere identidade, escopo e hash contra o manifesto. A leitura passa pelos guards de escopo existentes.

Observações que chegam depois do snapshot pertencem a outra geração de trabalho. Triggers de V97 em `observations` invalidam, na mesma transação, qualquer trabalho ativo cuja observação mudou de corpo, título, tipo, horário ou escopo, ou foi removida, descartando seus nós e manifesto. Purge, move e merge de sessão ou projeto também invalidam o trabalho no escopo antigo dentro da transação da operação; trabalhos terminados seguem a sessão com seus resultados. A poda de observações brutas exclui sessões com trabalho não terminado. Purge explícito prevalece e remove os derivados.

### 4.2 Cobertura determinística

O planejador percorre todas as observações do manifesto. Não chama o seletor top-k nem limita a quantidade a 256, 48 ou 72 observações.

Observações inteiras entram quando cabem. Uma observação maior que o espaço disponível vira fragmentos (`Span`) com ordinal de origem e offsets UTF-8 válidos, cortados em fronteira de parágrafo, linha ou espaço e, na falta delas, em fronteira de caractere. Nenhum resto de corpo é omitido. Não há sobreposição: cada byte pertence a exatamente um intervalo primário.

O ledger de cobertura é mantido pelo servidor a partir dos nós de map concluídos. A conclusão exige a união completa dos intervalos primários do manifesto, sem lacunas. O modelo não decide nem certifica a própria cobertura.

### 4.3 Orçamento

Instruções, esquema JSON, delimitadores, contexto de página e evidências repetidas entram no orçamento. A saída é reservada à parte. Map, reduce e final têm prompts e schemas próprios e compactos, cobrados do `max_input_tokens` antes de admitir qualquer evidência, e tetos de saída limitados também pela operação.

A estimativa é conservadora: 3 caracteres por token, com a margem de segurança de `[consolidation]`, e cada caractere não ASCII custa 3. Ela não é o tokenizer de nenhum provider. Se nem uma entrada mínima com seu envelope couber, a validação rejeita a configuração; nunca se envia uma chamada sem evidências.

Um erro de contexto excedido, resposta truncada ou saída inválida não autoriza descartar texto nem repetir o mesmo payload. O nó fica `recoverable` e é subdividido (map) ou reagrupado (reduce). Na chamada final, até 3 reduções do espaço disponível; depois disso o trabalho falha.

## 5. Extração e combinação

### 5.1 Map

Cada chunk produz um digest JSON compacto: itens (`fact`, `decision`, `gotcha`, `rule`, `procedure`, `open`, `correction`) com referências às observações e fragmentos, citações curtas, supersessões e conflitos. O digest não é uma proposta nem uma página.

O modelo trata observações, nomes de arquivos e texto de ferramentas como dados não confiáveis. O servidor valida referências contra o manifesto, confere as citações contra a evidência mostrada e sanitiza o digest antes de gravar o nó. O digest comporta no máximo 12 itens e 4 conflitos; respostas acima desses tetos são rejeitadas para subdivisão. Itens sem referência válida são descartados e contabilizados, e citações sem suporte são removidas. Não se persiste a resposta bruta do provider nem cópias ilimitadas dos prompts.

Um resultado truncado pelo limite de saída não conta como chunk concluído. O nó vira `recoverable` e é subdividido, sem validar como vazio e sem repetir a chamada idêntica.

### 5.2 Reduce

Os digests são combinados em grupos que respeitam o mesmo orçamento, em quantos níveis forem necessários. Nenhum prompt concatena todos os intermediários. Referências de origem e temporalidade são preservadas, e uma correção tardia pode substituir uma conclusão antiga. Conflitos sem solução permanecem explícitos. Os testes avaliam essa propriedade sem prometer que uma instrução no prompt garante fidelidade.

Se um redutor não consegue reduzir o bastante para haver progresso, a máquina de estados para com erro em vez de recursão ilimitada, preservando nós e cobertura.

### 5.3 Publicação

Na consolidação, só a chamada final produz os contratos existentes de uma ou várias páginas. A publicação passa por `Wiki::write_page` ou `Wiki::apply_batch` com atribuição, sanitização, admissão, supersessão, indexação e recuperação existentes. A versão-base de cada página é capturada com o contexto que o modelo viu; um caminho fora desse contexto só pode ser criado, nunca substituído. O contexto é recalculado depois da chamada, e uma mudança o descarta e repete a etapa, no máximo 3 vezes.

No auto-improve, os candidatos de todos os chunks são reunidos antes da chamada final, que produz o relatório completo uma única vez. Validação, limites (`max_proposals_per_run`, orçamento de edits) e eval rodam sobre o conjunto final, uma vez por proposta avaliada. Mudanças concorrentes continuam gerando conflito, sem last-write-wins. Um chunk não cria linha de `auto_improve_runs`.

Patches compatíveis para o mesmo destino são combinados antes dos limites globais e do eval; candidatos incompatíveis ficam registrados como rejeitados. O eval usa os corpos-base capturados com o contexto. Antes de executar um comando externo, o servidor grava um marcador; depois, grava o relatório validado e os resultados. A retomada reutiliza esse resultado. Se houver apenas o marcador, o trabalho falha explicitamente: o comando pode ter sido executado e não é repetido automaticamente.

O plano de publicação (páginas, ou propostas com seus resultados de eval) e sua identidade são gravados antes do primeiro efeito. As páginas recebem um carimbo `processing_job` no frontmatter e o staging o grava em `config_json.processing_job`. Depois de uma queda entre o commit do efeito e o checkpoint, a retomada encontra o efeito por essa identidade e o adota, sem escrever de novo; os IDs confirmados ficam em `processing_job_effects`. A publicação de propostas diferentes não passa a ser atômica por existir chunking. A política atual de aprovação continua válida, inclusive `require_approval=true`. O consumidor `experience` não foi alterado.

## 6. Estado, posse e ciclo de vida

Estados: `queued`, `running`, `reducing`, `publishing`, `completed`, `paused`, `budget_exceeded`, `failed` e `invalidated`. São terminais `completed`, `failed` e `invalidated`. O resultado final diferencia propostas staged, aplicadas, pendentes e conflitantes pelos estados já existentes de proposta.

A migração V97 adiciona quatro tabelas privadas: `processing_jobs` (job, escopo, dono, estado, lease, fence, contadores, identidade e plano de publicação), `processing_job_observations` (manifesto), `processing_job_nodes` (checkpoints sanitizados de map e reduce, até 50.000 por trabalho) e `processing_job_effects` (efeitos confirmados). Não se reutiliza `auto_improve_runs` como progresso nem a tabela de bootstrap.

O trabalho carrega o fingerprint de contexto: versão do pipeline (`chunk-v1`), identidade configurada dos backends e da ordem dos fallbacks, hashes dos prompts e schemas de map, reduce e final, orçamentos derivados, limites de chunking e de consolidação, `per_user_slots` e `require_approval`. As opções (instruções e, no auto-improve, a configuração de revisão com eval) e o bucket de identidade entram na chave de equivalência. Se o fingerprint calculado na retomada difere do gravado, o trabalho é invalidado e não é retomado. Os nós de map e reduce não dependem de páginas auxiliares; o contexto de páginas é lido na chamada final e revalidado como descrito em 5.3. Uma mudança remota no modelo por trás do mesmo alias não é detectável.

Cada claim tem lease de 120 segundos, renovado periodicamente, e um fence token. Uma tentativa é reservada atomicamente antes da chamada lógica e não é devolvida se o envio ficar incerto. Um guard também reserva cada envio adicional feito dentro do provider, como o fallback de JSON estrito para tolerante, outra opção da cadeia ou o reenvio após 401 do Codex. Há até 3 tentativas da chamada lógica; todas consomem o orçamento durável. Falhas transitórias reagendam o trabalho até o limite de 5 repetições de erro. O eval externo segue seus próprios limites e não conta como chamada LLM.

Escritas de página, evidência, proposta e staging feitas por um handle com fence passam por triggers TEMP locais à conexão do writer, que abortam a escrita de quem perdeu a posse, mesmo com uma resposta ainda em memória. As triggers não fazem parte do schema persistido, para que outro Store ou processo não possa limpar a posse armada de um writer. Manual e scheduler encontram o mesmo trabalho compatível em vez de duplicar chamadas.

O worker remove trabalhos terminados ou parados por mais de 7 dias a cada hora, em cascata sobre manifestos, nós e efeitos, preservando quem tem lease ativo. O estado é derivado e privado, não uma segunda wiki: um reindex pode descartá-lo sem afetar páginas publicadas. Backup, restore e reconstrução do índice não foram revisados neste documento.

## 7. Admissão das chamadas

Com a flag ligada, o provider de chat é embrulhado em `AdmittedLlmProvider` e compartilhado por todos os workers do servidor. Uma fila FIFO justa admite uma chamada por vez, e no máximo 32 chamadas esperam. Cada chamada do job chega à fila separadamente, então um trabalho longo reentra no fim da fila depois de cada chamada e não bloqueia uma chamada curta que chegou durante a execução. Reduções, finais e retries passam pela mesma fila.

A espera tem limite próprio de 300 segundos e termina em `CapacityBusy`, um sinal de saturação local que não conta como falha do provider. Depois de admitida, a chamada roda apenas sob o deadline do provider. Cancelar a chamada que espera libera o lugar na fila. O status acumula separadamente `wait_ms` e `exec_ms` das chamadas que retornaram; uma chamada perdida em queda ou cancelamento não tem duração registrada. Sem um wrapper de admissão, o tempo medido entra em execução.

As chamadas de rerank nunca esperam: se não há admissão imediata, falham rápido e a busca mantém a ordem anterior. Nenhuma latência de 20 segundos é prometida quando uma chamada longa já está em execução, e a fila local não preempta a GPU.

O controle só coordena clientes do provider dentro deste servidor. Um `llm-test` direto, outra aplicação ou outro processo Ollama continua fora dele. Embeddings não entram nessa fila de chat.

## 8. Interfaces assíncronas

- Admin: `POST /admin/processing-jobs` inicia ou junta o trabalho compatível e responde 202 com o status; `GET /admin/processing-jobs/{job_id}?workspace=&project=` devolve o status. Ambas ficam sob o gate admin root-only e a resolução de escopo sem criação.
- MCP: `memory_processing_job_start` (exige escrita no projeto) e `memory_processing_job_status`.
- CLI: `ai-memory processing-jobs start --session-id <id> --kind <kind>` e `ai-memory processing-jobs status --job-id <id>`, com `--workspace`, `--project` e `--json`.
- `kind` aceita `consolidate`, `consolidate_multi` e `auto_improve`.

O início retorna na hora o ID, o estado e o progresso. Repetir o início identifica o trabalho compatível sem repetir a execução. O status traz tipo, estado, `chunks_total`, `chunks_done`, `calls_used`, `max_calls`, `wait_ms` e `exec_ms`, motivo redigido de pausa, falha ou invalidação, resultado final e carimbos de criação, atualização e conclusão. `wait_ms` e `exec_ms` somam, em milissegundos, a espera na fila de admissão do provedor e a execução do provedor das chamadas que retornaram (sucesso ou falha), gravadas sob a claim antes do checkpoint de resultado ou erro e sem relação com a reserva de `calls_used`. Uma chamada em voo quando o processo cai não é medida, então os totais podem ficar abaixo do tempo real. Um provedor sem fila de admissão conta todo o tempo como execução. Prompts, respostas brutas, checkpoints, buckets de dono e tokens de claim nunca saem.

Os contratos síncronos atuais não trocam relatório final por ID de trabalho. Com a flag ligada, uma entrada síncrona compatível espera o mesmo trabalho durável, e seu cliente ainda sofre o timeout da conexão; trabalhos longos devem usar a interface nova.

Descoberta MCP: 25 ferramentas com a flag desligada e sem trabalhos; 27 com a flag ligada; 26 com a flag desligada e trabalhos retidos, listando só `memory_processing_job_status`. A ferramenta de início nunca aparece com a flag desligada e é recusada também no dispatch direto. A CLI mostra o estado desativado e não finge sucesso. Um ID de trabalho de outro projeto ou dono é tratado como não encontrado, sem distinguir de um ID inexistente. Nenhuma credencial DB-user acessa `/admin/*`.

O SDK TypeScript expõe `jobs.start`, `jobs.status` e `jobs.discover`, com tipos, classificação de cobertura, snapshots por variante (padrão, com chunking, com trabalhos retidos) e docs. Os mesmos snippets de roteamento e instruções do servidor descrevem as duas ferramentas.

## 9. Alternativas consideradas

### Resumo acumulado entre chunks

Exige menos etapas, mas cada chamada depende do resumo anterior. Uma omissão inicial se propaga e a retomada fica presa à cadeia. Não foi a escolha, por causa da garantia de cobertura e da auditoria por origem.

### Map/reduce com checkpoints

Escolha adotada. Permite planejar cobertura, retomar nós concluídos e distinguir leitura das fontes de publicação. Exige armazenamento temporário e validação de perdas e conflitos entre níveis.

### Gravação final por chunk

Rejeitada. Expõe conclusões parciais, pode repetir destinos e aplicar limites/eval várias vezes. Também dificulta recuperar um trabalho parcialmente publicado.

### Somente elevar timeout ou paralelismo do Ollama

Pode aliviar a instalação atual, mas não implementa cobertura por chunks, retomada ou controle de publicação. Não é requisito de instalação da funcionalidade.

## 10. Fases e critérios de aceite

Os critérios abaixo orientaram a implementação. A seção 13 separa a evidência determinística e de runtime dos limites de validação descritos na seção 12.

### Fase 1: contratos e flag

Tipos internos, configuração, erros, estados e contratos de início e progresso. Compatibilidade off/on, política de orçamento e estado após desligar a flag.

**Aceite:** configuração ausente e `false` mantêm os mesmos resultados, chamadas e efeitos do fluxo atual; `true` só é disponibilizado na entrega completa. Orçamentos inválidos falham no carregamento com mensagem útil. Template e referência de config acompanham as chaves.

### Fase 2: manifesto, planejador e checkpoints

Leitura escopada em lotes, cobertura de corpos, divisão de observações grandes, persistência, posse, retomada, invalidação e limpeza, sem criar um framework geral de workflow ou novo crate.

**Aceite:** reconstruir os intervalos cobertos recupera exatamente os corpos armazenados do snapshot; nenhum caractere UTF-8 desaparece, mesmo com observações maiores que um chunk e eventos com timestamps iguais. Leitura com projeto estrangeiro é recusada, com controle legítimo passando. Dois workers não publicam o mesmo trabalho. Purge/move durante uma chamada impede publicação posterior. Reinício não refaz nós concluídos e não zera orçamento consumido, inclusive quando ocorre depois do envio e antes do checkpoint. Alterações de páginas auxiliares ou políticas invalidam os nós dependentes.

### Fase 3: consolidação

Map/reduce e adaptador para as saídas single-page e multi-page. Integração com SessionEnd e `memory_consolidate`, preservando o modo zero-LLM e os dry-runs. Amostragem e truncamento ficam fora do novo caminho e permanecem no legado.

**Aceite:** fixtures com informações relevantes no começo, meio e fim passam por todos os chunks; uma correção tardia não vira uma conclusão antiga. Falha intermediária não publica resumo parcial. Redução excessivamente grande, saída truncada ou ausência de progresso não entram em loop nem são tratados como sucesso. O caminho de publicação mantém rollback, supersessão e índice coerentes. Queda entre commit da escrita e checkpoint não publica a mesma geração novamente.

### Fase 4: auto-improve

Os mesmos contratos de manifesto e nós, com extração própria de candidatos, combinação e staging final. Cobre chamadas manuais CLI/admin/MCP e scheduler. O consumidor experience não ganha chunking nesta entrega.

**Aceite:** dois chunks com candidatos para a mesma página não causam propostas duplicadas nem multiplicam os limites por execução. Eval usa a versão-base correta e roda uma vez por proposta final avaliada, não por chunk. Reinício entre staging e aprovação reusa os IDs e respeita os estados atuais e a aprovação manual, inclusive se a queda ocorrer entre o commit do staging e seu checkpoint. Manual e scheduler concorrentes não duplicam o trabalho compatível.

### Fase 5: admissão e acompanhamento

Admissão compartilhada, worker durável e superfícies assíncronas em CLI, MCP, admin e SDK. Nenhuma transação SQLite nem permissão de execução de um chunk fica retida durante uma espera longa por rede.

**Aceite:** um trabalho longo não coloca todos os seus chunks à frente de uma chamada curta; fila saturada é limitada e observável. Cancelamento por timeout remove a espera correspondente. Um token ou escopo estrangeiro não lê progresso nem resultado; admin continua root-only. Flag desligada não admite trabalho novo, e trabalhos já persistidos ficam em estado recuperável.

### Fase 6: prova de runtime e documentação

Fixtures determinísticas e um provider de teste controlado para falha, fila e retomada; depois o caminho real do servidor com provider local, histórico grande e chamada curta concorrente. Medir tamanho de cada payload, cobertura, chamadas totais, tokens quando informados, duração, espera e qualidade do resultado, comparando o fluxo legado e o novo no mesmo conjunto de sessões.

**Aceite:** demonstrar retomada após interromper o servidor, ausência de publicação de intermediários e progresso da chamada curta entre partes. Uma publicação final interrompida preserva efeitos confirmados e não os duplica ao retomar. Não declarar latência fixa ou fidelidade com base apenas no tamanho do chunk. Publicar os limites e o custo observados e avaliar os valores de 8.192 tokens e 128 chamadas antes de fixá-los.

## 11. Superfícies da entrega

- `ai-memory-core`: `ChunkingSettings`, tipos e IDs de trabalho (`processing_job.rs`).
- `ai-memory-consolidate`: `ProcessingService` e o módulo `processing/` (planejador, digest, máquina de estados map/reduce, orçamentos, etapa final, publicação), com prompts JSON próprios.
- `ai-memory-store`: migração V97, `processing_jobs.rs` (leases, reserva de chamadas, idempotência, limpeza), integração com purge, move, merge e poda de observações.
- `ai-memory-llm`: `AdmittedLlmProvider`, instalado em `serve.rs` somente com a flag ligada.
- `ai-memory-cli` e `ai-memory-mcp`: configuração, dispatch off/on, worker, rotas admin, `processing-jobs` na CLI e as ferramentas MCP.
- `companions/ai-memory-ts`: `jobs.start`, `jobs.status`, `jobs.discover`, tipos, cobertura e snapshots.
- Docs: `ARCHITECTURE.md`, `auto-improvement-loop.md`, `config-properties.md`, `security-boundaries.md`, `design-client-sdk.md` e a documentação do SDK. Entrada de CHANGELOG segue a regra do repositório, sem referência de ticket inventada.

A feature é aditiva e mira a branch de próxima versão de funcionalidades, sem bump ou tag automático. Alterações de runtime ficam separadas da instalação local usada no diagnóstico.

## 12. Riscos e verificação pendente

O modo chunked aumenta chamadas e custo total. Ler tudo não garante que o modelo retenha tudo. Contexto repetido e níveis de redução podem aumentar o custo e introduzir erros de síntese. O limite de 12 itens por digest também comprime densidade de fatos em chunks ricos, e os testes de fidelidade precisam expor essa perda.

Chunks menores reduzem a ocupação por chamada, mas não tornam a GPU preemptível e não coordenam outros processos. Saída de muitos milhares de tokens também ocupa o modelo; limitar só a entrada não basta.

Ainda não foram medidos custo, latência ou fidelidade com um modelo real, nem comparados modelos para escolher os valores de 8.192 tokens e 128 chamadas. Os testes com provider controlado verificam limites, cobertura e recuperação, não qualidade de síntese ou desempenho do Ollama. Backup e restore de um trabalho em andamento não tiveram smoke dedicado; o estado privado continua no mesmo SQLite coberto pelas operações existentes.

## 13. Verificação observada

Verificação local em macOS arm64, sem alterar a configuração instalada, reiniciar o servidor do operador ou chamar seu Ollama.

- `cargo clippy --workspace --all-targets -- -D warnings` e `cargo fmt --all -- --check` passaram.
- Os testes focados passaram: 451 no crate LLM, 51 no processamento, 23 de persistência de jobs e 9 de sidecars. Com o engine desligado, o scheduler ignorou a sessão com job pausado e revisou a sessão de controle.
- O SDK passou por `npm run check` e 58 testes de integração com o binário real. Os snapshots vieram de três servidores: 25 ferramentas no padrão, 27 com chunking e 26 após desligá-lo com jobs retidos.
- O smoke do servidor passou nas 115 verificações, com diretórios temporários e um provider HTTP controlado: flag desligada, cobertura UTF-8, map/reduce, duas quedas por SIGKILL, retomada, pausa e reativação, autorização, chamada curta concorrente, consolidação multi-page e staging de uma proposta não vazia com aprovação manual.
- Na sessão de cobertura, foram armazenadas 302 observações. Os corpos não vazios do manifesto foram reconstruídos byte a byte a partir das entradas de map; um corpo de 15.065 bytes atravessou duas chamadas. Foram 13 chamadas de map, uma de reduce e uma final. O maior payload, incluindo schema, ficou em 6.193 tokens pela estimativa do próprio orçamento, abaixo de 8.192. Esses números não medem tokens nem latência de um modelo real.
- As duas quedas não repetiram nós já confirmados nem geraram duas publicações. A chamada curta entrou antes do próximo chunk do trabalho longo; o provider recebeu uma requisição por vez.
- As mutações dos guards de dono e de confirmação pós-escrita do sidecar fizeram os testes adversariais falharem; restaurados os guards, passaram. A instalação parcial dos guards também foi reproduzida e passou a encerrar o writer antes de aceitar comandos.

A execução global com `--no-fail-fast` exercitou todos os binários. Restaram duas checagens de drift: o bloco gerenciado de `AGENTS.md` e um pin literal da versão do schema. O bloco foi regenerado; o pin foi removido, preservando a verificação de ausência das colunas antigas de tokens. A revalidação passou nos 359 testes do core, nos 15 de credenciais, nos doctests do workspace e no clippy. As demais suítes passaram na execução global. As fixtures foram isoladas removendo `AI_MEMORY_DATA_DIR` herdado apenas do processo de teste e usando o executável real do Node no PATH.
