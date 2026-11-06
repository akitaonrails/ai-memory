# Captura de ciclo de vida externo

Um orquestrador que observa seus agentes pode enviar eventos de ciclo de vida para um servidor
ai-memory compartilhado por meio de `/hook/batch`. Adaptadores de frameworks pertencem ao ambiente externo ao core;
o servidor continua responsável por armazenamento, recuperação, consolidação e memória durável.

Para escritas, consultas e handoffs deliberados sem captura de ciclo de vida, comece
com o [guia de memória programática](programmatic-memory.md).

## Escolha um caminho de captura antes de iniciar

Defina `AI_MEMORY_CAPTURE_OWNER` com o namespace do seu produtor no ambiente do
harness que você iniciar, por exemplo `example.runtime`. Qualquer valor que não seja espaço em branco
habilita esse processo e seus filhos herdados na captura externa. Valores não definidos,
vazios e contendo apenas espaços em branco mantêm a captura nativa normal.

```text
Orchestrator
  |-- launches harness with AI_MEMORY_CAPTURE_OWNER=example.runtime
  |-- records its lifecycle events with stable IDs
  `-- sends those events through POST /hook/batch

Harness
  |-- installed hooks keep delivering supported startup context
  `-- MCP keeps providing recall and deliberate memory writes
```

Comandos nativos de hook atualizados, pacotes de hook POSIX/PowerShell e integrações geradas
do OpenCode/OpenCode 2/OMP/Pi/OpenClaw respeitam a variável. Aplique novamente o
`install-hooks` após a atualização para renovar scripts preparados ou código gerado.
Instalações mais antigas não compreendem este contexto.

| Responsabilidade | Com captura externa |
|---|---|
| Observações de ciclo de vida nativas | Ignoradas antes de spool, fila ou POST |
| Drains de hook da CLI nativa e backfill de SessionStart | Ignorados |
| Entrega aceita de handoff, briefing e avisos de caixa de entrada | Preservada |
| Identidade nativa usada por MCP com reconhecimento de sessão/handoff | Preservada |
| Recuperação MCP e escritas deliberadas | Inalteradas |
| Política de captura do repositório | O produtor verifica e aplica antes de enfileirar cada evento |

Claude Code recebe contexto via SessionStart. Kimi Code o recebe via
UserPromptSubmit porque descarta a saída do SessionStart. Os mesmos caminhos de entrega
existentes permanecem em uso. A entrega de contexto pode cadastrar a sessão receptora
e reivindicar um handoff; suprimir a captura não a torna somente leitura. Harnesses
sem injeção automática ainda podem usar `memory_handoff_accept` via MCP.
Não reivindique também esse handoff no orquestrador se o hook for o responsável pela entrega.

Este é um contexto de processo herdado, não uma configuração de servidor, identidade de segurança
ou lease. Mantenha-o fora do ambiente do servidor compartilhado e da configuração global
do shell. Inicializações avulsas sem ele continuam capturando normalmente.
Um runtime hospedando múltiplos agentes em um único processo aplica a variável a todas as suas
integrações instaladas do ai-memory; ele próprio deve gerenciar a posse de captura mais granular.

`ai-memory hook --check-capture` relata `external_capture: true` e
`admits_capture: false` quando esse contexto está ativo. Ele não realiza ingestão nem
entrega de handoff. O namespace do próprio produtor não é impresso nem autenticado.

Use `policy_admits_capture` para a verificação prévia (preflight) do produtor externo: ela ignora
a posse de captura, mas verifica a política do repositório, elegibilidade do evento, exclusões,
completude do escopo e resolução do perfil do servidor. Escopo parcial e um perfil
rejeitado retornam `false`. Aplique `disposition`, incluindo remoção apenas de metadados,
antes de enfileirar. `scope` contém dicas de roteamento locais sanitizadas de no máximo 512
bytes cada; dicas com tamanho excessivo são omitidas e a verificação prévia falha de forma fechada.
`scope_resolution` distingue escopo explícito, parcial, derivado do servidor e
indisponível. `server_may_remap` avisa que o servidor pode escolher coordenadas
diferentes. Valores nativos de roteamento não são afetados pelo limite de inspeção.

`ai-memory doctor` relata se esta invocação delega a captura nativa e
mostra sua identidade de máquina autenticada. Suas contagens por agente sinalizam sessões
com múltiplas fontes de captura (eventos nativos ou extensões distintas, incluindo
backfill). Um servidor mais antigo deixa esses
campos como desconhecidos; uma contagem mista é um sinal de diagnóstico, não prova de duplicatas.

