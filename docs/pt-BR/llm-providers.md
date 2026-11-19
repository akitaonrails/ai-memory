# Provedores de LLM

> Configuração de provedores para consolidação e embeddings. Movido sem alterações da página inicial do README.

O ai-memory funciona sem um LLM: os hooks continuam capturando sessões, a busca usa
FTS5 + entidades declaradas + vizinhos do grafo, e os resumos recorrem a
uma saída baseada em regras. Adicione um provedor de LLM
quando quiser consolidação por LLM (em PreCompact / PostCompaction, sob demanda através de
`memory_consolidate`, ou opcionalmente no final da sessão com
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END`), linting mais rico e inicialização (bootstrap).
Finais de sessão substanciais sempre geram uma página de resumo baseada em regras + handoff de qualquer
forma. Uma sessão contendo apenas limites de `SessionStart` / `SessionEnd` é
encerrada sem página, handoff ou tarefa no provedor. Quando essa sessão vazia tiver
aceito o contexto de inicialização, seu handoff vinculado à sessão é retornado ao pool
aberto para o próximo receptor, em vez de ser perdido.
PreCompact / PostCompaction gravam um ponto de verificação (checkpoint) baseado em regras na requisição do hook.
O trabalho do provedor para esses pontos de verificação, e para o SessionEnd opcional, é enfileirado de forma
durável após essas gravações determinísticas e tratado por um worker delimitado do
servidor, de modo que a latência de esvaziamento do hook não o cancele. O worker inicia sempre que um
consolidador estiver configurado; o enfileiramento de SessionEnd permanece condicionado à flag opcional.
Uma reescrita de ponto de verificação ainda enfileirada quando a sessão termina é executada uma vez mesmo assim,
mesmo com a flag desativada. Uma que já esteja em execução quando a sessão termina é finalizada após
o resumo de SessionEnd e o substitui; assim, com a flag desativada, a página mais recente pode
omitir o final da sessão; o resumo permanece acessível como a versão substituída.
Toda tarefa enfileirada é executada como o proprietário da sessão registrado no início da sessão,
não anonimamente. Tarefas com falha tentam novamente com intervalo incremental e sobrevivem a uma reinicialização do servidor. Uma sessão nativa retomada só é encerrada novamente após
o avanço da geração de sua observação; a marca d'água persistida da geração faz com que a
entrega duplicada de SessionEnd e a distorção do relógio do sistema convirjam sem trabalho
repetido no provedor. A marca d'água de encerramento e o handoff automático são confirmados atomicamente, e
uma reprodução chaveada interrompida conclui o commit no wiki, a inserção na fila e a
conclusão da chave sem duplicar esse handoff. No próximo SessionStart, vence o
handoff automático elegível ao cwd mais recente; aceitá-lo expira handoffs automáticos elegíveis
mais antigos sem consumir o trabalho manual ou de diretórios irmãos. Um novo
handoff automático também expira handoffs automáticos abertos anteriores do seu cwd exato,
para que SessionEnds repetidos não se acumulem lá antes que um receptor inicie.

Para manter o estilo de consolidação específico de cada projeto, escreva
`_prompts/consolidation.md` no wiki desse projeto. O corpo pode expressar
preferências como "preferir títulos em português" ou "omitir ruídos rotineiros do CI".
A consolidação automática, de página única e de várias páginas usa a página; uma chamada
manual a `memory_consolidate` pode passar `instructions` para substituí-la uma vez. O ai-memory
higieniza e limita o valor a 2.000 caracteres, codifica-o em JSON na mensagem
do usuário e o trata como dados consultivos não confiáveis. Ele não pode fornecer fatos,
solicitar o uso de ferramentas ou a divulgação, nem substituir o esquema de consolidação e
as regras de fidelidade. Páginas de preferência expiradas por TTL são ignoradas. Sem página
ativa ou argumento, nenhum bloco de preferência é anexado.

Padrões recomendados:

| Provedor | Padrão | Quando usar |
|---|---|---|
| `anthropic` | `claude-haiku-4-5` | Melhor padrão para qualidade de consolidação e classificação de regras. |
| `anthropic-oauth` | `claude-sonnet-4-6` | Use uma assinatura do Claude Pro/Max via `claude setup-token`, sem chave de API. Atualmente recusado pela Anthropic para aplicativos de terceiros (geralmente `429`); veja abaixo. |
| `openai` | `gpt-5.4-mini` | Opção hospedada mais barata e mais rápida. |
| `openai-oauth` | `gpt-5.5` | Backend do ChatGPT Pro/Plus/Codex via `ai-memory auth login openai-oauth`; sem chave de API da Platform. |
| `codex` | `gpt-5.6-luna` | Reutiliza o `auth.json` gerenciado pela CLI do Codex; a atualização do token de acesso continua sob responsabilidade do `codex app-server`. |
| `cursor` | `cursor-grok-4.6-high` | Assinatura do Cursor via CLI do `agent` autenticado. Executa `agent --print --mode ask` (somente leitura). Não passa `--yolo`. O id padrão é o que o `agent --list-models` rotula como "Grok 4.6". |
| `copilot` | `gpt-5.5` | Backend do GitHub Copilot Chat via `ai-memory auth login copilot` ou `COPILOT_GITHUB_TOKEN`; requer uma assinatura do Copilot. |
| `gemini` | `gemini-3.5-flash` | Opção hospedada pelo Google com um nível gratuito generoso. |
| `opencode` | `mimo-v2.6-flash` | OpenCode Go ou Zen via `OPENCODE_API_KEY`. Go é o endpoint padrão; `AI_MEMORY_LLM_BASE_URL` seleciona o Zen. Defina `AI_MEMORY_LLM_MODEL` para um id fornecido pelo endpoint escolhido. |
| `openai-compat` | sem padrão | OpenRouter, Atlas Cloud, OrcaRouter, Cheaper Inference, API Route, FutureInfra, Ollama, vLLM, LM Studio e outros endpoints compatíveis. |
| `openai-compat` + `AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1` | sem padrão (recomendado: `anthropic/claude-haiku-4.5`) | Acesso hospedado a um amplo catálogo de modelos por meio de uma única chave. Veja [OpenRouter](#openrouter) abaixo e a comparação empírica em [`llm-provider-comparison.md`](llm-provider-comparison.md). |

`openai-oauth` armazena um token de atualização (refresh token) em `<data_dir>/auth.json` e comunica-se com
o backend de Responses do ChatGPT/Codex, não com `api.openai.com`. Para inícios rápidos
com Docker, execute `ai-memory auth login openai-oauth` com o wrapper para que o token
fique no mesmo volume `ai-memory-data` que o servidor.

`codex` é independente do `openai-oauth`: ele nunca copia credenciais para
o diretório de dados do ai-memory e nunca lê um token de atualização ou token de ID. Ele
lê apenas `tokens.access_token` e `tokens.account_id` de
`$CODEX_HOME/auth.json`, ou de `.codex/auth.json` na home da plataforma quando
`CODEX_HOME` não estiver definido ou estiver vazio. Cada requisição recarrega o arquivo. Um primeiro 401 pode
disparar uma recuperação serializada com `codex app-server --stdio` seguida por uma
nova tentativa; respostas 401 adicionais falham com uma dica de reautenticação. Substitua o
binário com `AI_MEMORY_CODEX_EXECUTABLE` quando `codex` não estiver no `PATH`.

```bash
export AI_MEMORY_LLM_PROVIDER=codex
export AI_MEMORY_LLM_MODEL=gpt-5.6-luna
export AI_MEMORY_LLM_REASONING_EFFORT=medium
ai-memory llm-test --provider codex --model gpt-5.6-luna --prompt "Reply with OK"
ai-memory llm-test --provider codex --model gpt-5.6-luna --structured --prompt "Return a short answer"
```

`cursor` usa o Cursor Agent já autenticado nesta máquina. Defina
`AI_MEMORY_LLM_PROVIDER=cursor`. O modelo padrão é `cursor-grok-4.6-high`
("Grok 4.6" em `agent --list-models`); qualquer outro id dessa lista funciona.
`AI_MEMORY_CURSOR_AGENT` substitui o binário. O provedor grava o prompt
em um espaço de trabalho temporário e chama `agent --print --mode ask --trust`. Ele
não passa `--yolo` nem `--force`: o texto de consolidação é conteúdo capturado não confiável,
e essas flags permitiriam a execução de ferramentas. A CLI do Cursor não tem
modo de JSON-schema, portanto o esquema trafega no prompt. A resposta é analisada
de forma tolerante (cercas de código e a prosa ao redor são removidas) e depois
desserializada no tipo esperado; nada a verifica em relação ao próprio esquema,
portanto um valor desserializado é aceito mesmo se violar uma restrição
que o serde não impõe. O processo do `agent` não herda o ambiente
do servidor. Ele recebe `PATH`, `HOME`, variáveis de usuário e localidade (`LANG`,
`LC_*`), diretórios temporários e XDG, `DBUS_SESSION_BUS_ADDRESS`, configurações
de proxy e CA, os padrões de processos do Windows e todas as variáveis `CURSOR_*`, de forma que
`AI_MEMORY_AUTH_TOKEN` e chaves de outros provedores nunca o alcancem. O espaço de trabalho
por chamada é criado no diretório temporário do sistema capturado na inicialização.

```bash
export AI_MEMORY_LLM_PROVIDER=cursor
export AI_MEMORY_LLM_MODEL=cursor-grok-4.6-high
ai-memory llm-test --provider cursor --model cursor-grok-4.6-high --prompt "Reply with OK"
```

O modo `file` de armazenamento de credenciais do Codex é aceito. `auto` funciona apenas quando sua
credencial efetiva estiver presente em `auth.json`; credenciais somente de chaveiro (keyring) e efêmeras
não são lidas e produzem um erro acionável de arquivo de autenticação ausente.
O Docker não é configurado automaticamente: o executável do Codex, `CODEX_HOME`
e seu arquivo de credenciais devem existir todos no mesmo contêiner/ambiente que
o ai-memory.

`opencode` comunica-se com o gateway do OpenCode usando a chave `sk-...` de
`opencode.ai/auth`, lida apenas de `OPENCODE_API_KEY`; ele não recorre
a `LLM_API_KEY`. O padrão é o endpoint **Go**,
`https://opencode.ai/zen/go/v1`: uma assinatura com uma cota mensal
por modelo, em vez de cobrança por token. Defina
`AI_MEMORY_LLM_BASE_URL=https://opencode.ai/zen/v1` (ou `llm_base_url` em
`config.toml`) para o catálogo pago por token do **Zen**. Os ids de modelos são por
catálogo e escritos de forma simples (`mimo-v2.5`, `glm-5.3-flash`), não na
forma `opencode-go/<model>` que a configuração do cliente próprio do OpenCode usa. O padrão
embutido é `mimo-v2.6-flash`, servido pelo Go. Ao selecionar o Zen, defina
`AI_MEMORY_LLM_MODEL` para um modelo do catálogo dele, como `claude-sonnet-5-5`.
O provedor preserva o catálogo escolhido e não alterna produtos de faturamento
automaticamente. Modelos com Responses publicados, incluindo `gpt-5.6-luna` e
`gpt-6-luna`, usam `/responses`. A API de rede segue o catálogo selecionado:
`minimax-m3`, `minimax-m2.7` e `qwen3.8-max` usam `/messages` no Go e
Chat Completions no Zen. O `muse-spark-1.3-contributor` e o
`muse-spark-1.2-contributor` do Go usam `/responses`. Modelos Claude compatíveis e os demais
modelos Qwen mapeados usam `/messages`; os modelos restantes mantêm Chat Completions.
Alterar a URL base reseleciona o transporte e preserva o tempo limite (timeout), esforço
de raciocínio e cabeçalhos do operador. O Go é identificado por um caminho de URL terminando em
`/zen/go` ou `/zen/go/v1`, opcionalmente incluindo um endpoint completo;
URLs de proxy customizadas com outros caminhos usam o mapeamento do Zen.
Consulte os [Go endpoints](https://opencode.ai/docs/go/#endpoints) e
[Zen endpoints](https://opencode.ai/docs/zen/#endpoints) oficiais, cujas tabelas de origem
são [go.mdx](https://github.com/anomalyco/opencode/blob/dev/packages/web/src/content/docs/go.mdx)
e [zen.mdx](https://github.com/anomalyco/opencode/blob/dev/packages/web/src/content/docs/zen.mdx).
O provedor envia o
cabeçalho de correlação `x-opencode-session` solicitado pelo OpenCode, um id por operação
lógica, e seu próprio `User-Agent`; `AI_MEMORY_LLM_HEADERS` substitui
qualquer um deles. `AI_MEMORY_LLM_REASONING_EFFORT` e `AI_MEMORY_LLM_TIMEOUT_SECS`
aplicam-se como em qualquer outro provedor. `AI_MEMORY_LLM_PROVIDER` / `llm_provider`
também aceitam os aliases históricos `opencode-zen` e `opencode_zen` para o
mesmo provedor (`llm-test --provider` aceita apenas `opencode`); o endpoint é
escolhido pela URL base, não pelo alias.

```bash
export AI_MEMORY_LLM_PROVIDER=opencode
export AI_MEMORY_LLM_MODEL=mimo-v2.6-flash
ai-memory llm-test --provider opencode --model mimo-v2.6-flash --prompt "Reply with OK"
```

Para o Claude através do Zen:

```bash
export AI_MEMORY_LLM_PROVIDER=opencode
export AI_MEMORY_LLM_BASE_URL=https://opencode.ai/zen/v1
export AI_MEMORY_LLM_MODEL=claude-sonnet-5-5
ai-memory llm-test --provider opencode --model claude-sonnet-5-5 --prompt "Reply with OK"
```

<a id="openrouter"></a>
O [OpenRouter](https://openrouter.ai) é usado através do provedor genérico `openai-compat`,
a mesma estrutura usada por Atlas Cloud, OrcaRouter, Ollama, vLLM e LM
Studio. Não há um nome de provedor dedicado `openrouter`, e definir
`AI_MEMORY_LLM_PROVIDER=openrouter` falha na inicialização com
`AI_MEMORY_LLM_PROVIDER=openrouter is not one of anthropic|openai|gemini|openai-compat|...`.
Selecione-o apontando a URL base do compat para o OpenRouter e fornecendo sua
chave `sk-or-v1-...` através de `LLM_API_KEY`:

Um arquivo de ambiente funcional mínimo (coloque em `~/.config/ai-memory/openrouter.env`
ou no argumento `--env-file` do Docker):

```env
LLM_API_KEY=sk-or-v1-...

AI_MEMORY_LLM_PROVIDER=openai-compat
AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1
AI_MEMORY_LLM_MODEL=deepseek/deepseek-v4-flash
AI_MEMORY_LLM_COMPAT_STRICT=true
# Set only when the endpoint defaults an omitted field to SSE:
AI_MEMORY_LLM_COMPAT_STREAM=false
```

Depois verifique:

```bash
ai-memory llm-test --provider openai-compat \
    --model deepseek/deepseek-v4-flash --prompt "Reply with OK"
```

`AI_MEMORY_LLM_COMPAT_STRICT=true` envia `response_format=json_schema` no estilo
OpenAI para cada chamada estruturada. Gateways hospedados modernos o respeitam;
alguns backends de modelos (DeepSeek entre eles) de outra forma envolvem sua saída JSON
em cercas markdown que o analisador tolerante do ai-memory precisa remover,
o que ocasionalmente descarta um campo. Defini-lo como `true` no nível do
arquivo de ambiente é mais seguro para a confiabilidade da consolidação. Desative-o
(`AI_MEMORY_LLM_COMPAT_STRICT=false`) apenas para o backend de um modelo específico
que rejeite explicitamente o formato restrito ao esquema.

`AI_MEMORY_LLM_COMPAT_STREAM` controla o campo `stream` nas requisições
de Chat Completions do `openai-compat`. Deixe-o indefinido para omitir o campo,
preservando o formato de requisição existente. Defina-o como `false` quando um endpoint
tornar Server-Sent Events o padrão para campos omitidos. Defini-lo como `true` é
rejeitado antes de qualquer requisição, pois esse transporte espera uma única resposta JSON
e não decodifica SSE. Use `false` ou deixe a configuração indefinida.

O arquivo `docker/.env.production.example` no repositório já vem com a configuração
do OpenRouter pré-preenchida, com `moonshotai/kimi-k2.6` como modelo de exemplo (veja as
gotchas abaixo para entender por que modelos de raciocínio não são elegíveis). Os IDs de modelos seguem
a convenção `provider/model` do OpenRouter. Não há um padrão embutido:
`AI_MEMORY_LLM_MODEL` é obrigatório, como é para todo endpoint `openai-compat`.

### Escolhendo um modelo

Para orientações sobre a seleção de LLMs, consulte
[`llm-provider-comparison.md`](llm-provider-comparison.md), onde o projeto
mantém seus próprios testes A/B de qualidade de consolidação, latência e custo. Essa comparação usa amostras de consolidação em inglês e aborda o
padrão recomendado (`anthropic/claude-haiku-4.5`), a alternativa mais barata
(`openai/gpt-5.4-mini`) e a observação de inelegibilidade de modelos de raciocínio; a
subseção do OpenRouter aqui se remete a ele em vez de duplicar as classificações.

Para cargas de trabalho dominadas por conteúdo que não seja em inglês, relatos da comunidade sugerem
que um embedder multilíngue de maior recall (por exemplo, `baai/bge-m3` via
provedor de embeddings `openai-compat`) melhora sensivelmente a qualidade da recuperação
em relação ao embedder local embutido que aceita apenas inglês. A configuração está
no parágrafo de embeddings abaixo.

O ai-memory aplica automaticamente os cabeçalhos de atribuição de app do OpenRouter
(`HTTP-Referer` e `X-Title`) sempre que a URL base apontar para
`openrouter.ai`, para que o tráfego do ai-memory seja creditado no painel de líderes
de apps do OpenRouter. `AI_MEMORY_LLM_HEADERS=HTTP-Referer=https://example.com,X-Title=my-app`
os substitui por operador. `AI_MEMORY_LLM_REASONING_EFFORT` é respeitado neste
caminho: hosts do OpenRouter recebem `reasoning: { effort, exclude: true }`.

A API de LLM/completions do OpenRouter é o que o ai-memory usa acima. Ela **não**
expõe de forma confiável um endpoint `/v1/embeddings` compatível com a OpenAI (historicamente
não tem exposto), portanto não presuma que a URL base do OpenRouter funcione para o embedder. Se uma
determinada implantação do OpenRouter oferecer um endpoint de embeddings, verifique se ele
responde no formato padrão de `/v1/embeddings` (faça um `curl` diretamente no endpoint)
antes de confiar nele. Se funcionar, configure o embedder através do caminho
openai-compat em vez do caminho openai, que tem uma cadeia de precedência de chaves
específica da OpenAI:

```bash
export AI_MEMORY_EMBEDDING_PROVIDER=openai-compat
export AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1
export AI_MEMORY_EMBEDDING_MODEL=baai/bge-m3
export AI_MEMORY_EMBEDDING_DIM=1024
# LLM_API_KEY is reused automatically when EMBEDDING_API_KEY is unset on
# the openai-compat embedder path; no second key is required.
```

`AI_MEMORY_EMBEDDING_DIM` é obrigatório para embeddings openai-compat e deve
corresponder ao modelo. Para conteúdo exclusivo em inglês, o sentence-transformer local
embutido (`AI_MEMORY_EMBEDDING_PROVIDER=local`, consulte
[`local-embeddings.md`](local-embeddings.md)) é uma alternativa válida que
evita a saída de dados hospedada; para conteúdo não em inglês, um modelo multilíngue hospedado
(`baai/bge-m3`, `intfloat/multilingual-e5-large` ou similar) servido por qualquer
provedor que exponha um endpoint real de `/v1/embeddings` é a escolha prática.
Os dois endpoints continuam independentes, portanto definir apenas `AI_MEMORY_LLM_BASE_URL`
não redireciona os embeddings; defina `AI_MEMORY_EMBEDDING_BASE_URL` também.

> [!TIP]
> **`openai-compat` envia `X-Request-Id` em cada tentativa de chat.** Cada tentativa HTTP
> de uma operação lógica, incluindo o fallback do modo restrito para o tolerante,
> transporta o mesmo id de operação (um UUID v7 de 36 caracteres) no
> cabeçalho `X-Request-Id`, para que um gateway que o registre (por exemplo, como
> `req=<id>`) possa correlacionar cada tentativa da mesma operação e encaminhá-la
> ao motor (vLLM). Quem chama a operação (a invocação de consolidação
> ou a revisão de autoaprimoramento) gera o id do zero, e ele
> nunca é o id de sessão do agente, portanto um gateway nunca vê duas operações
> de uma sessão, ou uma execução retomada após falha, como uma só. A reentrada após falha é exatamente
> uma nova operação: uma reinicialização de processo ou uma nova reivindicação na fila gera um novo id
> para as chamadas que realiza. O provedor oficial `openai` e o `opencode` não
> enviam este cabeçalho; o `opencode` mantém seu próprio contrato de
> `x-opencode-session`. Como o ai-memory gerencia o cabeçalho no caminho `openai-compat`, uma
> estática entrada `x-request-id` em `AI_MEMORY_LLM_HEADERS` é recusada na
> inicialização para esse provedor; o valor dinâmico não deve ser mascarado ou
> duplicado.

> [!TIP]
> **O `openai-compat` também pode informar o que cada operação é.** Com
> `AI_MEMORY_LLM_REQUEST_METADATA_HEADERS=true` (`llm_request_metadata_headers
> = true`), cada tentativa de chat carrega, junto ao `X-Request-Id`:
>
> | Cabeçalho | Valor | Enviado quando |
> | --- | --- | --- |
> | `x-ai-memory-workload` | `consolidation`, `profile`, `dream`, `lint`, `auto-improve`, `experience`, `bootstrap`, `query-answer`, `rerank` ou `unspecified` | sempre |
> | `x-ai-memory-workspace` | nome do espaço de trabalho | a operação lê um espaço de trabalho |
> | `x-ai-memory-project` | nome do projeto | a operação lê um projeto |
> | `x-ai-memory-user` | nome de usuário | a operação age em nome de um usuário |
>
> Um gateway pode usá-los para atribuir custo ou aplicar um limite por carga de trabalho,
> projeto ou usuário. Um cabeçalho que a operação não consegue nomear com um único valor é omitido,
> nunca adivinhado: um lote de classificação de perfil que mistura frases de vários
> operadores não envia usuário, uma resposta de `memory_query` (cujas
> evidências abrangem projetos) não envia escopo, e a chamada de rerank envia apenas a
> carga de trabalho. Um valor que não é um valor de cabeçalho HTTP válido (um nome de
> projeto não ASCII, por exemplo) também é omitido. Os valores são identificadores,
> nunca conteúdo de página. A configuração vem desativada por padrão, apenas o `openai-compat`
> envia esses cabeçalhos (os provedores oficiais, o `opencode` e uma contingência para
> outro provedor nunca enviam), e os quatro nomes são recusados em
> `AI_MEMORY_LLM_HEADERS` para o `openai-compat`, com a configuração ativada ou não.
> A configuração vale para todo o servidor: uma entrada `openai-compat` em
> `llm_fallbacks` também envia os cabeçalhos ao próprio endpoint, portanto ative-a
> apenas quando todo endpoint `openai-compat` que você configurar puder ver os nomes do seu
> espaço de trabalho, projeto e usuário.

### Contingência quando o crédito ou a cota se esgota

Uma cadeia `[[llm_fallbacks]]` ordenada passa para o próximo provedor diante de
uma falha transitória e também quando um provedor informa que a conta está sem
crédito, cota ou faturamento: o `400` "credit balance is too low" da Anthropic,
o `429 insufficient_quota` da OpenAI, um `402` do OpenRouter, um
`RESOURCE_EXHAUSTED` diário do Gemini, um `usage_limit_reached` do Codex ou um
`quota_exceeded` do Copilot. Um `401` de chave inválida comum não aciona a
contingência. O provedor esgotado fica de fora por `llm_quota_cooldown_secs`
(padrão 900). A próxima chamada depois disso o tenta de novo, então ele volta a
assumir assim que for recarregado.

```toml
llm_provider = "anthropic"
llm_model = "claude-sonnet-4-6"

[[llm_fallbacks]]
provider = "openai"
model = "gpt-5.4-mini"
api_key_env = "OPENAI_API_KEY"
```

`ai-memory status` então mostra `primary anthropic/claude-sonnet-4-6 exhausted
(credit/quota); using fallback openai/gpt-5.4-mini`. A regra completa, com os
sinais por provedor, está em
[`llm-provider-fallback.md`](llm-provider-fallback.md#exaustão-de-crédito-e-cota).

### Sondando o próprio provedor do servidor

`ai-memory llm-test` compila um provedor a partir da shell em que é executado. Esse é o
teste correto antes de colocar uma chave na configuração, e funciona com o servidor
inativo — mas é o teste incorreto quando o servidor roda em um contêiner ou em
outro host, porque as chaves existem lá e não na sua shell. Uma falha
como `building LLM provider` (sem chave) ou um `400 Please pass a valid
API key` do provedor não diz nada sobre o servidor nesse caso.

`ai-memory llm-test --server` solicita que o servidor execute a sondagem por conta própria, com seu
próprio provedor configurado, cadeia de fallback e chaves, e imprime o provedor,
modelo, latência e a classe do erro:

```bash
ai-memory llm-test --server
```

É a mesma sondagem que o painel de integridade e o comando `status` indicam quando uma chamada ao provedor
falha; ela executa um único prompt curto e fixo e não aceita `--provider`, `--model`,
`--prompt`, `--base-url` ou `--api-key`: um cliente não pode passar uma chave para o servidor
nem apontá-lo para outra URL. Em um servidor multiusuário, a rota é exclusiva do root, e
o corpo de erro do provedor é higienizado antes de ser impresso, para que uma chave que o provedor
tenha citado de volta nunca chegue ao seu terminal. Sem um provedor configurado, o
servidor responde `503` e informa isso.

### Segurança e cuidados importantes

- **Endereço de bind vs. publicação de porta.** Ao rodar dentro de um contêiner, o
  bind no nível de processo (`--bind 0.0.0.0:49374` ou similar) não decide
  quem pode alcançar a porta; a especificação de publicação do contêiner decide. `-p 127.0.0.1:49374:49374` restringe ao loopback no host;
  `-p 49374:49374` publica em todas as interfaces do host. Se a porta alguma vez
  for alcançável além do loopback, gere um bearer token primeiro:
  ```bash
  ai-memory generate-auth-token
  export AI_MEMORY_AUTH_TOKEN=<token>
  export AI_MEMORY_ALLOWED_HOSTS=<host,host,...>
  ```
  O servidor registra um aviso chamativo na inicialização sobre a exposição fora do loopback
  sem um token; trate-o como impeditivo. `AI_MEMORY_ALLOWED_HOSTS`
  protege contra DNS-rebinding. Espera-se que a terminação TLS fique à
  frente do processo; consulte [`docs/https-via-proxy.md`](https-via-proxy.md).
- **Higiene de segredos.** O arquivo de ambiente que contém `LLM_API_KEY` (e qualquer
  chave específica de provedor que você definir) deve ter permissão `chmod 600` e ficar fora de qualquer
  diretório que uma ferramenta de controle de versão veja. Se fizer backup de
  `~/.config/ai-memory/` (consulte [`docs/backup.md`](backup.md)), exclua
  o arquivo de ambiente. O script de exemplo e o `.gitignore` espelho fornecidos
  lá já excluem `*.env` por padrão.
- **Modelos baratos podem ter desvios (drift).** A consolidação por LLM grava páginas
  duráveis no wiki, portanto um modelo que ocasionalmente fabrique uma página com `kind: decision`
  para uma sessão que não tomou decisão alguma deixa uma página no wiki
  que o ai-memory tratará como evidência de primeira classe em leituras
  subsequentes. Este é um custo conhecido de usar um modelo menor / mais barato no
  prompt de consolidação. Faça verificações amostrais em
  `<data_dir>/wiki/<workspace>/<project>/decisions/` durante a primeira
  semana com um modelo novo em busca de páginas que você não reconhece; se alguma surgir,
  aumente o nível do provedor apenas para consolidação (mantenha o
  modelo mais barato para reranking de recuperação) ou desfaça a definição de
  `AI_MEMORY_LLM_PROVIDER` para recorrer ao caminho determinístico
  baseado em regras.
- **Modelos com sufixo `:free` utilizam uma cota compartilhada.** Qualquer id de modelo
  terminando em `:free` no OpenRouter é servido a partir de um pool compartilhado entre
  todos os usuários do nível gratuito, e não do orçamento pago do chamador. Quando esse
  pool se esgota, as requisições falham com um erro de limite de taxa
  (rate-limit) de pool compartilhado upstream. Eles servem para avaliação, mas não são confiáveis para
  produção; prefira um modelo listado com múltiplos provedores upstream em sua
  página no OpenRouter.
- **Modelos em modo de raciocínio são inelegíveis para consolidação.**
  A consolidação e o linting usam um contrato de saída em JSON estrito; modelos
  de raciocínio direcionam a maior parte de seu orçamento de tokens para raciocínio interno
  e travam além do teto de 300 s por requisição ou retornam strings
  vazias. `docs/install.md#llm-provider-tiers` contém a nota permanente "o que
  não recomendamos" listando esse padrão; o mesmo se aplica a
  qualquer modelo do OpenRouter sinalizado como variante de raciocínio/pensamento. Prefira
  um equivalente sem raciocínio para `AI_MEMORY_LLM_MODEL`.

`anthropic-oauth` acessa o mesmo endpoint `/v1/messages` que `anthropic`, mas
autentica com um bearer token OAuth em vez de uma chave de API. Execute
`claude setup-token` uma vez; ele imprime o token e não o salva em lugar nenhum, portanto
exporte-o você mesmo como `ANTHROPIC_OAUTH_TOKEN=<token>` (ou
`CLAUDE_CODE_OAUTH_TOKEN`) e defina `AI_MEMORY_LLM_PROVIDER=anthropic-oauth`. Nenhuma
`ANTHROPIC_API_KEY` é necessária. Os wrappers
do Docker encaminham qualquer um dos tokens pelo nome para comandos auxiliares de curta duração, como
`llm-test`; configure o contêiner do servidor de longa duração separadamente, conforme mostrado no
guia de instalação.

Para ambos os provedores da Anthropic, o ai-memory omite `temperature` para modelos Claude
4.7 e posteriores e Claude Mythos Preview porque esses modelos rejeitam
parâmetros de amostragem não padrão. O `llm-test` envia o mesmo valor representativo 0.2
que o pipeline normal antes que o provedor aplique essa regra de
compatibilidade.

**⚠️ Não oficial e contrário às políticas de uso da Anthropic. Use por sua conta e risco;
isso pode fazer com que sua conta sofra limitação de taxa (rate-limit) ou seja banida. Veja
[o aviso em `docs/install.md`](install.md#anthropic-via-claude-subscription-oauth).**

A Anthropic atualmente recusa tokens de assinatura provenientes de aplicativos de terceiros
([legal e conformidade](https://code.claude.com/docs/en/legal-and-compliance)),
portanto espere que `anthropic-oauth` falhe em cada requisição, normalmente com
`provider error 429` e `rate_limit_error`, embora o mesmo token funcione
na CLI do `claude`. Esse 429 é uma recusa de política, não um limite de taxa real. Use
o provedor `anthropic` com uma chave de API do Anthropic Console, ou outro provedor
da tabela acima.

`copilot` armazena um token de usuário do GitHub no mesmo arquivo de autenticação, troca-o por um
token de API de curta duração do Copilot via `/copilot_internal/v2/token` do GitHub, e
usa o endpoint do Copilot Chat com cabeçalhos de integração `vscode-chat`. Você também
pode definir `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` ou `GITHUB_TOKEN` no servidor.

> [!TIP]
> **Para os backends OAuth/assinatura, prefira um modelo pequeno e rápido** via
> `AI_MEMORY_LLM_MODEL` onde o backend permitir a escolha. O trabalho de LLM do ai-memory
> (consolidação, lint, exploração) consiste em sumarização, não em raciocínio complexo, logo um
> modelo da classe Haiku/mini é mais do que suficiente e muito mais suave nos limites de taxa
> da assinatura. Guarde os modelos de raciocínio de alto esforço para seu agente de código. Por backend:
> - `anthropic-oauth`: defina `claude-haiku-4-5`.
> - `openai-oauth` / `codex`: deixe o padrão do provedor (`gpt-5.5`). O
>   backend Codex/ChatGPT aceita apenas um conjunto reduzido de IDs de modelos definido pelo servidor
>   e rejeita outros (ex.: `gpt-5-mini`) com um 400 determinístico, portanto não
>   substitua o modelo aqui.
> - `copilot`: um id de classe mini como `gpt-5-mini` pode funcionar, mas o conjunto de modelos
>   aceitos pelo Copilot não é verificado. Verifique antes de confiar nele e volte
>   para o padrão caso o endpoint rejeite sua escolha.

> [!TIP]
> **A saída estruturada compatível com a OpenAI é restrita por esquema por padrão.**
> O ai-memory envia o JSON Schema de cada operação através de
> `response_format=json_schema`, o qual versões recentes do Ollama, vLLM, LM Studio e
> llama.cpp respeitam. Ele recorre ao analisador tolerante quando um
> endpoint rejeita explicitamente esse campo ou retorna um formato malformado. Defina
> `AI_MEMORY_LLM_COMPAT_STRICT=false` apenas para um endpoint incompatível.

O analisador tolerante remove invólucros `<think>`, `<thinking>`, `<analysis>` e
`<reasoning>` fora da resposta JSON e desempacota cercas Markdown.
Tags literais dentro de strings JSON permanecem intactas, para que resumos possam citar modelos de prompt
ou outro texto com tags. Respostas em texto estruturado da Anthropic usam as
mesmas regras de análise quando o provedor utiliza a escolha automática de ferramentas.

> [!TIP]
> **Motores locais capazes de raciocínio (vLLM / SGLang servindo modelos da
> classe Qwen3).** Tais motores gastam o orçamento de saída em uma etapa de raciocínio antes
> da carga útil estruturada e podem truncar o JSON no meio do objeto
> (`finish_reason = "length"`). Defina
> `AI_MEMORY_LLM_COMPAT_DISABLE_THINKING=true` para enviar
> `chat_template_kwargs: {"enable_thinking": false}` em cada requisição openai-compat
> para que o motor gaste o orçamento na carga útil. O parâmetro é
> opcional, apenas para openai-compat, e desativado por padrão: configurações existentes do vLLM / Ollama /
> LM Studio não são alteradas a menos que ele seja definido, e todos os outros provedores
> o ignoram. Quando o motor ainda assim truncar uma resposta estruturada, ou
> retornar HTTP 2xx com um `message.content` vazio, a chamada falha rapidamente
> com os erros terminais `truncated-response` / `empty-content`. Não há
> nova tentativa nem segunda chamada HTTP, e o texto de erro não transporta nenhum conteúdo
> de resposta.

Para modelos locais com contexto pequeno, configure ambos os limites de consolidação. O alvo
de entrada é dimensionado para o prompt renderizado completo, incluindo slot delimitado e
contexto da página atual mais o esquema de saída estruturada; o limite de saída é
enviado ao provedor. A soma deles deve caber na janela de contexto do modelo, com margem
adicional porque os tokenizadores dos provedores diferem:

```toml
[consolidation]
max_input_tokens = 10000
max_output_tokens = 1000
```

As variáveis de ambiente equivalentes são
`AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS` e
`AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS`. Falhas do provedor durante um
ponto de verificação automático PreCompact/PostCompaction recorrem à página determinística
baseada em regras; erros de admissão, armazenamento e escopo continuam falhando de forma fechada. Os
mínimos validados são 8.350 tokens de entrada e 1.000 tokens de saída no valor padrão de
`input_token_safety_margin` de 0.8; uma margem menor encolhe o orçamento de caracteres
e requer um alvo de entrada proporcionalmente maior.

Todo provedor de chat limita cada requisição de conclusão em 300 segundos
(`AI_MEMORY_LLM_TIMEOUT_SECS` para substituir, ou `llm_timeout_secs = 900` no
config.toml; a rápida atualização de token do openai-oauth mantém o teto padrão).
O padrão tolera que um motor local carregue a frio um modelo grande; gateways hospedados
lentos cujas conclusões longas excedem o teto falham em todas as requisições com
`http: error sending request`, portanto aumente o valor lá em vez de assistir à
consolidação esgotar suas tentativas.

O reranking é opcional e desativado por padrão. Com um provedor de LLM configurado,
`AI_MEMORY_RERANKER=llm` faz com que chamadas a `memory_query` de projeto e de escopo explícito
busquem a mais na etapa híbrida, fundam escopos e façam no máximo uma chamada de LLM
para reordenar os melhores candidatos. Isso pode promover uma página relevante que o
RRF classificou abaixo do corte solicitado, ao custo de latência e consumo de LLM. A
requisição envia a consulta mais no máximo 30 títulos de páginas e trechos de busca delimitados
ao provedor configurado; todos os valores são codificados em JSON e tratados como
dados não confiáveis. Um tempo limite esgotado, erro do provedor ou conjunto de pontuações incompleto/inválido
preserva a ordem normal. O tempo limite é de 20 segundos por consulta; aumente-o com
`AI_MEMORY_RERANK_TIMEOUT_SECS` (ou `rerank_timeout_secs` em `config.toml`)
quando um provedor lento ou uma cadeia de fallback precisar de mais tempo para responder. `global=true` e ocorrências
suplementares de preferências globais mantêm sua classificação existente sem RRF. Chamadas simultâneas ao provedor são limitadas
a quatro; consultas saturadas mantêm sua classificação local sem aguardar.

## Telemetria de carga de trabalho

`ai-memory status --json` (a resposta protegida por root de `GET /admin/status`)
inclui `providers.llm_workloads` sempre que um provedor de LLM estiver configurado. Seus
rótulos fechados são `unspecified`, `consolidation`, `rerank`, `auto-improve`,
`bootstrap`, `query-answer`, `lint`, `profile`, `dream` e `experience`.
Cada linha informa chamadas durante a vida útil do processo, sucessos e erros, latência
total/máxima, limites superiores de p50/p95 em intervalos fixos, último resultado e contagem de tokens. Os
contadores são locais ao processo e reiniciados na reinicialização; o servidor constrói o invólucro
do provedor configurado uma vez na inicialização e não tem recarregamento de provedor em tempo de execução.
`providers.llm_workloads_started_ms` fornece a hora Unix em milissegundos de quando
os contadores iniciaram (nulo quando nenhum provedor de LLM estiver configurado). Uma janela é a
diferença entre dois snapshots com o mesmo `llm_workloads_started_ms`. Uma
reinicialização, incluindo uma por mudança de configuração, inicia um novo valor, portanto
fecha um segmento, e uma janela que abrange reinicializações soma seus segmentos.

`estimated_input_tokens` e `estimated_output_tokens` usam uma estimativa deliberadamente
grosseira de quatro caracteres por token para que chamadas estruturadas e normais permaneçam
comparáveis. `reported_input_tokens` e `reported_output_tokens` contam apenas
respostas para as quais o provedor forneceu o consumo (usage); `usage_reported_calls` exibe
essa cobertura. Adaptadores de provedores de saída estruturada atualmente retornam JSON
decodificado sem dados de consumo, portanto seus contadores informados podem permanecer zerados mesmo quando a
estimativa não for zero.

A telemetria não armazena prompt, conclusão, escopo, projeto ou rótulo fornecido pelo
chamador. Uma cadeia de fallback é medida como uma invocação de interface de provedor, não
como uma chamada por tentativa de candidato interno; uma nova tentativa do chamador externo é outra
invocação. O rótulo da carga de trabalho em si nunca seleciona um provedor ou modelo. Uma
tarefa usa seu próprio provedor e cadeia de fallback apenas quando a configuração declarar um
perfil `[llm.<task>]` para ela (consulte `config.default.toml`); todas as outras tarefas
usam o provedor global e sua cadeia de fallback.

Embeddings são opcionais e separados do provedor de LLM. Defina
`AI_MEMORY_EMBEDDING_PROVIDER=openai`, `voyage`, `google`/`gemini`,
`openai-compat` ou `copilot` quando quiser recuperação vetorial além da
recuperação por FTS5 + entidade + vizinhos do grafo. `openai-compat` destina-se a motores
auto-hospedados (Ollama, LM Studio, vLLM): não requer chave de API e exige explicitamente
`AI_MEMORY_EMBEDDING_BASE_URL`, `AI_MEMORY_EMBEDDING_MODEL` e
`AI_MEMORY_EMBEDDING_DIM`. O opcional `EMBEDDING_API_KEY` credencia apenas
o embedder e é verificado antes de `OPENAI_API_KEY` e `LLM_API_KEY`, de modo que os
embeddings podem rodar em um provedor diferente do LLM. Como os dois
endpoints são independentes, `AI_MEMORY_LLM_BASE_URL` redireciona apenas o LLM;
defina `AI_MEMORY_EMBEDDING_BASE_URL` também ou o tráfego de embeddings continuará indo
para o endpoint padrão do provedor de embeddings. Tanto o caminho exclusivo por FTS
quanto o híbrido aplicam o mesmo ajuste delimitado de autoridade de página após a geração
de candidatos; os embeddings melhoram o recall de relevância, mas não decidem qual fonte
é canônica.

Modelos de embedding assimétricos, treinados com uma instrução diferente para
consultas e para o texto que está sendo indexado, precisam que essa instrução seja adicionada ao
próprio texto; o formato de rede de `/v1/embeddings` compatível com a OpenAI não possui campo
para isso. `AI_MEMORY_EMBEDDING_QUERY_PREFIX` e
`AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX` (aplicados apenas pelos embedders `openai` e
`openai-compat`) são prefixados aos textos da consulta e do documento,
respectivamente, antes do truncamento existente, de modo que um corpo longo continue sendo
truncado no mesmo limite geral de entrada com o prefixo incluído. Ambos ficam
vazios por padrão (nenhuma mudança de comportamento quando não definidos) e não sofrem trim, de modo
que o espaço final definido pelo publicador é preservado com exatidão. Por exemplo, o
[`nvidia/Nemotron-3-Embed-1B-BF16`](https://huggingface.co/nvidia/Nemotron-3-Embed-1B-BF16) da NVIDIA
(2048-dim, servido localmente via vLLM/`openai-compat`) especifica
`"query: "` para consultas e `"passage: "` para documentos segundo seu cartão de modelo:

```toml
embedding_provider = "openai-compat"
embedding_model = "nvidia/Nemotron-3-Embed-1B-BF16"
embedding_base_url = "http://localhost:8000/v1"
embedding_dim = 2048
embedding_query_prefix = "query: "
embedding_document_prefix = "passage: "
```

Modelos E5 base (`intfloat/e5-base-v2`, `e5-large-v2`, E5 multilíngue, …)
usam a mesma convenção `"query: "` / `"passage: "`. Variantes E5 ajustadas por instrução
e Qwen3-Embedding precisam, em vez disso, de uma string de instrução de tarefa **apenas
no lado da consulta** (seus documentos são incorporados de forma simples, sem prefixo
de documento), e os dois usam **espaçamentos exatos diferentes**, confirmados em
cada cartão de modelo:

- `intfloat/e5-mistral-7b-instruct`:
  `embedding_query_prefix = "Instruct: {task description}\nQuery: "`, com um
  espaço final após `Query:`.
- `Qwen/Qwen3-Embedding-0.6B` (e os outros tamanhos do Qwen3-Embedding):
  `embedding_query_prefix = "Instruct: {task description}\nQuery:"`, com **nenhum**
  espaço final; o texto da consulta segue os dois-pontos diretamente.

Preencha sua própria descrição de tarefa em `{task description}`, deixe
`embedding_document_prefix` indefinido para ambos e não copie a string exata
de um modelo para o outro. A diferença no espaço final foi
especificada pelo publicador, não é um erro de digitação.

Alterar apenas `embedding_query_prefix` nunca exige novo embedding: apenas
`embedding_document_prefix` é incorporado à identidade armazenada do embedding
(um valor não vazio altera o que o `ai-memory` trata como a trinca atual
`{provider, model, dim}` para os vetores daquela página), de modo que uma alteração
no prefixo de documento faz com que `memory_query` deixe de coincidir com os vetores antigos e a
próxima passagem de `ai-memory embed` (agendada ou manual) os gere novamente
automaticamente, sem `--force`. `--force` continua útil para
gerar novamente todos os embeddings imediatamente em vez de esperar pelo preenchimento retroativo agendado,
ou para forçar uma nova geração por uma razão que a trinca isoladamente não
captura (por exemplo, ajustes no tokenizador ou quantização do servidor vLLM).

`AI_MEMORY_EMBEDDING_PROVIDER=copilot` reutiliza o mesmo login Copilot OAuth que
o provedor de LLM `copilot` (`ai-memory auth login copilot` ou
`COPILOT_GITHUB_TOKEN`/`GITHUB_COPILOT_API_TOKEN`), sem nenhuma chave de API separada.
Seu padrão é o modelo `text-embedding-3-small`, dim 1536, e chama o endpoint
`/embeddings` do Copilot seguindo o mesmo contrato compatível com a OpenAI que o Copilot
documenta para chat. O formato exato desse endpoint não é coberto por um teste
ativo contra o Copilot aqui; trate-o como necessitando de um teste de fumaça com o Copilot real
antes de confiar nele em produção.

`AI_MEMORY_EMBEDDING_PROVIDER=local` não necessita de chave e de nenhum servidor:
embeddings de sentenças rodam no próprio processo (Rust puro `all-MiniLM-L6-v2`,
384-dim), com o modelo baixado uma vez em `<data_dir>/models/` sob
checksums fixos; consulte [`docs/local-embeddings.md`](local-embeddings.md).

Consulte [`docs/install.md#llm-provider-tiers`](install.md#níveis-de-provedor-llm)
para variáveis de ambiente e exemplos de Ollama/OpenRouter/Atlas Cloud/OrcaRouter/Cheaper Inference/API Route/FutureInfra,
e
[`docs/llm-provider-comparison.md`](llm-provider-comparison.md)
para a comparação empírica de modelos.
