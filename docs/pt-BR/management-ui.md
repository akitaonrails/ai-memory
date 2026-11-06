# Console de gerenciamento

O console opcional em `/manage` é renderizado no servidor pelo askama em
`ai-memory-web`, usando a folha de estilos Tailwind existente e empacotada. Compilações
comuns do Cargo não requerem Node, npm, empacotador ou framework JavaScript. Um pequeno
script inline autorizado por hash aprimora a seleção de temas e validação de senhas;
login, gestão de contas e operações de sessão/chave também funcionam sem JavaScript.

## Ativação

```bash
cargo build --locked --release -p ai-memory-cli --features management-ui
./target/release/ai-memory serve --transport http --enable-management-ui
```

A configuração em tempo de execução é `AI_MEMORY_ENABLE_MANAGEMENT_UI=true`. Ambas as opções
de compilação e execução vêm desativadas por padrão. Sem elas, o console fica ausente. A compilação
da interface inclui os recursos revisáveis independentemente `session-management` e
`self-service-keys`; habilitar a interface também habilita suas rotas HTTP.
Eles podem, em vez disso, ser compilados e habilitados individualmente sem interface; consulte
[gerenciamento de sessões](session-management.md) e [chaves de autoatendimento](self-service-keys.md).

Com `AI_MEMORY_BASE_PATH=/memory`, abra `/memory/manage`. Nenhum `--enable-web` ou
`--enable-api` é necessário. Se o navegador da wiki também estiver ativado, seu `--web-slug`
deve ser diferente de `/manage`.

Para compilações Docker a partir do código-fonte, passe `--build-arg AI_MEMORY_FEATURES=management-ui` e
defina `AI_MEMORY_ENABLE_MANAGEMENT_UI=true` ao executar a imagem. A imagem padrão
de release não inclui o console opcional.

## Autenticação e implantação

Use o bootstrap/recuperação de senha humana existente descrito em
[instalação](install.md#bootstrap-e-recuperação-de-senha-humana). O console
reutiliza verificação Argon2id, sessões de navegador, hashes CSRF, hashing nativo de credenciais,
portões de capacidade e o mesmo banco de dados SQLite. Ele não adiciona nenhum provedor de identidade,
formato de credencial, implementação de autenticação ou esquema de banco de dados.

O papel `root` existente é rotulado como Admin e `user` é rotulado como User. Admins
criam usuários, alteram papéis, redefinem senhas, desativam/ativam contas, emitem e
revogam chaves, e inspecionam ou expurgam qualquer sessão. Usuários inspecionam sessões próprias
e compartilhadas legadas, expurgam sessões próprias com acesso de escrita no projeto atual,
alteram senhas e gerenciam suas chaves. A salvaguarda do último root ativo
permanece em vigor. Novos usuários devem trocar a senha temporária antes de usar
o console. O formulário de senha requer 12 pontos de código Unicode; a política
subjacente de bytes UTF-8 do motor permanece inalterada.

Credenciais de navegador não autenticam MCP ou hooks. Use uma chave `aim_` pessoal
a partir da conexão MCP. Chaves nativas permanecem no nível User mesmo para proprietários Admin.
Desativar uma conta remove o acesso pelo navegador, mas mantém chaves de máquina existentes
ativas até serem revogadas explicitamente, como na CLI existente.

Use a [configuração de proxy reverso HTTPS existente](https-via-proxy.md), cookies seguros
e `AI_MEMORY_ALLOWED_HOSTS`. Uma VPN é opcional. Encaminhe o cabeçalho `Host`
original: formulários POST comuns exigem Origin HTTP(S) com a mesma autoridade e
só aceitam `Sec-Fetch-Site: same-origin` ou `none` quando esses metadados de navegador
estiverem presentes; requisições `same-site`, `cross-site`, malformadas e de origem divergente
são recusadas. Isso preserva a barreira contra login-CSRF ao converter
dados de formulário HTML para o manipulador JSON existente. Formulários de mutação também
carregam o token CSRF existente. Bearers de máquina não podem usar o console.

Templates autoescapam texto armazenado. Formulários selecionam operações fixas e ids tipados
de sessão/chave, e delegam em processo para os mesmos roteadores autenticados
do motor. Apenas a identidade do par de transporte é repassada em extensões de requisição, logo
limites de taxa de login retêm o par real e metadados obsoletos de rota/auth não podem
alterar a requisição delegada. Nenhum proxy HTTP em localhost é criado. Toda resposta do console
nega enquadramento e usa `private, no-store`; o aprimoramento inline possui
um hash CSP SHA-256. Senhas e novas chaves nunca aparecem em URLs ou armazenamento do navegador.

## Exclusão de sessão

A exclusão primeiro pré-visualiza contagens e depois exige confirmação explícita.
Pré-visualizações não têm efeito em arquivos, lápides, auditoria ou admissão. A confirmação usa
a proteção existente de mutação da wiki, webhooks de admissão, checkpoints git pré/pós
e a transação com gravador único. Propriedade e acesso de escrita atual são
reverificados nessa transação. Sessões compartilhadas sem dono e colaterais entre
projetos exigem um Admin; apenas um Admin recebe a prévia de impacto colateral.
Tanto observações colaterais quanto handoffs aparecem na pré-visualização.

O expurgo remove observações capturadas, handoffs de sua autoria, versões geradas de páginas
de sessão e registros de revisão. Conhecimento mantido e versões manuais posteriores de páginas
permanecem, assim como handoffs aceitos pela sessão, mas com autoria externa. Histórico
git, backups e transcrições nativas podem reter o conteúdo anterior. Uma limpeza parcial
de arquivos não pode reverter o expurgo do banco de dados; a UI relata isso e a repetição
e a reindexação respeitam a lápide de expurgo.

## Interface e desenvolvimento

O console usa uma paleta operacional clara/escura contida com superfícies
neutras, ações primárias em âmbar e indicadores de foco em cerceta. O tema do Sistema é o
padrão; preferências explícitas de Claro/Escuro persistem localmente. Fontes e
seu aviso de licença são incorporados sob
`static/management/` e servidos apenas pelo console opcional.

Templates residem em `templates/management/`. `src/management/views.rs` constrói
view models escapados, `forms.rs` mapeia operações POST comuns e `backend.rs`
delega para os roteadores do motor. O navegador normal da wiki permanece somente leitura. A paginação
de sessões e eventos usa o tamanho de página retornado pela API do motor.

```bash
TAILWIND_BUILD=1 cargo build -p ai-memory-web --features management-ui
cargo fmt --all -- --check
git diff --check
cargo clippy --workspace --all-targets --features ai-memory-cli/management-ui -- -D warnings
cargo tf --features ai-memory-cli/management-ui
```

Faça commit do `static/tailwind.css` regenerado quando templates ou estilos forem alterados.
A CLI independente e fixada do Tailwind é a exceção existente de regeneração do projeto;
compilações normais copiam o CSS fornecido e não necessitam de downloads.

Testes de integração em Rust exercitam sessões humanas reais, encaminhamento de cookies, login
comum de mesma origem, recusa de CSRF, portões de senha pendente, credenciais/escopos/sessões
externos, escape HTML, ids de ação tipados, rejeição de admissão, limpeza/recuperação
de expurgo próprio, recursos opcionais e montagem em subcaminho. Os mesmos testes são executados
no job de CI com a funcionalidade ativada; nenhum toolchain JavaScript é necessário ali.