## Reutilize o contrato de eventos público

Para um harness em execução, envie seu `agent` real na transmissão (como `claude-code` ou `codex`)
e o `session_id` nativo. Use o mesmo ID de sessão, cwd, escopo e operador autenticado
que a sua integração nativa restante. O ID de sessão do corpo tem
precedência sobre um parâmetro de consulta. Substituir apenas o ID de ingestão pelo ID lógico
de agente de um orquestrador o separa da identidade nativa de handoff/MCP.

Use `workspace` e `project` explícitos para cada evento. Preserve os nomes
canônicos de ciclo de vida (`session-start`, `user-prompt-submit`, `post-tool-use`,
`session-end`, etc.) e o formato de payload aceito pelo harness. `session-end`
significa que a execução terminou, não que um turno foi concluído. Sessões contendo apenas
eventos desconhecidos não são sessões substantivas para resumos automáticos.

Defina `extension=<producer-namespace>` e um `source_event=<event-name>` explícito
para proveniência. Ambos são necessários para preservar o namespace em eventos reconhecidos.
`extension` aceita até 64 caracteres de token ASCII; `source_event` aceita 128.
Letras, dígitos, `.`, `_`, `-` e `:` são permitidos. Esses campos descrevem a
origem; eles não expandem o enum de eventos principal nem retêm metadados arbitrários.

Mantenha agentes lógicos, relacionamentos de pai/filho e IDs de fluxo de trabalho no estado
do lado do adaptador por enquanto. Não reutilize campos nativos `agent_id`: alguns payloads de
harness os utilizam como marcadores de subagentes. O campo existente de workstream gerenciado
`managed_run` também tem seu próprio comportamento de lease, registro (ledger) e handoff.

### Identidade de nova tentativa estável

`ingest_key` é uma chave existente com escopo de projeto, independente de `extension`.
Construa-a uma única vez a partir da identidade estável do evento e persista-a com o evento antes
da entrega. Nunca faça hash de conteúdo de prompt/ferramenta para decidir se os eventos são iguais.

Por exemplo, um adaptador usando Node pode derivar uma chave de 64 caracteres da seguinte forma:

```javascript
import { createHash } from "node:crypto";

function ingestKey({ producer, actor, agent, sessionId, sourceEvent, eventId }) {
  const identity = JSON.stringify([
    "external-capture-v1", producer, actor, agent,
    sessionId, sourceEvent, eventId,
  ]);
  return createHash("sha256").update(identity, "utf8").digest("hex");
}
```

Aqui `actor` é um namespace de operador estável do lado do adaptador, nunca um bearer token.
Inclua as coordenadas dentro das quais `eventId` é exclusivo. Um adaptador com
IDs de eventos exclusivos em todo o produtor pode usar uma tupla menor. Mantenha sua codificação e
seus valores estáveis entre reinicializações de processo e linguagens de implementação.

A mesma identidade de produtor/evento produz a mesma chave na nova tentativa. IDs de eventos
distintos ou namespaces de produtores distintos geram chaves distintas mesmo para payloads iguais. Reutilizar
o ID de evento bruto como `ingest_key` sob duas extensões **não** o isola em namespace.
Chaves aceitam de 1 a 64 letras ASCII, dígitos, `_` ou `-`. Chaves malformadas recorrem
à ingestão sem chave, portanto valide-as antes de enviar.

O servidor reivindica a chave e a observação em uma única transação. Uma repetição concluída
é ignorada; uma repetição pendente retoma o trabalho downstream sem inserir outra
observação. Chaves normais de clientes expiram após **30 dias**. O namespace reservado
e validado pelo servidor `recovery_<55 hex>` é usado apenas pela recuperação/backfill de
transcrição nativa e permanece vinculado à sua sessão real armazenada até que essa
sessão ou projeto seja excluído. Esta é uma proteção limitada contra repetições, não uma
desduplicação permanente ou garantia de exatamente-uma-vez para cada efeito colateral na wiki.

### Lotes ordenados e recuperação

Um lote usa o array existente de itens `{url, body}`:

```json
[
  {
    "url": "/hook?event=user-prompt-submit&agent=claude-code&workspace=team&project=app&extension=example.runtime&source_event=user-prompt-submit&ingest_key=example-runtime-run-17-prompt-4",
    "body": {
      "session_id": "native-session-17",
      "cwd": "/work/app",
      "prompt": "Continue the parser fix."
    }
  }
]
```

