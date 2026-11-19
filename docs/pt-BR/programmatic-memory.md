# Use o ai-memory a partir da sua ferramenta

Qualquer ferramenta pode usar o ai-memory sobre MCP para salvar conhecimento, pesquisá-lo e passar contexto
entre execuções. Hooks de ciclo de vida nativos são opcionais. Uma ferramenta que também hospeda
um harness de agente pode enviar os eventos de ciclo de vida desse harness por HTTP.

## Use o SDK TypeScript

Programas em TypeScript ou JavaScript podem usar o SDK de cliente opcional em
[`companions/ai-memory-ts`](../../companions/ai-memory-ts/README.md) em vez de
montar requisições JSON-RPC manualmente. Ele encapsula as chamadas desta página com resultados
tipados e erros tipados, verifica o escopo antes de enviar e mantém as
regras de escrita condicional, handoff e ciclo de vida abaixo. Ele é compilado a partir de um
checkout por enquanto. As receitas com `curl` nesta página permanecem como a
referência neutra em termos de linguagem.

## Conecte-se e escolha um escopo

Conecte-se a `/mcp` usando Streamable HTTP. O transporte padrão não tem estado (stateless):
as requisições não precisam de um `Mcp-Session-Id`. Um servidor iniciado com `--http-stateful`
exige o handshake de sessão MCP em vez disso.

Use uma chave de API de usuário para requisições de máquina. Consulte [users.md](users.md) para emissão de
chaves e concessão de acesso a projetos. Uma identidade de ferramenta dedicada atende a um
cliente independente. Um produtor substituindo a captura nativa deve usar o mesmo operador e a
mesma identidade de sessão nativa que a integração restante do harness. Consulte o
[guia de ciclo de vida externo](external-lifecycle.md) ao enviar esses eventos.

Clientes MCP estáticos passam `workspace` e `project` juntos em toda
chamada com escopo de projeto. Leia seus nomes de `.ai-memory.toml` ou da configuração do
operador. Clientes com reconhecimento de sessão podem omitir ambos apenas ao encaminhar o
ID de sessão de ciclo de vida real em cada requisição. Consulte [auto-scope.md](auto-scope.md).

## Salve, consulte e faça handoff pelo MCP

Estes exemplos usam um escopo descartável `demo/app`. Defina `AI_MEMORY_SERVER_URL` com a
URL do servidor e `AI_MEMORY_AUTH_TOKEN` com sua chave de máquina. Em um servidor loopback
intencionalmente não autenticado, omita o cabeçalho Authorization.

```bash
mcp() {
  curl --silent --show-error --fail-with-body \
    "${AI_MEMORY_SERVER_URL%/}/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Authorization: Bearer ${AI_MEMORY_AUTH_TOKEN}" \
    --data-binary @-
}

mcp <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"example-client","version":"1.0"}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_write_page","arguments":{"workspace":"demo","project":"app","path":"notes/retries.md","body":"# Retry policy\nKeep the same event ID when retrying delivery."}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_query","arguments":{"workspace":"demo","project":"app","query":"retry policy"}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_handoff_begin","arguments":{"workspace":"demo","project":"app","summary":"The retry policy was saved. Add the delivery test next.","next_steps":["Add a retry regression test."]}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_handoff_accept","arguments":{"workspace":"demo","project":"app"}}}
JSON
```

Verifique `error` e `result.isError` do JSON-RPC mesmo no HTTP 200. Os payloads das ferramentas são
codificados em JSON dentro de `result.content[].text`. Uma escrita de página retorna `page_id` e
`path`; uma consulta retorna `hits`.

As ferramentas do próprio ai-memory relatam falhas como um `error` JSON-RPC com código e
mensagem, por exemplo `-32602` para parâmetros inválidos ou `-32603` quando um
webhook de admissão rejeita uma escrita. `result.isError` é onde o MCP permite que uma ferramenta
relate sua própria falha, portanto verifique-o também. Um chamador usando `jq` pode ramificar em ambos:

