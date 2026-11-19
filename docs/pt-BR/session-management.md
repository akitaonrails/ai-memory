# Autogerenciamento de sessões

Inicie o servidor HTTP com `--enable-session-management`
(`AI_MEMORY_ENABLE_SESSION_MANAGEMENT=true`). Ele vem desativado por padrão e
exige `--transport http`. Isso habilita `/management/v1/projects`,
`/management/v1/sessions`, `/management/v1/sessions/{id}/observations` e
`/management/v1/sessions/{id}/purge` para sessões de navegador humanas existentes.
Bearers de máquina e sessões com troca de senha pendente são recusados; mutações
exigem o par cookie/cabeçalho de CSRF existente.

Listagens exigem escopo explícito de espaço de trabalho/projeto e retornam `page_size`.
Usuários veem suas próprias sessões e sessões legadas compartilhadas, mas só podem excluir
uma sessão de sua propriedade exata com acesso de escrita no projeto atual. Admins podem inspecionar
e expurgar qualquer sessão. Propriedade e concessões são verificadas novamente na transação de escrita;
sessões compartilhadas e impactos colaterais entre projetos exigem um Admin. Páginas permanecem
compartilhadas dentro de projetos autorizados; atribuição nunca se torna um filtro de leitura de página.

Faça um POST em purge com `dry_run: true` para pré-visualizar, ou `confirm: true` para excluir. Uma prévia
não produz efeitos em disco, auditoria ou admissão. Expurgos confirmados usam o lock canônico de
mutação da wiki, webhooks de admissão, checkpoints de git pré/pós e lápides de repetição.
O conhecimento mantido e versões manuais posteriores de páginas sobrevivem. Expurgos do banco de dados
não podem ser revertidos após falhas na limpeza de arquivos: `files_failed` relata os
caminhos restantes, e a recuperação respeita a lápide. O histórico do git, backups e
transcrições nativas podem reter o conteúdo anterior.

Execute `cargo t -p ai-memory-mcp -E 'test(/session_management/)'`,
`cargo t -p ai-memory-store -E 'test(/owned_session_purge/)'` e
`cargo t -p ai-memory-wiki -E 'test(/owned_session_purge/)'` para os testes de fronteira.