Envie este JSON para `POST /hook/batch` com a autenticação bearer normal. A
URL em cada item fornece os parâmetros de consulta do evento; o servidor não busca essa URL.
A chave curta acima assume que o runtime atribuiu um ID de execução exclusivo para todo o produtor.
Use a receita com a tupla quando os IDs de eventos tiverem escopo mais restrito.

- No máximo 256 itens cabem em um lote. Aplicam-se os limites de corpo e sanitização existentes.
- O processamento em lote ocorre em linha, na ordem do array. Não há ordenação global
  entre requisições concorrentes nem transação de tudo-ou-nada para um lote.
- Prefira `accepted_indices` quando presente. Caso contrário, `accepted` é o prefixo contíguo
  reconhecido. Preserve cada item não reconhecido para nova tentativa.
- `results` contém um `{index, outcome}` para cada índice reconhecido, na
  ordem dos índices, incluindo um array vazio quando nada foi reconhecido. Os resultados
  são `stored`, `replayed`, `resumed`, `ignored_end`, `dropped_policy`,
  `dropped_subagent`, `dropped_unauthorized`, `dropped_collision` e
  `dropped_invalid` (nenhum ID de sessão fora de um SessionStart, ou uma sessão
  expurgada). Um drop é um
  reconhecimento terminal. Servidores mais antigos omitem `results`; o relay registra
  esses recibos como `unknown`.
- Uma origem com taxa limitada pode ser ignorada enquanto outras origens avançam. Inspecione
  reconhecimentos mesmo em HTTP 429 ou falha parcial; `failed_index` identifica
  uma falha de processamento após ignorados anteriores. Um reconhecimento também pode significar um
  drop deliberado por política, ou um evento que nunca pode ser armazenado (sem ID de sessão
  em qualquer coisa que não seja o início da sessão), e não uma nova observação.
- Em caso de timeout ou resposta perdida, tente novamente os itens e chaves inalterados. Reduza o ritmo (backoff) em caso
  de saturação em vez de abrir um número ilimitado de requisições.

Mantenha uma fila durável do lado do produtor se a recuperação offline/após reinicialização for importante.
O [lifecycle relay](../../companions/ai-memory-relay) opcional fornece uma por meio
de uma CLI separada. Ele aceita eventos do orquestrador e envia apenas o primeiro
evento pendente de cada sessão em um lote.
Descarregue (flush) eventos anteriores de uma sessão antes de enviar seu evento terminal. Serialize
a entrega dentro de uma sessão onde a ordem importar; sessões diferentes podem compartilhar o
servidor concorrentemente. Observações tardias e um evento terminal posterior usam o comportamento
existente de sessão retomada. Interromper um pai não sintetiza encerramentos para seus
filhos; o orquestrador deve relatar essas transições de ciclo de vida.

`POST /hook` permanece disponível, mas sua resposta `202` antecede a persistência.
Prefira `/hook/batch` quando o produtor precisar de um reconhecimento de processamento em linha.

## Limites

A variável não apaga nem cancela entradas de spool existentes. `hook-drain`
explícito e outros hooks avulsos ainda podem entregar essa pendência.
Integrações geradas também mantêm seu comportamento existente de descarte de fila,
o que pode descarregar eventos capturados anteriormente. Escolha o proprietário da captura antes
do primeiro evento da execução; mudar no meio do caminho não consegue desduplicar eventos já
produzidos por caminhos independentes.

O backfill manual, o importador, `finalize-session` e a importação de transcrição gerenciada
via `ai-memory run` mantêm seu comportamento existente. Não os combine como
produtores de ciclo de vida independentes para a mesma execução. A captura externa não
desativa arquivos de transcrição nativos de harnesses nem hooks de outros fornecedores.

Autenticação, verificações de proprietário, regras de admissão e sanitização ainda se aplicam. Todos os itens em
um lote compartilham uma única identidade autenticada; separe lotes por operador ao
representar múltiplos usuários. `extension` é uma proveniência autodeclarada e a
variável de captura não concede permissões. Modos de acesso e concessões de projeto se aplicam
a tokens de usuário do banco de dados. As páginas são compartilhadas por membros autorizados do projeto; a posse de sessão e
handoff ainda se aplica. Namespaces de produtores fornecem apenas proveniência.
Consulte [atribuição multiusuário](users.md).

Nenhum adaptador de framework, mecanismo de fluxo de trabalho, runtime genérico de plugin ou coletor
OpenTelemetry é introduzido. Um futuro adaptador OpenTelemetry poderá mapear identidades de
conversa e chamadas de ferramentas na fronteira. Mantenha a identidade do produtor de ingestão
separada do provedor de modelo e preserve a identidade de sessão nativa necessária
pelas integrações existentes do ai-memory.
