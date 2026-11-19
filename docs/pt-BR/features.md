# Recursos e ativação

Este guia aborda pré-requisitos, ativação e verificação para os recursos
fornecidos. Siga os guias vinculados para obter todas as opções.
Exemplos de configuração pertencem a `<data_dir>/config.toml`, a menos que uma seção indique
`.ai-memory.toml`. Reinicie o servidor após alterar a configuração do servidor.

## Comece por aqui

A versão web deste guia inclui "Estado efetivo da instalação e próximas
etapas", avaliado para o chamador atual. O endpoint autenticado
`GET /api/v1/docs/features/status` expõe os mesmos fatos seguros. As configurações
vêm do snapshot de inicialização do servidor, enquanto as permissões são verificadas a cada
requisição. Reinicie após alterar a configuração do servidor. Um provedor configurado ainda
pode estar com problemas de integridade, então use a Saúde para diagnosticá-lo. O servidor não pode verificar
hooks remotos, versões de clientes, arquivos marcadores ou preferências do navegador. Conexões ausentes do
host são lidas como "Not reported", e configurações de administrador são lidas como "Restricted"
sem a funcionalidade necessária. A versão em Markdown é uma referência que
corresponde ao build, não um dump de configuração em tempo real.

O painel integrado e esta documentação precisam da interface web habilitada:

```bash
ai-memory serve --transport http --enable-web
```

Abra `/web/docs`, ou o caminho correspondente sob o caminho base e
slug web configurados. A documentação é incorporada no binário, incluindo imagens Docker; nenhum
checkout, conexão com a internet, LLM ou migração de banco de dados é necessário para lê-la.
Ela compartilha a autenticação do servidor web. Ela descreve recursos, não quais
recursos opcionais um servidor em execução individual habilitou.

Instale o cliente nativo em cada máquina de agente e configure a URL do servidor
e as credenciais seguindo o [guia de instalação](install.md). Atualizar um
contêiner de servidor não atualiza clientes ou hooks em uma máquina separada.

## Captura e integração de agentes

Hooks de ciclo de vida capturam observações delimitadas e saneadas de um agente. Eles
exigem um servidor acessível e um harness compatível instalado localmente.

O inicializador gerenciado pode configurar integrações aceitas na primeira inicialização:

```bash
ai-memory run codex
```

Para conectar um cliente que você inicializa diretamente, visualize previamente e depois aplique sua integração:

```bash
ai-memory install-mcp --client codex
ai-memory install-mcp --client codex --apply
ai-memory install-hooks --agent codex
ai-memory install-hooks --agent codex --apply
```

