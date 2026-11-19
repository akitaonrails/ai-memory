# Chaves de API MCP em autoatendimento

Inicie o servidor HTTP com `--enable-self-service-keys`
(`AI_MEMORY_ENABLE_SELF_SERVICE_KEYS=true`). Fica desativado por padrão e requer
`--transport http`. Sessões humanas ativas no navegador podem fazer GET ou POST em
`/management/v1/credentials` e POST em `/management/v1/credentials/{id}/revoke`.
Bearers de máquina e sessões com troca de senha obrigatória não podem usar essas rotas;
mutações exigem o par cookie/cabeçalho de CSRF existente.

Envie um `label` via POST para criar uma chave para o ID de usuário autenticado e imutável. O
servidor retorna a chave `aim_` uma única vez e armazena apenas o seu hash com pepper. O
label tem os espaços aparados e deve ter de 1 a 128 caracteres (caso contrário, 400). Um usuário pode
manter no máximo 20 chaves ativas; a 21ª é recusada com 409 até que alguma seja
revogada. Configure `[auth] token_pepper` como nos comandos existentes de
administração de chaves nativas. A revogação responde 404 para credenciais de outro usuário,
um ID desconhecido ou uma chave já revogada, nunca confirmando que a
chave de outra pessoa existe. A listagem retorna metadados e prévias, nunca o conteúdo
do token. As respostas utilizam `Cache-Control: private, no-store`.

Use a chave como credencial Authorization: Bearer para `/mcp` e hooks de
ciclo de vida. Ela permanece no nível de Usuário mesmo se o proprietário for Admin. Uma redefinição
de senha ou desativação de conta encerra as sessões no navegador, mas não revoga chaves
emitidas pelo próprio usuário; revogue-as separadamente. Nenhum provedor de identidade, implementação
de senha, banco de dados ou esquema é adicionado.

Execute `cargo t -p ai-memory-mcp -E 'test(/self_service_keys/)'` para os testes adversariais
de usuário anônimo, máquina, CSRF, senha pendente, usuário desativado, ID externo, desconhecido
e revogado, label, limite de chaves e proprietário legítimo.
