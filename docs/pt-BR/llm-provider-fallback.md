# Cadeias de fallback de provedores LLM

Issue: #648

O provedor LLM de nível superior pode usar uma cadeia ordenada de fallback. Tarefas individuais
também podem selecionar seu próprio provedor e cadeia de fallback.

## Provedores específicos por tarefa

Configure uma tabela opcional `[llm.<task>]` para qualquer uma destas tarefas:
`consolidation`, `bootstrap`, `auto_improve`, `experience`, `dream`, `lint`,
`profile`, `rerank` e `query_answer`. O provedor de `query_answer` lida
tanto com `memory_query(answer=true)` quanto com `memory_explore`. O lint agendado é
apenas por regras, a menos que `[maintenance] lint_llm_interval_secs` ative a passagem com LLM,
que usa o provedor de `lint`.

```toml
[llm.bootstrap]
provider = "openai-compat"
model = "local-model"
base_url = "http://127.0.0.1:49375/v1"
api_key_env = "AI_MEMORY_LOCAL_ROUTER_TOKEN"

[[llm.bootstrap.fallbacks]]
provider = "gemini"
model = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"
```

O `provider` da tarefa é obrigatório e não pode ser vazio. `model` assume como padrão o
modelo padrão daquele provedor, exceto para `openai-compat`, que exige tanto `model` quanto
`base_url`. Uma tarefa nunca herda o endpoint global. `api_key_env` nomeia uma
variável de ambiente, não um valor de credencial. Sem ela, o provedor usa
sua fonte normal de credenciais. OpenAI OAuth, Codex e Cursor mantêm seus
arquivos de login nativos ou CLI e rejeitam `api_key_env`. Uma variável
nomeada explicitamente que esteja ausente causa falha na inicialização do `ai-memory serve` e nunca
recorre à chave do provedor padrão como fallback.