Use `claude-code` para o Claude Code, ou o nome exato aceito do
[guia de instalação](install.md#configurando-outras-clis-de-agentes). O MCP fornece ferramentas;
os hooks fornecem captura. Instalar apenas um não configura o outro.

Verifique: inicie uma sessão e execute o trabalho. Abra seu projeto no painel;
sessões registradas e contagens de eventos capturados devem aumentar. Se não aumentarem, use
o [diagnóstico](diagnostics.md) e verifique o executável do cliente, a URL e as credenciais.

## Contabilização e investigação de tokens

O painel mostra o uso para respostas individuais de modelos e totais por sessão
e tipo de agente. Atualize o cliente nativo do Claude Code ou Codex e reinstale seus
hooks como acima para habilitar a captura. Não há uma chave separada de tokens no painel.
O servidor registra relatórios conforme eles chegam; a sessão não precisa terminar.
O Pi também relata totais cumulativos de sessão por meio de sua extensão gerada
atualizada, mas não registros de respostas individuais. Veja
[limites de captura e contabilização do Pi](usage.md#investigar-o-uso-de-tokens-no-painel)
para configuração, histórico incluído e comportamento de bifurcação (fork).

O formulário de uso de tokens filtra o livro-razão de respostas autorizado completo no
servidor antes de selecionar uma página de 50 linhas. From é inclusivo e Until é exclusivo,
ambos em UTC, e uma URL contendo apenas data significa meia-noite em UTC. Nomes de modelos e ferramentas correspondem
exatamente. Filtros de harness, sessão e turnos nativos cruzam o projeto atual,
limites de sessão e de propriedade. Paginação e recarregamento mantêm todos os filtros. O
total de tokens correspondentes cobre o livro-razão filtrado, enquanto os cartões de resumo mantêm
todo o escopo autorizado para comparação. O horário da resposta é usado quando relatado,
e linhas mais antigas recorrem ao horário da captura.

Abra "Turn & tools" em uma resposta para ver o ID da resposta nativa, turno,
base do timestamp, sessão e referências delimitadas a ferramentas. Seus links restringem o
livro-razão a esse turno ou ferramenta. Requisições e resultados de ferramentas do Claude usam IDs explícitos.
Nomes de ferramentas do Codex são marcados como associações do mesmo turno. Uma associação não
divide o custo de tokens entre ferramentas nem mostra causalidade. Argumentos, saídas e prompts
são excluídos. Atualize o binário nativo em cada máquina de agente para capturar estes
metadados, porque atualizar apenas o contêiner do servidor não pode atualizar clientes
remotos. Linhas existentes permanecem desconhecidas a menos que um cliente nativo reproduza seu
transcrição original. O extrator segue o
[esquema de chamadas de ferramentas do Claude](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)
e os [registros de uso de sessão do Codex](https://github.com/openai/codex/blob/main/codex-rs/core/src/state/session.rs).

Verifique: abra o projeto, depois o agente, depois a sessão e inspecione "Token
usage". Os painéis de uso do projeto e da sessão são atualizados a cada dez segundos. Busca,
filtros de sinais e ordenação cobrem todas as respostas capturadas, e controles de página mostram
cinquenta por vez. Entrada, saída e cache são separados, e relatórios repetidos para
uma mesma resposta são desduplicados. Os cartões de agentes mostram a cobertura.

Relatórios ausentes significam uso desconhecido. Harnesses incompatíveis, hooks mais antigos e
registros de transcrições excluídas ou não aceitas podem deixar lacunas. Hooks atualizados retomam
o histórico legível em lotes delimitados em eventos posteriores, então um backlog grande pode exigir
vários hooks antes que todas as respostas apareçam. Sinais de contexto amplo, saída longa e
acima da média são dicas de investigação, não falhas confirmadas ou
custos monetários. Veja [uso do painel](usage.md#investigar-o-uso-de-tokens-no-painel).

## Temas e navegação responsiva

Escolha "Light", "Dark" ou "System" no cabeçalho. O navegador salva sua
preferência localmente. Nenhuma configuração no servidor ou reinicialização é necessária.
Em telas pequenas, abra o menu para projetos, caixa de entrada, busca e documentação.

Dentro de um projeto, "Overview", "Agents", "Token usage", "Sessions" e
"Knowledge" são abas separadas: apenas a seção selecionada é mostrada. Overview
contém o resumo e as mensagens pendentes; as outras abas contêm seus respectivos
cartões, diagnósticos de resposta, lista de sessões e páginas compiladas. A URL preserva
a aba selecionada para compartilhamento, recarregamentos e avançar/voltar do navegador. Use Esquerda/Direita,
Home ou End enquanto focado nas abas para navegar pelo teclado. "Inbox" e
"Link graph" abrem suas próprias páginas. As abas quebram linha no celular sem ocultar
destinos fora da tela.

## Caixa de entrada de agentes e mensagens direcionadas

Enfileire uma solicitação para um projeto, opcionalmente direcionando a um harness como Codex
ou Claude Code. O destinatário deve ter uma sessão capturada nesse projeto para
aparecer no seletor do navegador.

Abra "Inbox" para ver contagens enfileiradas por espaço de trabalho e projeto, com um link direto
para cada caixa de correio. Sem nenhum projeto na URL, uma caixa de correio com mensagens em espera é
selecionada primeiro. O título nomeia a caixa de correio aberta, e abri-la não
coleta mensagens. A barra lateral verifica mensagens pendentes imediatamente e a cada
dez segundos enquanto a página estiver visível. Uma contagem e até cinco links de projetos
aparecem apenas quando houver correio aguardando por um agente, e desaparecem após
a coleta. Cada link abre "Awaiting collection", de modo que mensagens pendentes mais antigas não
fiquem enterradas atrás do histórico coletado. Use "Message state" para alternar de volta para todas
as mensagens ou outro estado.

Para redigir, escolha "From project". "To project" usa como padrão o mesmo
projeto; altere-o explicitamente para enviar entre projetos. "To agent" lista apenas
harnesses registrados naquele destino, e alterar qualquer um dos projetos limpa
o agente escolhido. Projetos sem sessões capturadas mostram um estado vazio e
desabilitam o envio. Escreva a solicitação e selecione "Queue message". O envio
exige acesso de gravação a ambos os projetos. Um login humano usa sua sessão de navegador
e proteção CSRF. O login mais antigo somente via
GET exige a inserção explícita de um token de gravação para cada envio; o redator
o limpa após a requisição e não o persiste.

Verifique: inspecione "Sent". "Queued" significa pendente; "Collected" significa que um
agente reivindicou a mensagem, não que concluiu o trabalho. Ler a caixa de entrada
pela web nunca reivindica mensagens nem inicia um agente.

Para alterar uma mensagem que ainda está enfileirada, abra-a em "Received" e selecione
"Edit". A mensagem sai da fila enquanto você edita, de modo que nenhum agente possa coletá-la.
"Save and requeue" coloca o novo texto de volta em seu lugar original, e "Discard
changes" recoloca o texto antigo. Mensagens retidas aparecem como "Editing" e são contadas
acima da lista. Uma mensagem que um agente já coletou não pode ser editada.

Um cliente MCP estático especifica explicitamente o projeto e o harness receptor:

```json
{"workspace":"team","project":"app","agent":"codex"}
```

Passe esses argumentos para `memory_message_pop`, usando seus nomes reais de escopo
registrados. Clientes com reconhecimento de sessão podem usar sua identidade registrada. Um cliente de terminal
pode receber a mesma requisição:

```bash
ai-memory message pop --workspace team --project app --agent codex
```

Mensagens são reivindicadas uma única vez. Leia o [guia de mensagens](agent-messaging.md) para
filas compartilhadas, cancelamento, proveniência do remetente e tratamento de mensagens não confiáveis.

## Handoffs e recuperação pós-compactação

Instale hooks de ciclo de vida para transportar contexto útil para a próxima sessão ou
harness. A captura e a recuperação de handoffs seguem a
política de ciclo de vida configurada; nenhum LLM é necessário para o caminho determinístico.
Use `ai-memory run` para continuidade gerenciada entre harnesses.

Verifique: inspecione o contexto de inicialização e o estado do handoff da próxima sessão.
Uma sessão interrompida pode permanecer aberta sem um evento de término de sessão; isso por si só
não é evidência de um processo em execução. Veja [handoffs e recuperação](usage.md).

## Sessões gerenciadas e workstreams

Inicie um harness compatível com `ai-memory run <harness>` para habilitar sessões
gerenciadas para essa inicialização. Inicializações diretas continuam não gerenciadas; essa configuração não
altera o modo global.
Use as opções documentadas de retomada e perfil de inicialização para configurações
específicas de conta. O root pode inspecionar metadados gravados de ciclo de vida em "Workstreams".

Verifique: abra o workstream e suas sessões vinculadas. O painel relata o
estado registrado; ele não infere o estado de processos ativos a partir do texto do prompt.
Veja [workstreams gerenciados](managed-workstreams.md).

## Recordação, roteamento de projetos e páginas de memória

Cada página consultada inclui "Knowledge evidence and validity" para sua versão
indexada. Sessões de origem legíveis vinculam-se às suas linhas do tempo, e uma citação
de observação identifica a observação dentro de sua origem. O escopo de origem e a propriedade
da sessão são verificados separadamente do acesso compartilhado à página. IDs opacos de feedback e
de reescrita não recebem links de origem inventados. Uma citação ausente deixa a evidência desconhecida; isso não estabelece que
a página careça de suporte. Páginas expiradas permanecem disponíveis por meio de uma URL explícita, mas saem da
recuperação normal. Contradições declaradas incluem apenas extremidades ativas e legíveis,
e não decidem qual declaração está correta. Páginas de evidência contêm vinte
citações.

Buscas de projeto aceitam chaves de projeto canônicas e legadas; seus links de resultado,
trilha de navegação e seletor de escopo usam o nome armazenado atual. A realização de buscas não
renomeia o projeto.

Links de resultado de busca carregam a consulta, filtros, limite de resultados e o modo do projeto ou
global. "Recompute explanation" executa o mesmo classificador autorizado sob demanda
e mostra as classificações de streams, contribuições RRF, autoridade e reclassificação opcional
ao lado da página. Pode entrar em contato com provedores configurados de embeddings ou reclassificação. O
resultado é um novo cálculo, não um snapshot da busca original.
A busca entre projetos relata seu stream somente FTS, e uma URL de página direta não tem
motivo de recuperação inferido.

Padrão: recuperação lexical e resumos baseados em regras funcionam sem um LLM.
Conecte o MCP e, em seguida, use as habilidades de recuperação instaladas e `memory_query`.
A memória do projeto é separada desta documentação do produto.

Para uma identidade de projeto explícita, use os nomes registrados em `.ai-memory.toml`:

```toml
workspace = "team"
project = "app"
```

Estes são nomes ilustrativos. Obtenha nomes reais com o servidor ou o operador;
não os infira a partir de um diretório. Clientes MCP estáticos enviam ambos os campos de escopo;
clientes com reconhecimento de sessão podem usar o roteamento automático.

Verifique: confira o escopo resolvido antes de gravar e confirme se as páginas do projeto
esperado aparecem. Veja [arquivos marcadores](marker-file.md), [uso](usage.md),
e a [referência de configuração](ARCHITECTURE.md#configuração-configtoml).

## Embeddings e reclassificação (reranking)

Para recuperação semântica e reclassificação opcional de candidatos, configure o
provedor de embeddings, o modelo e as credenciais descritos em
[provedores de LLM](llm-providers.md) e na [referência de configuração](ARCHITECTURE.md#configuração-configtoml).
A disponibilidade e os padrões dependem da configuração do provedor e do build.
A reclassificação por LLM é uma opção ativada separadamente por meio de `AI_MEMORY_RERANKER=llm` e exige
um provedor de LLM compatível e configurado.

Verifique: use explicações de consultas e diagnósticos para inspecionar os streams ativos
de classificação. Mantenha provedor/modelo/dimensão alinhados com os embeddings armazenados; vetores
antigos são ignorados. A recuperação lexical comum permanece disponível sem eles.

## Consolidação por LLM e fallback de provedores

Para compilar evidências capturadas em conhecimento mantido com um provedor de modelos
opcional, configure `llm_provider`, o modelo e o fluxo de credenciais do provedor
de [provedores de LLM](llm-providers.md). O suporte a provedores e os
tipos de credenciais estão documentados lá; não cole credenciais dentro das páginas.

Sem um LLM, captura, recuperação lexical e resumos determinísticos ainda funcionam.
Chamadas ao provedor são separadas das próprias chamadas de modelo do harness do agente.
A consolidação por provedor no término da sessão é opcional; veja o guia de provedores para
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END` e comportamento de pontos de verificação (checkpoints).

Verifique: inspecione páginas compiladas, tarefas e diagnósticos após um gatilho de
consolidação documentado. Configure o fallback ordenado apenas se necessário; o
[guia de provedores](llm-providers.md) fornece o link para a referência de fallback.

## Aprendizado e revisão de propostas

Pré-requisito: um provedor de LLM configurado e sessões concluídas elegíveis.
O agendador de aprendizado é ativado por padrão quando existe um provedor; cadência
e filtros prévios podem adiar o trabalho. Para exigir revisão humana:

```toml
[auto_improve]
require_approval = true
```

Para interromper a revisão agendada mantendo as operações manuais:

```toml
[auto_improve.scheduler]
enabled = false
```

Verifique: inspecione "Learning review" como root e a trilha de auditoria
de gravações pendentes. Propostas são aprovadas automaticamente por padrão quando a aprovação não é necessária. Leia
a seção de comportamento atual de [autoaprimoramento](auto-improvement-loop.md)
antes de aplicar exemplos de suas seções de design histórico.

## Exclusões de captura e texto do assistente

Ativar exclusões: adicione uma seção `[capture]` ao marcador de projeto mais próximo,
usando os padrões de caminho documentados em [arquivos marcadores](marker-file.md#exclusões-de-captura).
Eventos reconhecidos de ferramentas de arquivos que forem excluídos são descartados antes do transporte e do armazenamento.
O saneamento permanece ativo para o conteúdo aceito.

A captura do texto do assistente é uma opção separada: a configuração `capture_assistant`
do servidor e hooks de clientes instalados com `--capture-assistant` devem coincidir.
Veja [instalação](install.md#capacidade-e-atualização-da-política-de-captura).

Verifique: use conteúdo de teste sintético e não confidencial e verifique se o conteúdo
excluído não aparece na linha do tempo com escopo. Não teste com segredos reais.

## Autenticação, acesso de equipes e o console de gerenciamento

Ativar: siga a [configuração de autenticação](install.md#bootstrap-e-recuperação-de-senha-humana)
e as [orientações de segurança](security.md). Mantenha o limite existente de
funcionalidades de root/usuário e conceda acesso ao projeto explicitamente. Servir além do loopback exige
a configuração adequada de autenticação, host e transporte.

O console de gerenciamento exige adicionalmente um build com a funcionalidade habilitada e uma
flag de tempo de execução:

```bash
cargo build --locked --release -p ai-memory-cli --features management-ui
./target/release/ai-memory serve --transport http --enable-management-ui
```

A imagem padrão não inclui este console opcional. Para uma imagem Docker compilada
a partir do código-fonte, use `--build-arg AI_MEMORY_FEATURES=management-ui` e ative a
configuração de tempo de execução descrita em [console de gerenciamento](management-ui.md).
As APIs de gerenciamento de sessão e de chaves autosserviço também possuem seletores
independentes de compilação/execução; siga os links daquele guia para essas opções.

Verifique: o root pode acessar páginas de operador; um usuário regular vê apenas dados
de projetos permitidos e sessões de sua propriedade. Chaves de API nativas e sessões de navegador humano têm
funcionalidades diferentes. Veja [SSO](sso.md) para identidade de dispositivo OIDC.

## Clientes externos e captura de ciclo de vida

Ativar: escolha o caminho de integração que melhor se adapte ao cliente. Aplicações podem
usar ferramentas MCP públicas por meio do [guia programático](programmatic-memory.md).
Um orquestrador que possui eventos de ciclo de vida pode usar o caminho de captura externa
e o relay opcional descritos em [ciclo de vida externo](external-lifecycle.md).
Esses utilitários adicionais são instalados separadamente do servidor principal.

Verifique: novas tentativas com a mesma identidade de evento não duplicam a captura,
as coordenadas do projeto permanecem corretas, e a captura nativa e a externa não
possuem ambas o ciclo de vida do mesmo processo.

## Backup, retenção e implantação

Ativar: use as configurações documentadas de implantação e manutenção para sua
instalação. Backups são operações online explícitas; comandos de restauração e
destrutivos têm requisitos diferentes quanto a processos ativos e confirmações.

```bash
ai-memory backup --to /path/to/private-backups/memory.tar.gz
```

Verifique: retenha o arquivo compactado fora de camadas descartáveis de contêiner e siga
as [operações de ciclo de vida](lifecycle-ops.md) antes de testar a restauração. Mantenha o
volume do servidor, as credenciais e as instalações do cliente ao atualizar. Veja
[implantação](deploy.md), [HTTPS](https-via-proxy.md) e [diagnóstico](diagnostics.md).

## Acesso à documentação e ao changelog para agentes

Os endpoints JSON autenticados são referências de produto somente leitura:

```text
GET /api/v1/docs?q=token
GET /api/v1/docs/features
GET /api/v1/docs/features/status
GET /api/v1/docs/changelog?release=Unreleased
```

O catálogo retorna IDs de documentos, títulos, categorias, descrições e caminhos
de origem. Uma resposta de documento adiciona Markdown, a versão do pacote do build e
opções de release quando aplicável. Não contém dados de projeto/sessão ou segredos de
configuração em tempo real. Use o prefixo de servidor configurado e suas credenciais.

Para Markdown, use `/web/docs/features.md` ou
`/web/docs/changelog.md?release=Unreleased`. Mude `/web` para seu slug
web configurado. O changelog adota como padrão uma release; requisite `release=all` para toda
a sua série principal, ou escolha uma série arquivada no catálogo.

No changelog web, selecionar uma release carrega suas notas imediatamente e atualiza
a URL compartilhável, a estrutura de tópicos e o link do Markdown. Sem JavaScript, use "Show
release" para enviar a seleção.

O [Changelog](../../CHANGELOG.md) separa trabalhos não lançados de releases marcadas.
Guias e notas são fornecidos com este binário; referências fora do catálogo
incorporado abrem seu código-fonte do repositório no GitHub e exigem acesso à internet.
A consulta à documentação não habilita um recurso nem executa seus exemplos.