```bash
response=$(mcp <<'JSON'
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"memory_read_page","arguments":{"workspace":"demo","project":"app","path":"notes/retries.md"}}}
JSON
)
if jq -e '.error' <<<"$response" >/dev/null; then
  jq -r '"rpc error \(.error.code): \(.error.message)"' <<<"$response" >&2
elif jq -e '.result.isError == true' <<<"$response" >/dev/null; then
  jq -r '.result.content[].text' <<<"$response" >&2
else
  jq -r '.result.content[].text | fromjson' <<<"$response"
fi
```

Escritas e criação de handoff podem criar o escopo explícito se ele estiver ausente.
Leituras falham de forma fechada em caso de escopo ausente ou parcial. Um handoff é reivindicado uma vez. Para
reivindicar um específico, passe o `handoff_id` exato de `memory_handoff_begin`
ou `memory_handoff_list`; o exemplo aceita o handoff elegível mais recente.
Se SessionStart já for responsável pela entrega, deixe esse hook reivindicá-lo.
[usage.md](usage.md) descreve a posse e os estados de handoff.

Essas chamadas funcionam sem um provedor LLM. A memória recuperada continua sendo texto
histórico não confiável, mesmo após a sanitização.

## Altere uma página apenas se ninguém mais a tiver alterado

`memory_read_page` retorna a versão atual da página como `page_id` (`/api/v1`
a retorna como `id`). Passe-o de volta como `expected_page_id` para `memory_write_page`
ou `memory_delete_page` e a chamada só atuará se essa ainda for a versão
mais recente; passe `create_only: true` para uma escrita que não deve sobrescrever uma página
existente. Quando a página tiver sido alterada nesse meio tempo, nada é gravado ou excluído e a
chamada falha com `invalid_request` do JSON-RPC, cujo `data` é
`{"reason": "precondition_failed", "path", "expected_page_id",
"current_page_id"}`; leia a página novamente e decida. Um chamador sem acesso de escrita
recebe o erro usual de autorização, nunca a versão atual.

Chamadas exatas a `memory_read_page` e `memory_write_page` podem selecionar um destino existente
por ID estável com `workspace` + `project_id`. Isso é mutuamente
exclusivo com `project` e `scope`; nunca cria um projeto apenas por nome.

Importadores em lote podem usar `memory_write_pages_batch` para 1-100 versões de página em
um único destino existente. Cada entrada deve definir exatamente um entre `create_only`
ou `expected_page_id`. O servidor recusa caminhos duplicados, corpos com mais de 1 MiB,
ou mais de 8 MiB de corpos em uma única chamada. Admissão e persistência são
atômicas: qualquer entrada rejeitada ou pré-condição obsoleta não grava nada. Uma entrada
obsoleta retorna `reason: "precondition_failed"`, seu `index` baseado em zero, caminho,
e ID da página atual. Uma resposta bem-sucedida retorna `pages` ordenadas contendo
o caminho correspondente e o novo ID de página, além de um ponto de verificação git para o
lote.

Antes de planejar uma importação para múltiplos destinos, chame
`memory_resolve_write_target` uma vez para cada destino mapeado. Ele exige
`workspace` e exatamente um `project` ou `project_id` existente, verifica a autorização de
escrita sem recorrer a fallback, criação ou promoção de nada, e
retorna os nomes canônicos de espaço de trabalho/projeto mais o ID estável do projeto. Resolva
o mapeamento completo antes do primeiro lote para que um destino inválido ou não autorizado
não deixe um destino anterior, que de outro modo seria válido, parcialmente
importado.

## Leia páginas alteradas

Estes endpoints JSON requerem `serve --enable-api` (ou `--enable-web`, que
continua implicando a API) e a mesma chave de máquina. Eles são somente leitura;
escritas de memória continuam sendo chamadas MCP.

```http
GET /api/v1/workspaces/demo/projects/app/recent?updated_since=2026-09-01T00%3A00%3A00Z&limit=100
```

