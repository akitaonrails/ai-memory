# Livro de receitas (cookbook) do ai-memory

Uma folha de dicas orientada a tarefas: "Eu quero fazer X" → como fazer. Para a referência completa, consulte
[`ARCHITECTURE.md`](ARCHITECTURE.md); para instalação, consulte [`install.md`](install.md)
(incluindo o [aplicativo da barra de menus do macOS](install.md#aplicativo-da-barra-de-menus-do-macos));
para a tabela de roteamento de ferramentas, consulte [`usage.md`](usage.md).

## O que é o ai-memory, em um parágrafo

O ai-memory dá aos seus agentes de código memória de longo prazo entre sessões. Ele roda
como um servidor único; seu agente fala com ele via MCP (ferramentas como `memory_query`,
`memory_write_page`) e via hooks de ciclo de vida que capturam automaticamente o que você
faz. A memória é uma **wiki em markdown no git** (a fonte da verdade, editável à mão) mais
um índice SQLite derivado para busca. Tudo tem **escopo por projeto**
`(workspace, project)`, resolvido a partir de marcadores/rotas de home ou, por padrão, do
caminho normalizado do repositório (`upstream`, depois `origin`; nome base da pasta apenas
sem um remote válido). Funciona sem nenhum LLM (captura + busca de texto completo + resumos baseados em regras); adicionar um provedor
habilita consolidação e autoaprimoramento.

## Receita: usar o ai-memory como a memória da sua ferramenta

Chame as ferramentas MCP existentes para salvar páginas, consultar conhecimento e passar um handoff
entre execuções. Hooks nativos são opcionais. O [guia de memória programática](programmatic-memory.md)
inclui requisições HTTP completas, regras de escopo e autenticação de máquina.
Se sua ferramenta também hospeda um harness, siga o [contrato de ciclo de vida externo](external-lifecycle.md).

## Tarefas do dia a dia (através do seu agente, via MCP)

Na maior parte do tempo você só conversa com seu agente; ele chama a ferramenta correta. Casos comuns:

| Você quer | Diga ao seu agente | Ferramenta que ele usa |
|---|---|---|
| Relembrar trabalho anterior | "Já discutimos X?" / "o que decidimos sobre Y?" | `memory_query` |
| Atualizar-se após um tempo longe | "Onde paramos?" / "atualize-me" | bloco de handoff / `memory_explore` |
| Lembrar um fato durável | "Lembre-se de que aqui sempre usamos pnpm" | `memory_write_page` |
| Lembrar até uma data | "Lembre-se disso até a migração ser lançada" | `memory_write_page` + `expires_at` |
| Uma regra permanente para *todo* projeto | "Sempre: nunca use force-push" | `memory_write_page` com `scope: "global"` |
| Ler uma página específica salva na íntegra | "Leia a página `_rules/deploy`" | `memory_read_page` |
| Salvar contexto para a próxima sessão | "Vamos encerrar — salve o contexto para a próxima vez" | `memory_handoff_begin` |
| Ver estatísticas do projeto | "Quanta memória nós temos?" | `memory_status` / `memory_briefing` |

Você raramente escreve páginas à mão: hooks de ciclo de vida capturam prompts e chamadas de ferramentas
automaticamente e (com um LLM) a consolidação os compila em páginas.

## Receita: manter uma regra que um projeto deve seguir

> "Lembre-se: antes de mexer no código de pagamento, leia as notas de PCI."

Seu agente chama `memory_write_page` e isso se torna uma página de wiki durável (roteada
sob `_rules/` quando for uma regra). Na próxima sessão, `memory_query` a trará à tona, e
se o `.ai-memory.toml` do projeto habilitar o resumo de inicialização, as regras são
prefixadas ao contexto do agente automaticamente: um trecho de cada regra primeiro,
corpos inteiros enquanto o orçamento permitir, e `memory_read_page` para o restante. Fixe
a regra para que seja exibida na íntegra antes das outras. Para fazê-la valer para **todos** os seus
projetos, peça isso como uma preferência permanente. Ela vai para o seu perfil interprojetos
(veja a próxima receita) ou para o escopo compartilhado `_global` com
`scope: "global"`. Em um servidor
multiusuário, gravar uma regra global exige root ou permissão de `write` em `_global`. A partir
da página de outro projeto, crie um link para essa página permanente com `[[_global:path]]`, que
sempre aponta para o projeto reservado `_global` no espaço de trabalho padrão. Projetos irmãos
usam `[[project:path]]`; outro espaço de trabalho usa
`[[workspace/project:path]]`. Um simples `[[name]]` permanece dentro do projeto atual.

## Receita: evitar que o agente repita algo que você rejeitou

> "Não, não use mock do banco de dados neste teste de integração. No mês passado um mock escondeu
> uma migração quebrada; use o Postgres de teste compartilhado."

Com um provedor de LLM configurado, você não precisa pedir para que isso seja
lembrado. Após a sessão terminar, o revisor de autoaprimoramento identifica
uma correção acompanhada de uma justificativa, qualquer que seja o idioma em que você a escreveu.
Ele propõe uma página em `_rules/`, ou uma correção para a que já existe sobre esse tópico,
informando o que evitar, por que e o que fazer em vez disso, citando suas palavras como
evidência. A proposta passa pela política de aprovação de `[auto_improve]`. Um simples
"não" ou uma renomeação pontual não vira regra. Detalhes e limites estão
em [`auto-improvement-loop.md`](auto-improvement-loop.md#correções-de-usuários).
Se o agente ainda repetir o erro mesmo com a regra em vigor, transforme a regra
em um hook ou em um teste.

## Receita: notar quando o código de uma página foi alterado

> "Lembre-se desta gotcha de deploy; ela diz respeito a `bin/deploy.sh`."

Seu agente pode passar `source_files` para `memory_write_page`: os arquivos
do repositório que a página descreve, cada um com o commit em relação ao qual foi verificado
(`[{"path": "bin/deploy.sh", "commit": "3f2a9c1"}]`, até 16). O servidor
armazena a lista no frontmatter da página e nunca abre os arquivos, portanto funciona
da mesma forma em um servidor remoto ou em contêiner. Para verificar uma página, leia-a
e execute este comando a partir do checkout para cada entrada:

```bash
git --literal-pathspecs diff --quiet 3f2a9c1 -- bin/deploy.sh
```

Uma saída diferente de zero, ou um caminho que não exista mais, significa que o arquivo mudou:
registre `memory_feedback` com `signal: "stale"` e um motivo como
`source-changed: bin/deploy.sh since 3f2a9c1`. A página cai na classificação e aparece
como uma ocorrência de `feedback_flagged` no `memory_lint`. Reescrever a página com
o novo commit limpa a sinalização.

## Receita: parar de reexplicar seus hábitos a cada novo projeto

> "Em todos os meus projetos: use pnpm, mantenha testes de integração em tests/suite."

Seu agente grava cada um deles no perfil interprojetos
(`memory_write_page` com `scope: "profile"`). A partir de então, cada projeto, em
cada harness, recebe uma breve seção "suas escolhas habituais" no início da sessão, e
o agente aplica essas escolhas sempre que o arquivo de regras do repositório e você não
disserem nada. Um repositório novinho em folha recebe todo o baseline, mais um ponteiro para
`ai-memory profile apply` para gravá-lo no arquivo de regras desse repositório.

- Você não precisa pedir: hábitos que você expressa nos prompts ("sempre…", "prefira
  X em vez de Y", "a partir de agora…") são aprendidos por conta própria assim que você os disser como
  regra geral ou em dois projetos. `ai-memory profile review` mostra o que foi
  aprendido, a partir de quais palavras, e o que ainda falta um projeto para consolidar.
- Veja o que ele contém: `ai-memory profile list`; descarte um item: `ai-memory profile
  forget tools/pnpm.md` (ele permanece descartado até você dizê-lo novamente).
- Deixe um projeto de cliente de fora: `[profile] contribute = false` no `.ai-memory.toml`
  desse repositório; mantenha o compêndio (digest) fora de um projeto: `consume = false`.
- Restrinja uma entrada a uma stack com `applies_to: ["rust"]` em `memory_write_page`
  (`--applies-to rust` em `write-page`), ou marque-a com `enforced_by` para mantê-la
  fora do compêndio.
- Ativado por padrão para usuário único (um perfil abrangendo todos os espaços de trabalho). Em um
  servidor compartilhado, fica desativado até o operador definir `[profile] enabled = true`,
  e então cada pessoa ganha um perfil privado.

Detalhes: [`cross-project-profile.md`](cross-project-profile.md).

## Receita: fazer um projeto ler um documento específico antes de implementar

Duas maneiras, dependendo de onde o documento reside:

1. **Já é uma página de wiki** (você a salvou ou importou como descrito abaixo): diga ao
   agente "leia a página `norms/gdpr-retention` antes de implementar" e ele chama
   `memory_read_page` com esse caminho e trabalha a partir do corpo completo.
2. **É um arquivo externo** (uma norma/especificação no disco): salve-o como uma página primeiro
   ("lembre-se desta especificação como `norms/gdpr-retention`", cole ou aponte para ele) e então
   faça referência a ele como em (1). Referências grandes são melhor divididas em uma página por
   documento para que a recuperação possa trazer apenas o relevante.

## Receita: importar uma base de conhecimento existente (ex.: normas/leis/especificações OKF)

A wiki do ai-memory **é** OKF (markdown compatível com Obsidian + frontmatter). Para
trazer um conjunto de documentos existente como memória de projeto:

- **Poucos documentos**: salve cada um como uma página durável através do seu agente
  (`memory_write_page`) ou da CLI (`ai-memory write-page`), uma página por
  documento, sob um prefixo estável como `norms/…`. Depois, qualquer projeto pode ler uma
  específica antes de implementar (receita acima).
- **Um vault OKF/Obsidian inteiro ou uma exportação de outra ferramenta**: use o
  importador companion (veja [`companion-crates.md`](companion-crates.md)). Ele
  ingere exportações de OMC e de conversas externas no armazenamento. `ai-memory
  export-okf` é o inverso (exporta sua wiki como um pacote OKF), útil para ver
  o formato exato em disco que seus documentos devem assumir.
- A wiki fica em `<data_dir>/wiki/<workspace>/<project>/…` e é puro
  markdown no git, portanto você também pode soltar arquivos lá dentro e deixar o monitor de arquivos +
  reindexador detectá-los. Mantenha-os sob um prefixo claro e faça o commit.

Uma vez que o material esteja salvo como páginas no escopo correto, qualquer projeto pode buscá-lo
(`memory_query`) e ler um documento específico na íntegra (`memory_read_page`)
antes de implementar com base nele.

## Receita: fazer o bootstrap de muitos repositórios de uma só vez

`ai-memory bootstrap` semeia um projeto a partir de seu histórico git, README e documentações. Execute-o
a partir de vários terminais (ou em um loop com `&`) para semear muitos repositórios; o servidor
decide quantos chamam o LLM por vez com `bootstrap_max_concurrent`
(padrão 1). Execuções para o mesmo projeto sempre esperam umas pelas outras, e blocos
dentro de uma execução continuam sequenciais, de forma que o maior repositório define o piso.

- **GPU local (vLLM, servidor llama.cpp):** o servidor agrupa em lote requisições concorrentes,
  portanto uma única execução o deixa quase ocioso. Aumente o limite até que o
  motor comece a enfileirar (métrica `num_requests_waiting` do vLLM) ou seu cache
  KV fique cheio; 4 a 8 por GPU é um bom ponto de partida.
- **API hospedada (DeepSeek, OpenAI, …):** cada chamada é limitada pela latência, então o
  ganho de velocidade é quase linear até o limite de taxa ou concorrência da sua conta.
  Defina o limite abaixo dessa cota.
- **API hospedada com fallback:** adicione uma entrada `[[llm_fallbacks]]`. Quando a
  primária responder `429`, seu circuito abre pelo tempo do `Retry-After` do provedor e
  as execuções restantes vão para o próximo candidato em vez de falharem.

```toml
llm_provider = "openai-compat"
llm_model = "deepseek-chat"
llm_base_url = "https://api.deepseek.com/v1"   # key from the LLM_API_KEY env var
bootstrap_max_concurrent = 8

# Takes the overflow while the primary is rate-limited.
[[llm_fallbacks]]
provider = "openai-compat"
model = "<another-model>"
base_url = "<another OpenAI-compatible endpoint>/v1"
api_key_env = "FALLBACK_API_KEY"
```

## Vindo do agentmemory

Os dados migram com o importador companion
([`ai-memory-importer`](../../companions/ai-memory-importer), limite em
[`companion-crates.md`](companion-crates.md)). A maior parte do trabalho consiste em reconectar
cada harness. Faça isso nesta ordem:

1. **Exporte do agentmemory enquanto ele ainda roda.** Seu comando de exportação grava
   um único arquivo JSON `ExportData`; sua exportação para o Obsidian grava um diretório de vault.
   O JSON é o caminho completo (versões de memória, TTLs, sessões,
   resumos, lições, insights). O importador lê apenas esse arquivo e nunca
   entra em contato com o agentmemory.
2. **Pare o agentmemory e desconecte-o.** Remova a entrada do seu servidor MCP, os
   plugins e skills que ele instalou para cada harness, e o bloco de diretrizes de memória
   que o comando `connect` grava nos arquivos de regras (`AGENTS.md`,
   `GEMINI.md`, `.cursor/rules/`, `.kiro/steering/` e similares). Deixado no
   lugar, esse bloco faz os agentes continuarem chamando ferramentas que não existem mais, e uma
   delas, `memory_consolidate`, também é uma ferramenta do ai-memory com argumentos
   diferentes. `ai-memory doctor` lista todos os vestígios que consegue ver, com o arquivo,
   linhas e a correção, sem alterar nada:

   ```text
   ⚠ agentmemory configuration is still present (2 place(s)). ...
     - MCP server entry in /home/me/.cursor/mcp.json (mcpServers.agentmemory)
         fix: remove the `mcpServers.agentmemory` entry from /home/me/.cursor/mcp.json; ...
     - guideline block in /home/me/work/api/AGENTS.md (lines 5-10)
         fix: delete the agentmemory guideline block (lines 5-10) from ...
   ```

   Regras mantidas fora de arquivos (as do Warp, por exemplo) precisam de inspeção manual.
3. **Conecte o ai-memory em cada harness:** `ai-memory install-hooks --agent <name>
   --apply`, `ai-memory install-mcp --client <name> --apply` e `ai-memory
   install-skills --agent <name>`.
4. **Mapeie projetos, faça dry-run e depois importe.** O agentmemory nomeia um projeto pelo
   diretório base, portanto o importador necessita de um arquivo de mapeamento para o
   `workspace`/`project` do ai-memory (pelo nome base, ou pelo `cwd` exato da sessão quando dois
   repositórios compartilham o mesmo nome). O dry-run lista cada projeto não mapeado e
   cada tipo de registro que deixa de fora, com contagens; `--apply` recusa-se a executar
   enquanto houver algo não mapeado:

   ```bash
   cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
     agentmemory-export --file agentmemory-export.json --mapping mapping.json
   cargo run --manifest-path companions/ai-memory-importer/Cargo.toml -- \
     agentmemory-export --file agentmemory-export.json --mapping mapping.json \
     --apply --manifest-out agentmemory-manifest.json
   ```

   Páginas vão para `imported/agentmemory/` no projeto mapeado. As versões de uma
   memória tornam-se uma página com seu histórico de substituição, `forgetAfter`
   torna-se `expires_at`, e observações são descartadas a menos que você passe
   `--replay-observations`. Reexecutar é seguro: páginas já importadas são
   ignoradas. Para um vault, use `agentmemory-vault --dir <vault> --workspace W
   --project P` como alternativa.
5. **Verifique.** Pergunte ao seu agente algo que uma memória antiga respondia, ou chame
   `memory_query` para um título conhecido em cada projeto, e execute `ai-memory
   doctor` novamente até que ele não aponte nenhum vestígio.

O que não possui equivalente no ai-memory (nós e arestas do grafo, ações, sinais,
rotinas, sentinelas, rascunhos, checkpoints) é contabilizado na seção `dropped` do
manifesto e não é importado. O ai-memory deriva seu próprio grafo de entidades a partir
das páginas importadas.

## Receita: espelhar uma wiki de equipe no repositório (sincronização de wiki de equipe)

Para manter a memória compartilhada de uma equipe revisável junto ao código que ela documenta, use
o companion [`ai-memory-wikisync`](../../companions/ai-memory-wikisync)
(limite em [`companion-crates.md`](companion-crates.md)). Ele exporta
famílias de páginas explicitamente permitidas da interface `/api/v1` somente leitura do servidor
para um diretório no seu repositório. Ele opera em dry-run por padrão:

```bash
ai-memory-wikisync plan   --server http://127.0.0.1:49374 \
    --workspace demo --project app --dest ./wiki \
    --include _rules --include decisions     # lists create/update/unchanged; never writes
ai-memory-wikisync export ...same args... --apply   # writes; prints the git commands to run
```

Tudo é opcional e opera em modo fail-safe: a lista de famílias permitidas deve ser explícita
(`*` é recusado), arquivos editados localmente desde a última exportação são informados
com um resumo do diff e recusados sem `--force`, nenhum frontmatter é forjado
e a ferramenta nunca executa o git por conta própria. `sync` envia edições revisadas do repositório
de volta via MCP (cada gravação condicionada à versão com base na qual planejou;
necessita do ai-memory 3.0+), e exclusões só trafegam com `--propagate-deletes`.

### Mantenha atualizado após merges e no CI

`install-hook` grava um hook `post-merge` do git que executa `sync` após cada
merge ou pull: um relatório dry-run por padrão, ou uma gravação com `--on-merge apply`. Ele
nunca armazena um token (o hook lê `AI_MEMORY_AUTH_TOKEN` quando executa) e
nunca passa `--prefer`, portanto conflitos continuam aguardando por uma pessoa:

```bash
ai-memory-wikisync install-hook --workspace demo --project app \
    --dest docs/wiki --include _rules --include decisions
ai-memory-wikisync uninstall-hook --dest docs/wiki   # removes only its block
```

No CI, `sync --check` não grava nada e encerra com `0` se sincronizado, `3` quando o
repositório e o servidor diferirem, e `4` em conflitos ou recusas. Um checkout
de CI não tem estado de sincronização, portanto compara o repositório com o servidor
diretamente. Um job do GitHub Actions que sinaliza divergências em pull requests da wiki:

```yaml
name: team-wiki
on:
  pull_request:
    paths: ["docs/wiki/**"]
jobs:
  wiki-drift:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: cargo install --locked --git https://github.com/akitaonrails/ai-memory ai-memory-wikisync
      - name: Compare docs/wiki with the team's ai-memory server
        env:
          AI_MEMORY_SERVER_URL: ${{ secrets.AI_MEMORY_SERVER_URL }}
          AI_MEMORY_AUTH_TOKEN: ${{ secrets.AI_MEMORY_AUTH_TOKEN }}
        run: |
          ai-memory-wikisync sync --check \
            --workspace demo --project app --dest docs/wiki \
            --include _rules --include decisions
```

O servidor deve ser acessível a partir do executor (`serve --enable-api` protegido por um
token). Para tolerar divergências e falhar apenas em caso de recusas, finalize o comando com
`|| [ $? -eq 3 ]`.

## Receita: controlar o que é mantido, envelhecido ou consolidado

Por padrão, não há nada com que se preocupar: a memória decai em uma curva suave
única, e uma atualização para a versão 2.4 não altera pontuações nem remove nada. Quando você *quiser*
ajustar o envelhecimento, tudo é opcional e reversível. O original de qualquer conteúdo
compactado ou mesclado permanece no git e na cadeia de substituição, recuperável com
`ai-memory restore-page`.

- **Manter algo para sempre:** fixe-o. Uma página fixada fica isenta da varredura de
  esquecimento, independentemente do nível ou idade. Páginas semânticas e procedimentais também nunca
  decaem; apenas a memória operacional/episódica envelhece.
- **Fazer uma nota expirar em um prazo:** peça ao seu agente para lembrá-la "até
  <data>" (um `expires_at`); a varredura de esquecimento a exclui quando o tempo passar,
  não importando quantas vezes foi lida. Um TTL tem precedência sobre fixação.
- **Ajustar a duração de cada nível:** defina meias-vidas por nível no `.ai-memory.toml` do
  projeto em `[decay.half_life_days]` (ex.: manter o histórico episódico por mais tempo,
  rascunhos operacionais por menos tempo). Omita-o para a curva única padrão.
- **Compactar em vez de despejar e desduplicar:** `[decay] compact_cold_episodic`
  mantém os fatos duráveis de uma página fria (caminhos, códigos de erro, decisões) e descarta a
  prosa; `[decay] dedup_cold_clusters` agrupa páginas frias quase duplicadas em
  uma sobrevivente. Ambos funcionam sem LLM, desativados por padrão, e substituem em vez de
  excluir.
- **Manter memória usada por mais tempo:** nada a configurar. Uma página que você abre, pesquisa
  ou alcança através de navegação por páginas relacionadas é reforçada automaticamente e resiste
  ao decaimento.
- **Revisar conhecimento similar:** execute `memory_lint` (através do seu agente ou da
  CLI); com embeddings configurados, ele emite alertas de `similarity_candidate`
  para pares de páginas do mesmo tópico, apenas em caráter consultivo. A similaridade identifica tanto vocabulário compartilhado
  quanto desacordo, portanto leia cada candidato como um par a verificar, não como uma
  contradição ou instrução de substituição. Relações explícitas de `contradicts`
  permanecem como ocorrências de `contradiction`. Se
  esse ruído for muito alto, eleve `contradiction_band_min` (`config.toml` ou
  `AI_MEMORY_CONTRADICTION_BAND_MIN`, padrão `0.4`). Em um repositório desse tipo, a
  faixa mede mais a proximidade do domínio do que conflito; portanto, um piso mais alto apara
  pares do mesmo domínio, mas não relacionados.
- **Permitir que um LLM consolide na ociosidade ("dream"):** com um provedor *e* um embedder
  configurados, `[dream] enabled` ativa uma etapa em segundo plano que reescreve agrupamentos
  de notas frias em páginas coerentes únicas enquanto você estiver ocioso e cancela no
  instante em que você retornar. Fica desativado por padrão, nunca exclui uma fonte e é condicionado a
  uma avaliação interna de recall antes que pudesse se tornar o comportamento padrão.

## Receita: sessões longas em uma janela de contexto pequena

Um modelo local com janela de contexto em torno de 200k tokens, ou qualquer sessão de longo
horizonte, compacta repetidas vezes. Cada compactação mantém um resumo com perdas, e
a primeira coisa que ela perde é o início da sessão: a tarefa que você deu, suas
restrições, as primeiras decisões. O ai-memory tem os prompts que você realmente
digitou, para que o agente possa se reancorar neles.

- No Claude Code, Codex, Factory Droid e OpenCode V1/V2, após cada
  compactação os hooks injetam uma pequena âncora pós-compactação (cerca de 2.000
  caracteres por padrão) com os primeiros e os últimos prompts da sessão, e as
  instruções instaladas dizem ao agente para confiar nela em vez do resumo.
- No Gemini CLI, Cursor, Copilot CLI, Kimi Code, Grok, Kiro, Cline, Muse e
  nos demais, nada é injetado: o bloco de roteamento e a skill de handoff instruem
  o agente a ler os prompts de sua própria sessão com
  `memory_read_session_observations` após uma compactação. Instale o roteamento
  gerenciado (`ai-memory install-instructions`, ou peça "instale o roteamento
  do ai-memory") para que toda sessão receba essa regra.
- Onde quer que os hooks respondam a um prompt, você pode iniciar um com
  `reload context` para recuperar o trecho que a última compactação descartou, por exemplo
  `reload context, then finish the migration`.

A configuração `compact_prompt` do Codex substitui todo o prompt de compactação do Codex e
aplica-se apenas a provedores locais, de modo que não pode adicionar uma única linha da forma que o hook
do OpenCode faz. Altere-a apenas se pretender reescrever a forma como o Codex resume. Consulte
[`usage.md`](usage.md#recarregar-o-contexto-após-uma-compactação) para os detalhes
específicos por harness.

## Receita: dois agentes / dois repositórios trabalhando juntos

- **Continuidade entre agentes no mesmo projeto** (sair do Claude Code, abrir o Codex
  no mesmo repositório): automática. Um handoff é capturado no fim da sessão e o
  hook de inicialização da próxima sessão o prefixa. Pergunte "onde paramos?".
- **Pedir a um agente em outro projeto para fazer algo** sem carregar o contexto daquele
  projeto aqui: use a troca de mensagens entre projetos. Diga "envie para o project-b uma solicitação para
  adicionar o endpoint de exportação" (`memory_message_send`), e lá "verifique minha
  caixa de entrada" (`memory_message_pop`). Consulte [`agent-messaging.md`](agent-messaging.md).

## Receita: pedir para um operador decidir

Quando um orquestrador precisa parar para consultar uma pessoa, envie uma mensagem entre projetos normal
e faça sondagem por uma resposta normal. As linhas de assunto são a convenção
compartilhada; `memory_message_send` e `memory_message_list` são as ferramentas.

1. Envie para o projeto em que o operador lê as mensagens. O `subject` é
   `decision: <short question>`. O corpo informa a pergunta, o `id` e o rótulo
   de cada opção e `reply_project: <your workspace>/<your project>`.
   Guarde o `message_id` retornado. Você precisa de permissão de escrita no projeto dessa caixa de correio,
   e ele já deve existir.
2. O operador, ou um companion, responde com `memory_message_send` de volta para
   `reply_project`. O `subject` é `decision-reply: <message_id>`. O corpo
   transporta `option: <id>`. O envio da resposta precisa de permissão de escrita no seu projeto.
3. Faça sondagem com `memory_message_list` na sua caixa de entrada até que o assunto de uma linha pendente seja
   exatamente aquela linha `decision-reply:`. A correspondência mais antiga é a resposta. Retire-a da fila
   quando aceitá-la e então cancele a pergunta com `memory_message_cancel`.

Antes de agir com base em uma decisão com `requires_root: true`, compare `from_owner_user`
na resposta com a identidade de root com a qual você foi configurado. Trata-se de
atribuição, não de uma função, e fica vazio em servidores anônimos e de operador
único; trate um valor vazio como não sendo root. O servidor em si não verifica
quem responde. `/web` mostra a caixa de correio e mantém cada mensagem pendente.

A listagem é por ordem de antiguidade (mais antiga primeiro), 50 linhas por padrão e 200 no máximo, portanto mantenha
a caixa de entrada de onde faz a sondagem esvaziada. Um companion pode despertar pelo webhook observador
de `message_send` e depois listar; o webhook transporta escopo e ator, e o
orquestrador continua sondando.

Estrutura completa e limites:
[`agent-messaging.md`](agent-messaging.md#decisões-do-operador-por-assunto-e-corpo).

## Receita: várias contas ou um inicializador externo

Dê a cada conta ou provedor seu próprio diretório de configuração e passe-o com `--env`, para
que o harness, seus hooks e MCP, e a importação de transcrições usem essa home única:

```bash
ai-memory run --env CLAUDE_CONFIG_DIR="$HOME/.claude-work" claude
ai-memory run --env CODEX_HOME="$HOME/.codex-work" codex   # the dir must exist
```

- `--env` e `--env-file` pertencem ao `ai-memory run`, portanto vão antes do
  nome do harness.
- Um inicializador ou orquestrador que inicie harnesses com seu próprio ambiente
  (chaves de provedores, diretório de configuração de uma conta) pode gravá-lo em um arquivo e passar
  `--env-file <path>`, uma chave `KEY=VALUE` por linha. Os valores são considerados literalmente, portanto
  use caminhos absolutos no arquivo.
- Para nomear uma conta de uma vez em vez de repetir `--env`, adicione um perfil ao
  `config.toml` e selecione-o com `--profile` (antes do nome do harness):

  ```toml
  [run.profiles.work.env]
  CLAUDE_CONFIG_DIR = "/home/me/.claude-work"
  ```

  Depois execute `ai-memory run --profile work claude`. Use caminhos absolutos: valores
  de perfil não sofrem expansão. `--env-file` e `--env` ainda substituem uma entrada
  de perfil, e um nome desconhecido falha antes que qualquer coisa seja configurada ou iniciada. Um
  `--profile` após o nome do harness pertence ao harness (OMP e Codex
  têm seus próprios).
- A conexão automática roda uma vez por diretório de configuração, de modo que a segunda conta recebe seus hooks +
  MCP em sua primeira inicialização própria. Para conectar uma manualmente, exporte a variável para os
  instaladores: `CLAUDE_CONFIG_DIR="$HOME/.claude-work" ai-memory install-hooks
  --agent claude-code --apply`, e em seguida o mesmo para `install-mcp --client
  claude-code --apply`.

Consulte [`managed-workstreams.md`](managed-workstreams.md).

## Receita: executar um agente não supervisionado com segurança (`--yolo` + ai-jail)

`--yolo` mapeia-se para a flag de modo perigoso de cada harness (Claude Code →
`--dangerously-skip-permissions`), que executa cada chamada de ferramenta sem
nenhuma confirmação. `ai-memory run --yolo` adiciona barreiras de proteção ao redor disso, condicionadas
inteiramente a um terminal interativo real (stdin e stderr ambos TTYs), de modo que
scripts, hooks e CI nunca são interrompidos com perguntas:

```bash
ai-memory run --yolo claude
```

- **O aviso.** Antes de o agente ser gerado, você é convidado a confirmar:
  `Enter`/`y`/`yes` prossegue (o padrão); `n`/`no` aborta antes de qualquer coisa
  ser iniciada.
- **A oferta do ai-jail.** Se o [ai-jail](https://github.com/akitaonrails/ai-jail)
  for utilizável (no Linux/macOS, instalado no `PATH` ou em `~/.local/bin/ai-jail`,
  com o backend de sandbox presente: `bwrap` no Linux, `sandbox-exec` no
  macOS) e você não estiver dentro dele, uma segunda pergunta oferece para
  executar a sessão dentro dele. Quando não for utilizável (ou no Windows), não há
  uma segunda pergunta; a execução simplesmente prossegue. Aceitar executa novamente o comando original
  sob `ai-jail --network --agent-state --env <NAME>... --`,
  encaminhando apenas as variáveis de ambiente de
  credenciais/configuração que já estiverem definidas (URL do servidor/hook,
  `CLAUDE_CONFIG_DIR`, chaves de API de provedores, etc.). `--network` mantém o
  servidor ai-memory de loopback acessível enquanto isola o sistema de arquivos.
  Recusar mantém a execução sem sandbox (sua escolha, já avisado).
- **Escolher o que a jail recebe.** Depois de aceitar a oferta, uma lista de verificação
  mostra o que este host pode montar, pré-marcado para que o `Enter` tome a atitude
  mais prática: cada credencial existente (`~/.config/gh`, `~/.aws`, `~/.kube`,
  `~/.config/gcloud`, `~/.docker/config.json`), chaves SSH + agente quando o seu
  `origin` for um remote SSH, e metadados de worktree em um worktree vinculado. O
  socket do Docker (concede root no host), GPU, display, Imagens e Tailscale são
  listados desmarcados. Digite números de linhas para alterná-los (`2 4`), ou `all` / `none`.
  Uma credencial montada pode ser usada pelo agente não supervisionado, portanto desmarque o que ele
  não deve tocar. O que você vê é o que você obtém: linhas desmarcadas são passadas
  como `--no-X`, portanto permanecem desligadas mesmo se o seu `~/.ai-jail` global as habilitar.
- **Suas respostas voltam da próxima vez.** A lista de verificação se lembra do que você
  escolheu para cada checkout, de modo que a próxima inicialização traz as mesmas marcas e o Enter
  é suficiente. Cada linha pré-marcada explica por que é recomendada (`found on this
  machine`, `origin pushes over SSH`), e o cabeçalho informa quando suas respostas
  foram salvas e o que mudou desde então (`new: …`, `no longer offered: …`). Digite
  `reset` para voltar às opções recomendadas. As respostas são armazenadas no
  diretório de dados do ai-memory, nunca no seu repositório.
- **Permitir que o agente leia um projeto irmão.** Pressione `f` na lista de verificação para
  escolher pastas extras. O seletor começa na pasta acima do seu checkout:
  digite para filtrar, Espaço para selecionar (somente leitura), Ctrl+W para tornar a selecionada
  leitura/escrita, Tab ou → para abrir uma pasta, ← para subir, Enter ao concluir. Pela
  linha de comando, `ai-memory run --jail --jail-map ../ai-jail claude` monta uma
  pasta somente leitura e `--jail-rw-map DIR` leitura/escrita; ambos podem ser repetidos. Sua home,
  `/`, pastas de credenciais (`~/.ssh`, `~/.aws`, `~/.config/gh`, …) e
  o próprio diretório de dados do ai-memory são recusados, e o erro explica o motivo. Edições
  que o agente faz em um irmão com permissão de leitura/escrita são capturadas sob o projeto
  atual, porque a captura segue a sessão. Isso requer o ai-jail 2.15.
- **As mesmas escolhas da próxima vez.** Quando a sessão na jail terminar, o ai-memory
  imprime a linha `ai-memory run …` que repete suas escolhas, lista `--jail=…`,
  pastas e harness incluídos. Cole-a, ou guarde-a como um alias, para
  pular as perguntas. Trocar de harness no menu pós-execução mantém a mesma
  jail e opções.
- **Pulando as perguntas.** `ai-memory run --jail claude` reexecuta dentro do
  ai-jail imediatamente, ativando os padrões pré-marcados e deixando
  todo o restante para sua própria configuração do ai-jail. Isso funciona com ou sem
  `--yolo`, e em scripts também; ele falha em vez de rodar sem jail caso o ai-jail não seja
  utilizável. `--jail=github,ssh,no-mise` é exato: as opções listadas (`no-X`
  força o desligamento), com todas as outras linhas da lista forçadas como desligadas; `all` ativa
  todas as linhas e `none` desativa todas as linhas. `--no-jail` nunca usa jail e pula a
  oferta (o aviso de `--yolo` permanece). Não existem flags isoladas estilo `--github`
  de propósito: elas colidiriam com as flags do próprio harness (o Claude
  Code possui `--worktree`). As montagens de credenciais exigem o ai-jail 2.5.0; opções
  que o ai-jail instalado não possuir são ocultadas, e indicar uma constitui erro.
- **Conectando de volta ao ai-memory.** O processo isolado recebe automaticamente
  a URL resolvida do servidor e o caminho base da URL, incluindo um servidor remoto do
  `config.toml`. Um token root lido do `config.toml` ou de
  `bearer_token_file` não é exposto ao harness gerenciado; a inicialização é interrompida
  em vez disso. Para um servidor autenticado, forneça à inicialização um token de API com
  menor privilégio com `AI_MEMORY_AUTH_TOKEN=... ai-memory run --jail ...`, ou execute
  sem a jail. Um token de ambiente fornecido explicitamente já pertence
  ao limite do processo filho e é encaminhado.
- **Um `.ai-jail` no projeto tem precedência.** Se o diretório a partir do qual você inicializa tiver seu próprio
  `.ai-jail`, o ai-jail o carrega como está: sem lista de verificação, e um `--jail` simples não adiciona
  opções; `--jail=…` ainda aplica sua lista por cima. Um arquivo de projeto não pode
  habilitar credenciais (o ai-jail o trata como não confiável); logo, para montá-las
  automaticamente lá, habilite-as em seu `~/.ai-jail` global ou passe
  `--jail=github,…`. O ai-memory sempre passa `--no-save-config`, de modo que uma execução
  na jail nunca grava suas próprias flags no `.ai-jail` do seu repositório.
- **Já dentro do ai-jail.** Ambos os prompts são ignorados e a execução prossegue
  diretamente, portanto `ai-jail ai-memory run … --yolo` não encontra atrito adicional.
  A detecção ocorre no Linux (hostname `ai-sandbox`) / macOS (`PS1` iniciando com
  `(jail) `); opera em modo fail-open (exibe o aviso) quando indetectável, nunca
  abrindo brecha para pular silenciosamente.
- **Claude "true yolo".** `--true-yolo` inclui tudo o que o `--yolo` faz
  (`ai-memory run claude --true-yolo` é suficiente; adicionar `--yolo` também não causa
  problemas) e, para o Claude, também impõe `bypassPermissions` sobre qualquer
  `defaultMode` em suas configurações. Para outros harnesses, equivale a
  `--yolo`. `claude_true_yolo = true` em `config.toml` /
  `AI_MEMORY_CLAUDE_TRUE_YOLO=true` aplica o extra do Claude a cada inicialização com
  `--yolo`, nunca a uma execução sem ele.
  **Ele não pode remover suas próprias regras `ask`**: o Claude Code respeita regras explícitas
  de `permissions.ask` (e suas checagens embutidas de segurança de comandos) em todos os modos,
  portanto uma regra como `Bash(docker run *)` em `~/.claude/settings.json` ainda
  pausará a execução. Para um sandbox sem pausas, remova essas entradas `ask`; regras
  `deny` bloqueiam sem pausar, então podem ficar. Funciona melhor combinado com o ai-jail.
- **Passando variáveis extras de ambiente, ex.: token do GitHub.** `ai-memory run claude --yolo
  --env GH_TOKEN="$(gh auth token)"` encaminha a variável para o agente na jail (exige
  ai-jail 2.4.2 ou posterior quando você aceitar a oferta de jail). Isso
  entrega seu token a um agente isolado, portanto faça isso apenas para trabalhos em que você confiaria nele;
  ele deliberadamente nunca é encaminhado de forma automática.

Consulte [`design-yolo-safety-ai-jail.md`](design-yolo-safety-ai-jail.md) para o
contrato completo.

## Receita: capturar apenas alguns repositórios

`install-hooks --agent claude-code --apply` conecta os hooks para todo o usuário
(`~/.claude/settings.json`), de modo que toda sessão do Claude Code é capturada e você
exclui caminhos com `[capture] ignore_paths` no `.ai-memory.toml`. Para escolher por repositório
em vez disso, execute a partir do checkout:

```bash
ai-memory install-hooks --agent claude-code --scope project --apply
```

Isso grava o arquivo ignorado pelo git `.claude/settings.local.json` onde
o Claude Code o lê (a raiz do git; o diretório atual no Windows ou quando
a raiz do repositório for o seu diretório pessoal) e deixa o arquivo do nível de usuário
intocado. Backups de um arquivo atualizado vão para o diretório de dados, não para o checkout.
Escolha um único escopo por máquina: o Claude Code mescla hooks de projeto e de usuário. O
instalador avisa quando o arquivo não está no gitignore, e `ai-memory uninstall
--only hooks --apply` a partir do checkout remove as entradas novamente.
Apenas Claude Code; outros harnesses mantêm seus arquivos de hooks de nível de usuário.

## Receita: enviar repositórios diferentes para servidores diferentes

Uma máquina, várias organizações, cada uma com seu próprio servidor ai-memory.
Registre cada servidor uma vez sob um nome, com o diretório que ele tem permissão
para atender, e deixe o marcador de cada repositório escolhê-lo:

```bash
ai-memory server add team-a --url https://memory-a.example.com --root ~/work/team-a --auth-token-stdin
ai-memory server add team-b --url https://memory-b.example.com --root ~/work/team-b --auth-token-stdin
```

```toml
# ~/work/team-b/.ai-memory.toml
workspace = "team-b"
server = "team-b"
```

Repositórios sem a chave `server` continuam usando o servidor configurado por
`install-hooks`. O marcador sempre nomeia apenas um perfil; um perfil que não
resolve descarta o evento em vez de enviá-lo para qualquer outro lugar. Detalhes, as
regras de fail-closed e quais integrações oferecem suporte a isso:
[`marker-file.md`](marker-file.md#roteamento-de-captura-para-outro-servidor-server).

## Receita: executar o servidor em um Mac

Use o aplicativo da barra de menus quando quiser um único `.app` que inicie o servidor e
abra as ferramentas existentes (interface web, `ai-memory status`, configuração, logs). Ele não
substitui essas ferramentas por um segundo painel.

```bash
./companions/ai-memory-macos/build.sh
open "companions/ai-memory-macos/dist/AI Memory.app"
```

Arraste **AI Memory.app** para `/Applications`, depois clique em **Install & Start Server**
no menu extra. Conecte um agente usando o binário empacotado:

```bash
BIN="/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"
"$BIN" install-mcp --client claude-code --apply
"$BIN" install-hooks --agent claude-code --apply
```

A memória fica em `~/Library/Application Support/ai-memory`. Substituir o
`.app` não a sobrescreve. Caminhos completos (tarball, código-fonte, Docker, launchd):
[`macos.md`](macos.md).

## A partir do terminal (CLI)

Seu agente executa a maioria destes para você; `run` e `continue` são as formas de iniciá-lo:

```bash
ai-memory run <harness>              # launch a harness, hooks + MCP auto-wired
ai-memory continue                   # resume the newest managed checkout
ai-memory resume --search auth       # pick a matching workstream in this checkout only
ai-memory resume --all               # pick from every linked checkout on this machine
ai-memory workstreams                # list this checkout's managed workstreams
ai-memory status                     # counts, paths, health
ai-memory recover                    # replay outage spool/transcripts safely
ai-memory list-projects              # every workspace/project the server knows about
ai-memory project init               # pin this checkout's scope in a .ai-memory.toml
ai-memory doctor                     # is every harness that ran here captured?
ai-memory backfill                   # import prior local history into an empty store
ai-memory search "wal mode"          # FTS5 lookup in this project
ai-memory recent                     # newest wiki pages in this project
ai-memory write-page …               # save a durable page
ai-memory handoffs                   # list open handoffs for the project
ai-memory message list               # cross-project inbox
ai-memory server list                # server profiles a marker can route to
ai-memory export-okf …               # export the wiki as an OKF bundle
ai-memory serve                      # run the server
```

Em um terminal interativo, `status` e `doctor` agrupam e quebram suas saídas
para facilitar a leitura. Saída redirecionada, JSON, `NO_COLOR` e `TERM=dumb` permanecem
simples; consulte [Apresentação de relatório diagnóstico](diagnostics.md) para o contrato
exato.

## Quando não estiver fazendo o que você espera

- **Servidor/homelab inativo**: `ai-memory run` não precisa do servidor para iniciar um
  harness. Quando o servidor estiver inacessível, ele imprime um aviso destacado e
  inicia de qualquer forma. Hooks continuam capturando para o carretel (spool) local (esvaziado
  automaticamente quando o servidor retorna), um registro existente de MCP
  degrada para ausência de recuperação na sessão, e o código de saída do filho é
  retornado. O que você perde nessa execução é a posse (lease)/contexto do workstream,
  importação imediata de transcrições e entrega de handoff; sessões retomam apenas
  através de um seletor nativo explícito, pois a falta de posse significa ausência de exclusão
  mútua. Uma retomada explícita é registrada em diário com seu cursor nativo pré-inicialização
  exato e a recuperação importa apenas o delta acrescentado; um cursor que não puder ser
  comprovado exige reparo manual. Uma nova execução só é registrada em diário quando uma nova
  sessão nativa puder ser correlacionada; ambiguidades exibem uma ação de backfill manual.
  Assim que o servidor retornar, execute `ai-memory recover`: para harnesses capazes de
  transcrição, ele coloca em quarentena entradas de carretel correlacionadas, esvazia eventos não relacionados
  e reproduz a transcrição exata e delimitada através de `/hook/batch` higienizado;
  o Antigravity não possui exportador de transcrição aceito, portanto sua execução degradada é
  registrada em diário apenas como carretel e só é liberada após a evidência de hook correlacionada ter
  uma confirmação de entrega durável; a recuperação nunca chama seu exportador. Estado de carretel vazio ou
  com perda correlacionada anterior permanece no diário para reparo manual. A recuperação autorizada pelo proprietário
  conclui uma execução gerenciada original expirada apenas quando ela não foi substituída. Um diário cheio recusa
  novas entradas em vez de desalocar trabalho antigo, e eventos descartados no carretel fazem o
  comando falhar enquanto retêm cada entrada do diário para uma execução limpa posterior.
  Reexecutar é seguro. Veja [Inicializações offline
  degradadas](managed-workstreams.md#inicializações-offline-degradadas). Para falhar em vez
  de degradar, passe `--require-server` (ou defina `run.require_server = true` /
  `AI_MEMORY_RUN_REQUIRE_SERVER=true`). Para uma interrupção planejada, considere
  `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0` para que eventos enfileirados nunca sejam descartados
  pela contagem de tentativas de repetição.

- **Nada está sendo lembrado**: hooks podem não estar instalados. `ai-memory run
  <harness>` instala seus hooks + MCP na primeira inicialização por harness,
  versão do ai-memory e diretório de configuração, e novamente após `ai-memory uninstall`. Se esse harness
  já foi iniciado através de `run` e seus hooks desapareceram por outro motivo
  (removidos manualmente, uma falha na primeira conexão), instale-os à mão: `ai-memory
  install-hooks --agent <your-agent> --apply` e `ai-memory install-mcp
  --client <client> --apply`. Em seguida, verifique `ai-memory status` / `ai-memory doctor`.
- **O checkout atual resolve para um projeto inesperado**: execute `ai-memory
  doctor`. Seu bloco de coordenadas de projeto exibe o marcador local efetivo,
  fonte/estilo de identidade de repositório sem credenciais, nomes candidatos canônicos
  e legados, o resultado exato/compatibilidade/ausente/ambíguo do servidor,
  e se uma gravação autorizada pode promover o nome existente no local. A
  verificação é somente leitura: uma coordenada desconhecida é relatada, nunca criada. Se a
  coordenada do servidor for ambígua, um UUID/nome atual retornado é apenas o candidato
  preferencial baseado em identidade para contexto, não uma resolução única ou
  permissão de renomeação. Linhas locais de harness continuam visíveis e suas contagens
  capturadas são mostradas como indisponíveis em vez de zero.
- **Apenas *alguns* agentes estão sendo lembrados**: execute `ai-memory doctor`. Ele lista
  cada harness que possui sessões locais neste projeto e se o servidor
  as capturou. Um harness que você alternou sem instalar seu hook (uma
  lacuna silenciosa: ele mantém seu próprio histórico local sem capturar nada) aparece
  como um aviso com o comando exato `install-hooks` para corrig-lo. Para o Claude
  Code ele também informa o diretório padrão de auto-memory detectado e se
  as exclusões de captura do repositório o cobrem. Um
  `autoMemoryDirectory` customizado não é detectável a partir das transcrições de sessões do Claude
  e não é informado. Um veredito de `excluded` aplica-se apenas a hooks nativos/gerados;
  hooks de compatibilidade em shell e PowerShell não impõem exclusões
  de política de captura.
- **Nada de novo está sendo lembrado de forma alguma**: execute `ai-memory doctor`. Ele funciona
  com o servidor inativo e informa isso; relata o carretel local de hooks (eventos
  aguardando o servidor, e um aviso assim que estiver cheio e eventos recém-capturados
  forem descartados até que haja espaço disponível); e quando os hooks de Codex do ai-memory
  estiverem instalados, ele pergunta ao Codex se ele os executará. O Codex ignora um
  hook que você não confiou, e o ignora novamente após seu comando mudar,
  sem avisar você: abra o Codex, execute `/hooks` e confie neles.
- **Agentes continuam chamando `memory_recall`, `memory_save` ou outras ferramentas que o ai-memory
  não possui**: a entrada MCP do agentmemory ou seu bloco de diretrizes no arquivo de regras
  ainda está lá. `ai-memory doctor` aponta cada um com a correção (consulte
  [Vindo do agentmemory](#vindo-do-agentmemory)).
- **Acabei de instalar hooks em um projeto em que trabalho há algum tempo**: na primeira
  vez que você abrir o projeto após a instalação, o ai-memory importa seu histórico de sessão
  local existente uma única vez (delimitado, higienizado no servidor, apenas em um
  armazenamento vazio) para não começar do nada. Roda automaticamente em
  segundo plano; você pode disparar ou visualizar previamente com `ai-memory backfill`
  (`--dry-run` para ver o que importaria), ou desativar com
  `AI_MEMORY_BACKFILL_ON_START=false`. Um único registro de transcrição com mais de 1 MiB
  (geralmente uma captura de tela em resultado de ferramenta) é ignorado e contabilizado no stderr;
  o restante dessa sessão ainda é importado.
- **Uma busca não encontra algo que você salvou**: confirme o escopo. A memória é
  por projeto; uma página salva no projeto A não retorna no projeto B a menos que
  tenha sido gravada no escopo global ou você consulte com um escopo explícito.
- **Você deseja um recurso de LLM (consolidação, compêndios)**: defina um provedor
  (`AI_MEMORY_LLM_PROVIDER=…`); sem ele, captura + busca continuam funcionando.