Quando uma tarefa não possui perfil, ela compartilha a mesma instância de provedor padrão e a mesma
cadeia de fallback. O perfil de uma tarefa substitui essa cadeia por seu próprio primário e seus
fallbacks ordenados; fallbacks globais não são anexados. Perfis de tarefas funcionam quando
o provedor global não está definido, sem habilitar outras tarefas. Configurações existentes de timeout,
compatibilidade, cabeçalhos e raciocínio se aplicam globalmente, assim como
`llm_quota_cooldown_secs` (veja
[Exaustão de crédito e cota](#exaustão-de-crédito-e-cota)).

Campos escalares possuem formas aninhadas de variáveis de ambiente, como
`AI_MEMORY_LLM__BOOTSTRAP__PROVIDER` e `AI_MEMORY_LLM__BOOTSTRAP__MODEL`.
Listas de fallback são configuradas em `config.toml`, e não por meio de variáveis
de ambiente. Para os campos obrigatórios de cada fallback, consulte
[`config-properties.md`](config-properties.md#llm_fallbacks).

## Design original da cadeia

O texto a seguir descreve a cadeia global de fallback original.

## Objetivos

- Fazer failover apenas após uma falha transitória do provedor, usando a política
  estabelecida em `LlmError::is_transient()`, ou após o provedor informar que a
  conta está sem crédito, cota ou faturamento (`LlmError::is_quota_exhausted()`).
- Preservar a requisição original, o esquema JSON e o ID lógico da operação em cada
  tentativa de candidato.
- Manter credenciais no carregamento de configuração único existente e fora de logs,
  payloads de status e estado persistido.
- Usar o timeout global de requisição configurado para cada candidato.
- Tornar o candidato selecionado e o resultado do failover observáveis por meio de relatórios
  passivos de integridade do provedor.

## Não-objetivos

- Um roteador para APIs arbitrárias de terceiros ou uma integração com Command Code.
- Ler credenciais do banco de dados de outra aplicação ou instalar uma
  CLI de provedor.
- Tentar novamente falhas determinísticas, como autenticação, requisições inválidas,
  esquema incompatível ou respostas malformadas.
- Alterar embeddings, latência de hook, comportamento de wiki/armazenamento ou esquemas MCP na
  primeira implementação.

## Formato de configuração

Os campos LLM de nível superior atuais permanecem como o perfil primário. Os fallbacks ficam em
`config.toml` para que um perfil possa conter uma URL base explícita e um
*nome de variável de ambiente* de credencial sem colocar valores de credenciais na configuração.

```toml
llm_provider = "opencode"
llm_model = "mimo-v2.5-free"

[[llm_fallbacks]]
provider = "openai-compat"
model = "poolside/laguna-s-2.1-free"
base_url = "http://127.0.0.1:49375/v1"
api_key_env = "AI_MEMORY_LOCAL_ROUTER_TOKEN"

[[llm_fallbacks]]
provider = "gemini"
model = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"
```

`api_key_env` é opcional quando o provedor já possui uma fonte nativa de
credenciais, como OpenAI OAuth ou Copilot. O carregador valida cada perfil e
resolve todo o material de credencial uma única vez. Um provedor inválido ou perfil malformado
falha todos os comandos no carregamento. Uma credencial cuja variável `api_key_env` esteja ausente
falha o `ai-memory serve` na inicialização, em vez de deixar um fallback latente que
só falharia durante uma interrupção, mas não impede outros comandos: `ai-memory
status` e os outros clientes leves nunca montam a cadeia, portanto a variável só
precisa estar no ambiente do servidor (por exemplo, no bloco de ambiente de um wrapper de
serviço), e não exportada em todo shell.

O primário executa primeiro e, em seguida, os fallbacks na ordem de declaração. Perfis
de fallback não possuem atalho por variável de ambiente.

## Fronteira de implementação

Adicione `FallbackLlmProvider` em `crates/ai-memory-llm/src/fallback.rs`:

```text
Config::load
  -> primary ProviderConfig + fallback ProviderConfig values
  -> build_provider for each value
  -> FallbackLlmProvider(Vec<Candidate>)
  -> existing Arc<dyn LlmProvider> consumers
```

Cada `Candidate` registra apenas o nome e o modelo do provedor para diagnóstico mais seu
`Arc<dyn LlmProvider>`. Ele não retém a configuração bruta ou credenciais.
O wrapper implementa todos os quatro pontos de entrada da trait:

- `complete`
- `complete_with_operation_id`
- `complete_structured_raw`
- `complete_structured_raw_with_operation_id`

Cada método chama o método do candidato correspondente. Uma chamada estruturada passa
o esquema inalterado para cada candidato elegível; uma chamada com reconhecimento de operação passa
o mesmo `LlmOperationId` para cada candidato elegível. Isso mantém intactas a semântica de
nova tentativa do chamador e a idempotência.

`build_provider` continua sendo o único caminho de construção de dialeto/autenticação. A CLI permanece
leve: `Config::llm_provider_chain()` pode retornar `None`, um único provedor ou
o wrapper, enquanto os chamadores continuam recebendo `Option<Arc<dyn LlmProvider>>`.

## Política de falhas e estado do circuito

| Falha | Tentar próximo candidato? | Justificativa |
| --- | --- | --- |
| 429 | Sim | Política transitória existente. |
| 5xx | Sim | Política transitória existente. |
| timeout / erro de conexão | Sim | Política transitória existente. |
| crédito / cota / faturamento esgotado, conta desativada (qualquer status, veja abaixo) | Sim | A mesma requisição tem sucesso em outro lugar; este provedor falha até alguém recarregar ou a cota reiniciar. |
| 400 / 401 / 403 / 404 / 422 sem sinal de crédito ou cota | Não | Geralmente configuração de requisição, funcionalidade, modelo ou credencial. |
| esquema / formato de resposta / erro de desserialização | Não | A mesma entrada falharia determinísticamente de novo. |

A mesma regra cobre as trocas de credenciais à frente de um provedor: a
troca de token do Copilot e a renovação de `openai-oauth`/OIDC. Um 429 ou 5xx
desses endpoints de token é transitório, pois a concessão não foi rejeitada; qualquer
outro valor diferente de 2xx permanece como falha de autenticação com sua dica de novo login.

Antes de um candidato ser chamado, o wrapper verifica o circuito em memória daquela
entrada da cadeia. Duas entradas com o mesmo provedor e modelo, mas credenciais diferentes,
portanto, alternam de forma independente. Um erro transitório abre apenas o circuito daquela
entrada. O tempo de espera (cooldown) padrão é de 30 segundos; um cabeçalho `Retry-After` em delta de segundos
ou detalhe de resposta `RetryInfo.retryDelay` do Google o estende, limitado a
uma hora. Outros candidatos continuam elegíveis. Uma resposta bem-sucedida fecha o
circuito daquele candidato. Reiniciar o servidor limpa os circuitos. A implementação
não usa estado de circuito durável nem prazo de requisição forçado separado.

Se todos os candidatos elegíveis falharem, o wrapper retorna um erro agregado delimitado
contendo rótulos de provedor/modelo e classes de erro, nunca corpos de resposta
ou segredos. Um candidato ignorado porque seu circuito está aberto aparece como
`provider/model: skipped (circuit open: quota-exhausted)` (ou `transient`), de
modo que o erro diz por que um candidato não foi tentado.

## Exaustão de crédito e cota

Quando uma conta fica sem crédito, o status HTTP sozinho se parece com uma
requisição malformada ou um limite de taxa curto: a Anthropic envia
`400 invalid_request_error`, a OpenAI `429 insufficient_quota` e o OpenRouter
`402`. Antes, só o `429` avançava a cadeia, então um primário Anthropic com
saldo zerado falhava toda consolidação e o fallback configurado nunca rodava.

`LlmError::failover_class()` retorna `Transient`, `QuotaExhausted` ou
`Permanent`, e `is_transient()`, `is_quota_exhausted()` e
`should_fail_over()` derivam dele. A verificação de cota fica em
`crates/ai-memory-llm/src/quota.rs`. Ela lê apenas o corpo de erro delimitado que
`LlmError::Provider` já carrega (no máximo 1 KiB, com segredos removidos),
retorna um booleano e não registra nenhuma parte do corpo.

| Provedor | Sinal tratado como `QuotaExhausted` |
| --- | --- |
| `anthropic`, `anthropic-oauth` | `credit balance is too low` (HTTP 400), `billing_error` (402), `reached your specified API usage limits` (limite mensal de gasto), `organization has been disabled` (403) |
| `openai` | `insufficient_quota` (429), `billing_hard_limit_reached`, `billing_not_active`, `account_deactivated` (401) |
| `openai-compat` (OpenRouter, DeepSeek e gateways similares) | qualquer `402`; `insufficient credits`, `insufficient balance`, `out of credits`, `requires more credits`, `key limit exceeded` |
| `gemini` | `429 RESOURCE_EXHAUSTED` cuja cota violada seja diária (`PerDay`) ou tenha `limit: 0`; `spending cap`; `credits are depleted` |
| `openai-oauth`, `codex` | `usage_limit_reached` / `usage limit has been reached` (também quando chega como erro terminal de SSE, reportado como 502) |
| `copilot` | `quota_exceeded`, `monthly quota` |
| `opencode` | qualquer `402`; `insufficient balance`, `out of credits` |

Os marcadores são comparados sem diferenciar maiúsculas de minúsculas contra o
corpo de cada provedor, porque gateways repassam os erros uns dos outros. Um
limite de taxa por minuto continua transitório. O Gemini reutiliza o texto
"exceeded your current quota" da OpenAI para seu limite por minuto, então essa
frase não é um marcador.

Um `401` comum (uma chave inválida, revogada ou digitada errada) nunca faz
failover. O operador precisa corrigir a chave, e mover toda chamada para o
fallback esconderia o problema. Um `401` ou `403` só faz failover quando seu
corpo carrega um marcador explícito de faturamento, cota, crédito ou conta
desativada da tabela, como `account_deactivated` da OpenAI ou a organização
desativada da Anthropic.

Os laços de nova tentativa da consolidação e do bootstrap não reenviam um erro
de cota esgotada dois segundos depois, mesmo quando ele chega como um `429`,
porque ele não é transitório. A fila de consolidação do SessionEnd mantém esse
trabalho em seu backoff delimitado em vez de marcá-lo como terminal: uma
recarga ou reinício de cota pode resolvê-lo dentro dessa janela, como sempre
pôde para um `429`.

Uma falha de cota abre o circuito do candidato por `llm_quota_cooldown_secs`
(padrão 900, faixa aceita de 30 a 86400, env
`AI_MEMORY_LLM_QUOTA_COOLDOWN_SECS`). Uma dica de nova tentativa mais longa do
provedor, como um horário de reinício da cota, estende a janela até uma hora ou
o valor configurado, o que for maior. Uma falha transitória que chega enquanto
a janela está aberta não a encurta nem muda seu motivo. O candidato não recebe
tráfego enquanto seu circuito está aberto. A primeira chamada real após a
janela o tenta de novo: se ele responder, volta a assumir e seu circuito fecha;
se ainda estiver esgotado, essa chamada custa uma requisição rejeitada e a
janela recomeça. Nada sonda em segundo plano, e um reinício limpa o estado.

Exemplo, Anthropic como primário com a OpenAI assumindo quando o saldo da
Anthropic acaba:

```toml
llm_provider = "anthropic"
llm_model = "claude-sonnet-4-6"
llm_quota_cooldown_secs = 900

[[llm_fallbacks]]
provider = "openai"
model = "gpt-5.4-mini"
api_key_env = "OPENAI_API_KEY"
```

`ANTHROPIC_API_KEY` e `OPENAI_API_KEY` precisam estar ambos no ambiente do
servidor.

## Relatório de integridade

`ProviderHealth` preserva o papel global de LLM e sua lista de candidatos.
O campo `llm_task_chains` relata cada substituição explícita de tarefa separadamente,
com seu provedor, modelo, status e lista de candidatos. Configurações somente por tarefa
mantêm o papel global desativado enquanto registram as métricas de sua própria carga de trabalho.
O estado por candidato contém:

- provedor e modelo;
- estado configurado / selecionado por último;
- timestamp do último sucesso ou erro;
- classe de erro ofuscada (`quota-exhausted` para uma falha de crédito ou cota)
  e status HTTP quando disponíveis;
- timestamp de circuito-aberto-até e `circuit_reason` (`transient` ou
  `quota-exhausted`) enquanto está aberto.

`ai-memory status` imprime uma manchete quando o primário está sem crédito ou
cota, por exemplo
`primary anthropic/claude-sonnet-4-6 exhausted (credit/quota); using fallback openai/gpt-5.4-mini`,
e marca aquele candidato como `out of credit/quota; skipped until <time>, then
probed again`.

O status registra apenas chamadas que o servidor já fez. Ele não sonda fallbacks
nem dispara tráfego de recuperação em segundo plano.

O log do servidor traz os mesmos eventos conforme eles acontecem, com os mesmos
campos ofuscados e nunca um corpo de resposta. Uma falha transitória registra um aviso citando
o provedor, modelo, posição na cadeia, classe de erro, status HTTP
e tempo de espera do candidato com falha; esse é o único lugar onde o modelo com falha aparece,
porque chamadores como o reranker rotulam a cadeia com seu primeiro
candidato. Uma falha de cota registra seu próprio aviso (`out of credit or
quota`) com os mesmos campos ofuscados e o tempo de espera de cota. Uma falha
determinística é registrada em nível de debug, já que o próprio chamador
relata o erro retornado. Uma linha de info registra cada mudança do
candidato que responde, portanto um primário que permanece inativo gera uma linha, e não uma
por chamada.

## Testes

1. Testar em unidade a ordem da cadeia com instâncias simuladas de `LlmProvider`.
2. Verificar se todos os quatro métodos da trait preservam requisição/esquema/id de operação.
3. Verificar se falhas de `429`, `5xx`, timeout e conexão avançam; verificar se
   erros determinísticos param no primeiro candidato. Verificar que o corpo de
   crédito/cota de cada provedor avança (fixtures de unidade em `quota.rs`,
   provedores reais contra um mock local em `tests/suite/fallback_provider.rs`),
   que o candidato esgotado é ignorado durante a janela de cota e sondado depois
   dela, e que um `401` de chave inválida comum não avança.
4. Verificar se um circuito aberto ignora apenas seu candidato `(provider, model)` e se um
   sucesso o fecha.
5. Testar a validação de configuração, incluindo perfis ausentes/vazios e credenciais
   ausentes, por meio de `Config::load()` sem ler um diretório base real;
   uma credencial ausente é checada por `require_llm_fallback_credentials()` e
   `llm_provider_chain()`, não por `load`.
6. Testar snapshots de integridade para rótulos de candidatos e ofuscação.
7. Manter testes de configuração existentes de provedor único compatíveis byte a byte.

## Sequência de entrega

1. Adicionar análise de perfil, validação e testes sem alteração de comportamento quando nenhum
   perfil de fallback existir.
2. Adicionar o wrapper `FallbackLlmProvider` e testes focados com provedores simulados.
3. Integrá-lo ao `serve`, estender a integridade passiva e adicionar a documentação de configuração
   e status, além da entrada em `CHANGELOG.md`.
4. Executar os gates do workspace e casos manuais de `llm-test` contra um endpoint local
compatível com OpenAI que primeiro retorne uma falha transitória e depois uma
conclusão estruturada válida.