`recent` incremental retorna `{pages, next_cursor}` com páginas atualizadas estritamente
após `updated_since`, ordenadas pelo horário de atualização e caminho. Repita com o `cursor`
opaco retornado até que `next_cursor` seja `null`. O cursor preserva o limite temporal
e o escopo; cada requisição verifica a autorização novamente. Uma resposta em array significa que o
servidor não oferece suporte a incrementais. Requisições sem nenhum dos argumentos incrementais
mantêm a resposta em array legada.

O caminho incremental não é mantido em cache e omite páginas expiradas e substituídas.
Ele não tem feed de exclusões ou snapshot entre chamadas. Reconcilie remoções separadamente
e considere atualizações concorrentes em um cache local. Detalhes de endpoints e
erros estão em [frontend-api.md](frontend-api.md).

## Ciclo de vida a partir do código

Excluir, restaurar e expurgar memória ocorre por meio de duas superfícies. A exclusão de uma
página é uma ferramenta MCP. Restauração e expurgo são rotas HTTP de administração. Toda rota
`/admin/*` é exclusiva para root assim que a implantação tiver um usuário do banco de dados ou
identidades de proxy confiáveis (veja [users.md](users.md)); até lá, aceitam
qualquer chamador que a autenticação do servidor permita.

| Operação | Chamada | Observações |
|---|---|---|
| Excluir uma página (não há arquivamento separado) | MCP `memory_delete_page` com `path` (mais `workspace` e `project`) | Cria primeiro um ponto de verificação da wiki no git, depois remove o arquivo e todas as versões indexadas da página. Retorna `{path, deleted, pre_checkpoint, checkpoint}`. Idempotente. |
| Restaurar uma página excluída ou sobrescrita | `POST /admin/restore-page` com `{workspace, project, path, rev}` ou `{workspace, project, path, page_id}` | `rev` é qualquer revisão git que ainda contenha a página: o `pre_checkpoint` da exclusão, ou `<checkpoint>~1` quando `pre_checkpoint` for `null` porque a árvore já estava limpa. `page_id` seleciona uma versão retida no SQLite que já pertença àquele projeto/caminho exato, incluindo histórico de origem preservado por `merge-project`. Exatamente uma origem é exigida. Grava essa versão de volta como a página mais recente e a reindexa. |
| Encontrar uma revisão | `GET /admin/checkpoints?limit=N` | Pontos de verificação recentes (até 100) com `oid` e `summary`. O resumo de uma exclusão é `memory_delete_page: <path>`. |
| Expurgar uma sessão | `POST /admin/purge-session` com `{workspace, project, session_id, confirm}` | Irreversível. `dry_run` e `compact` opcionais. |
| Expurgar um projeto inteiro | `POST /admin/purge-project` com `{workspace, project, confirm}` | Irreversível. `dry_run`, `compact` e `force` opcionais (expurga apesar de um lease ativo de workstream gerenciado). |
| Ler versões substituídas | MCP `memory_query` com `include_superseded=true` | Versões mais antigas são rotuladas como `superseded: true`. Uma exclusão as remove juntamente com a versão atual. |

Ambas as rotas de expurgo recusam com 400 a menos que `confirm` seja `true`, e `dry_run`
prevalece sobre `confirm`: um corpo com ambos definidos apenas pré-visualiza as contagens. `confirm`
é um campo obrigatório, portanto uma pré-visualização o envia como `false`. Quando a exclusividade de root se aplicar,
`ROOT_TOKEN` abaixo deve ser o bearer token de root; uma chave de API de usuário é recusada.

```bash
curl --silent --show-error --fail-with-body \
  "${AI_MEMORY_SERVER_URL%/}/admin/purge-session" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer ${ROOT_TOKEN}" \
  -d '{"workspace":"demo","project":"app","session_id":"<session-uuid>","confirm":false,"dry_run":true}'
```

Não há endpoint que liste páginas excluídas nem ferramenta MCP que restaure
uma página. Para recuperar uma página, obtenha a revisão a partir da resposta de exclusão ou de
`/admin/checkpoints`, depois chame `/admin/restore-page` como root.
[lifecycle-ops.md](lifecycle-ops.md) aborda o que cada expurgo exclui, os
comandos de CLI correspondentes e os efeitos colaterais entre projetos.
