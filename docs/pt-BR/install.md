# Livro de receitas (cookbook) de instalação

O [início rápido do README](../../README.md#quick-start) cobre os caminhos
felizes (a instalação nativa de dois comandos, Docker + Claude Code, Arch AUR,
Fedora RPM, app de barra de menus do macOS). Esta página cobre todo o resto:

- [Instalação nativa em uma etapa](#instalação-nativa-em-uma-etapa-installsh-ai-memory-setup)
  (`install.sh` + `ai-memory setup`, Linux e macOS, sem root)
- [Plugins de marketplace](#plugins-de-marketplace)
  (Claude Code e Codex: hooks, MCP e skills como um único plugin)
- [Servidor em uma máquina diferente](#servidor-em-uma-máquina-diferente)
  (homelab, máquina na LAN, servidor remoto)
- [Configurando a URL da CLI e a autenticação](#configurando-a-url-da-cli-e-a-autenticação)
- [Pacote nativo do Fedora (RPM)](#fedora-rpm)
  (serviço de sistema ou serviço de usuário do systemd)
- [Pacotes nativos do Arch Linux (AUR)](#pacotes-nativos-do-arch-linux-aur)
  (serviço de sistema ou serviço de usuário do systemd)
- [Nix / NixOS](#nix--nixos)
  (pacote flake, módulo NixOS, esteira de testes do mantenedor)
- [Aplicativo da barra de menus do macOS](#aplicativo-da-barra-de-menus-do-macos)
  (`.app` independente + LaunchAgent)
- [Configurando outras CLIs de agentes](#configurando-outras-clis-de-agentes)
  (Codex, Command Code, IBM Bob, Devin CLI, OpenCode, OpenCode 2, OMP, Pi, Cursor, Claude Desktop, Gemini CLI, Qwen Code, Antigravity CLI, Grok Build CLI, GitHub Copilot CLI, Factory Droid, Meta Muse Code, CodeBuddy Code, Hermes Agent, Zero, ZCode, Kimi Code, Kiro CLI, Cline CLI, Pool, OpenClaw, FreeBuff, VS Code Copilot, Zed e receitas apenas com MCP para Warp, Kilo Code, Goose e OpenHuman)
- [Instalando hooks sem o Docker](#instalação-de-hooks-sem-o-docker)
  (instalador baseado em curl)
- [Executando o ai-memory sem o Docker](#execução-do-ai-memory-sem-o-docker)
  (mise, compilando a partir do código-fonte)
- [Workstreams gerenciados entre diferentes harnesses](managed-workstreams.md)
  (`ai-memory run`, retomada nativa transparente e encaminhamento de argumentos)
- [Camadas de provedores LLM + Ollama auto-hospedado](#níveis-de-provedor-llm)
- [Subcomandos comuns](#subcomandos-comuns)
- [Trechos de roteamento gerenciados e Agent Skills](#snippets-de-roteamento-gerenciados-e-agent-skills)
- [Inicialização e recuperação de senha humana](#bootstrap-e-recuperação-de-senha-humana)
- [Operando sem autenticação](#operando-sem-autenticação) (somente local)
- [Mantendo o ai-memory atualizado](#mantendo-o-ai-memory-atualizado)

> **Abreviação.** A maioria dos trechos usa `$TOKEN` e `homelab:49374`. Se
> você estiver acompanhando literalmente:
> ```bash
> export TOKEN=$(docker run --rm akitaonrails/ai-memory:latest generate-auth-token)
> ```
> e substitua `homelab` por `localhost` se o servidor for executado na
> mesma máquina que a CLI do agente.

A imagem Docker é publicada para `linux/amd64` e `linux/arm64`; Macs com Apple
Silicon e hosts Linux ARM64 não devem precisar de `--platform linux/amd64`.

> **Podman.** O wrapper `bin/ai-memory` usa automaticamente o Podman rootless
> quando o Docker não está instalado. Ele também funciona através do shim
> `podman-docker` `docker`; defina `AI_MEMORY_DOCKER=podman` para forçar o Podman quando ambos os motores
> estiverem instalados. O nome padrão da imagem é totalmente qualificado para a resolução de nomes curtos
> não interativa do Podman. Consulte
> [Hosts com SELinux em modo enforcing](#hosts-com-imposição-de-selinux) para saber como ele detecta o
> estado rootless e do SELinux do motor.

---

## Instalação nativa em uma etapa (`install.sh` + `ai-memory setup`)

No Linux (x86_64, aarch64) e macOS (Apple Silicon, Intel), dois comandos levam
do zero a um servidor em execução com seus agentes configurados. Nenhum precisa de root.

```bash
curl -fsSL https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh | sh
ai-memory setup
```

O `install.sh` é publicado a cada release, ao lado do seu próprio
`install.sh.sha256`. Ele escolhe `ai-memory-<os>-<arch>.tar.gz` para a máquina,
baixa-o com seu arquivo `.sha256` e interrompe antes de descompactar qualquer coisa quando
o arquivo de checksum estiver ausente, nomear outro ativo ou não coincidir. Ele também
recusa um arquivo compactado contendo um caminho absoluto ou com `..`, ou qualquer membro que não seja
um arquivo comum ou diretório (um link simbólico ou hard link), e descompacta com
`--no-same-owner`. Sem o curl ele recorre ao wget, e para a URL padrão
do GitHub apenas a um wget que ofereça suporte a `--https-only` (para que um redirecionamento não possa
cair para http simples); o wget do BusyBox é recusado nesse caso. O release vai para
`~/.local/lib/ai-memory`, e `~/.local/bin/ai-memory` torna-se um link para o
binário, de modo que o diretório `hooks/` incluído fica ao lado dele e
o `ai-memory upgrade` pode posteriormente substituir ambos. Um `ai-memory` no caminho do link que
não seja esse link (o wrapper Docker, um binário compilado manualmente) é deixado intocado e
o script é encerrado. Todo o script é executado a partir de uma função chamada em sua última
linha, portanto, um download interrompido não executa nada. Para lê-lo antes de executar:

```bash
curl -fsSLO https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh
curl -fsSLO https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh.sha256
sha256sum -c install.sh.sha256   # macOS: shasum -a 256 -c install.sh.sha256
sh install.sh
```

| Variável | Padrão | Efeito |
|---|---|---|
| `AI_MEMORY_VERSION` | latest | Instala este release, por ex., `3.0.0` |
| `AI_MEMORY_BIN_DIR` | `~/.local/bin` | Onde o link do `ai-memory` é criado |
| `AI_MEMORY_LIB_DIR` | `~/.local/lib/ai-memory` | Onde o release é descompactado |
| `AI_MEMORY_RELEASE_BASE_URL` | GitHub Releases | Um espelho servindo `latest/download/<asset>` e `download/v<version>/<asset>`; a mesma variável que o `ai-memory upgrade` lê. O checksum vem da mesma origem que o arquivo compactado, portanto, use apenas um espelho sob seu controle. |
| `AI_MEMORY_INSTALL_TARGET` | detectado | Força `linux-x86_64`, `linux-aarch64`, `macos-x86_64` ou `macos-aarch64` |

O Windows nativo tem seu próprio script, `install.ps1`, publicado ao lado de
`install.sh` com seu próprio `install.ps1.sha256`. No PowerShell (nenhum
privilégio de administrador necessário):

```powershell
irm https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1 | iex
ai-memory setup
```

Ele baixa `ai-memory-windows-x86_64.zip` e seu `.sha256`, interrompe antes de
descompactar quando o arquivo de checksum estiver ausente, nomear outro ativo ou não
coincidir, e recusa uma entrada do arquivo compactado que seja absoluta ou contenha `..`. O
release vai para `%LOCALAPPDATA%\Programs\ai-memory` (`AI_MEMORY_INSTALL_DIR`
o substitui), que é adicionado ao seu `PATH` de usuário a menos que
`AI_MEMORY_NO_MODIFY_PATH=1`. Uma instalação existente lá é deixada intocada; atualize-a
com `ai-memory upgrade`. `AI_MEMORY_VERSION` e `AI_MEMORY_RELEASE_BASE_URL`
funcionam como no `install.sh`. No Windows on Arm, ele instala a compilação x86_64, que o
Windows 11 executa sob emulação. Os erros são disparados em vez de encerrar o shell, portanto, uma
falha sob `iex` mantém sua janela do PowerShell aberta. Os detalhes estão em
[`docs/windows.md`](windows.md).

Com o [Scoop](https://scoop.sh), adicione o bucket do mantenedor e instale a partir dele
em vez disso (ai-memory 3.4.0 ou posterior):

```powershell
scoop bucket add akitaonrails https://github.com/akitaonrails/scoop-bucket
scoop install ai-memory
ai-memory setup
```

O Scoop verifica o zip em relação ao SHA-256 publicado com o release e coloca
um shim `ai-memory` no `PATH`. Atualize com `scoop update ai-memory`;
o `ai-memory upgrade` recusa uma instalação do Scoop, porque o bucket gerencia a
versão.

`ai-memory setup [--yes] [--agent <name>]... [--no-service] [--plan [--json]] [--web [--no-open]]`
executa, em ordem, os comandos que uma instalação nativa de outra forma precisaria um por um:

1. `init`. Em um diretório de dados novo, isso gera o token bearer root em
   `<data_dir>/auth-tokens/default` (somente proprietário) e registra
   `[auth] bearer_token_file`, conforme descrito em
   [`[auth]` root bearer](ARCHITECTURE.md#auth-root-bearer). Uma instalação existente
   mantém sua configuração e postura de autenticação; `ai-memory auth enable` ainda é a
   opção de ativação para um token nesse caso.
2. Um serviço por usuário executando `serve --transport http --enable-web` nesse
   diretório de dados e configuração: `~/.config/systemd/user/ai-memory.service` (ou sob
   `$XDG_CONFIG_HOME`) no Linux, gerado a partir da unidade de usuário empacotada, ou
   `~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist` no macOS,
   gerado a partir do LaunchAgent empacotado. A unidade executa o binário a partir do qual o `setup`
   foi executado. Quando esse binário reside em um local de pacote versionado (o
   `Cellar/ai-memory/<version>` do Homebrew, um caminho do `/nix/store` ou o
   `apps\ai-memory\<version>` do Scoop, todos perdidos após uma atualização e `brew cleanup`,
   coleta de lixo ou `scoop cleanup`), a unidade nomeia um link estável que
   resolve para o mesmo arquivo em vez disso: o caminho conforme invocado, um `ai-memory` no
   `PATH` como `~/.nix-profile/bin/ai-memory`, o
   `opt/ai-memory/bin/ai-memory` ou `bin/ai-memory` do Homebrew, ou o
   `apps\ai-memory\current\ai-memory.exe` do Scoop. Sem esse link, ela nomeia
   o caminho versionado, e você reexecuta o `setup` após a atualização.
   No Windows não há gerenciador de serviços por usuário, portanto, o `setup` grava um
   valor `ai-memory` sob `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
   (nenhum direito de administrador necessário) que executa `ai-memory service-host` no
   logon e inicia esse host imediatamente. O host executa `serve --transport
   http --enable-web` no mesmo diretório de dados e configuração sem janela de console
   e o reinicia após uma falha, desistindo após cinco falhas em até dez
   segundos após a inicialização; consulte
   [`docs/windows.md`](windows.md#scenario-e-keep-the-server-running-native-windows-service).
   A etapa é ignorada quando um servidor ai-memory já responde na URL
   configurada e nenhuma unidade ou entrada de logon nossa existe ainda (Docker, uma unidade
   de sistema empacotada, um `serve` em primeiro plano), quando o servidor configurado não estiver nesta
   máquina, e com `--no-service`. O setup exige uma resposta bem-sucedida
   `application/json` com `{"status":"ok"}` de `/healthz` antes
   de tratar um listener como saudável. Uma resposta não reconhecida deixa qualquer serviço
   local inalterado e falha no autoteste com um diagnóstico. Verifique qual
   processo possui o endereço, libere-o ou altere `bind` no
   `config.toml` indicado e execute o setup novamente; atualize também um `server_url` explícito.
   O serviço em segundo plano do OpenCode também tem como padrão a porta 49374 e pode ser movido
   com `opencode service set port <free-port>`.
3. Para cada agente: `install-mcp --apply`, `install-hooks --apply` e
   `install-skills --scope global`. Sem `--agent`, o `setup` configura cada
   harness cujo armazenamento de sessões ou configuração MCP exista sob sua home, usando os
   mesmos locais de armazenamento que o `doctor` (incluindo `CLAUDE_CONFIG_DIR`,
   `CODEX_HOME` e realocações semelhantes). O plano numera cada agente e
   informa pelo que ele foi encontrado (seu armazenamento de sessões, sua configuração MCP ou, no caso do Muse,
   seu diretório), listando em seguida os outros agentes aceitos por `--agent`. Cursor,
   Gemini CLI, Droid e os outros agentes sem armazenamento a ser procurado são
   configurados nomeando-os. O OpenCode 1 e 2 compartilham um armazenamento de sessões, portanto, um
   `opencode` detectado ou nomeado executa `opencode --version` uma vez e configura o
   plugin e a entrada MCP para a versão principal que ele relatar, como `install-mcp --client
   opencode` e `install-hooks --agent opencode` fazem manualmente. Quando essa sondagem
   falha (nenhum `opencode` no `PATH`, um timeout ou saída ilegível), o `setup`
   não grava nenhum plugin ou entrada MCP do OpenCode e relata o problema; `--agent
   opencode2` força o V2 sem sondagem. O motor do Kiro CLI v3 compartilha um
   armazenamento com o Kiro CLI, portanto, ele só é configurado quando nomeado.
4. Um autoteste que não grava nada: `GET /healthz`, uma chamada `memory_status`
   via MCP com a credencial da instalação e `doctor`. Quando o Codex foi configurado,
   o `setup` também pergunta ao Codex se ele executará os hooks do ai-memory. Se o Codex
   ainda não confiar neles, a linha de encerramento informa isso e diz para você abrir
   o Codex, executar `/hooks` e confiar neles, em vez de relatar a instalação como
   pronta. Isso ainda encerra com 0, porque somente você pode conceder a confiança no Codex.

Sem `--yes`, o `setup` imprime esse plano e pergunta em um terminal. `y` aplica-o.
Com mais de um agente no plano, você pode digitar os números
dos agentes a serem configurados (`1 3` ou `1,3`); todas as outras etapas ainda são executadas, e uma resposta
indicando um número fora do plano não altera nada. Sem terminal (um script,
a chamada de ferramenta de um agente), ele não altera nada e encerra com código diferente de zero, solicitando
`--yes`. `--plan` imprime o plano e encerra sem alterar nada;
`--plan --json` imprime-o como um documento JSON para um script ou front-end, com um
campo `format` que só é incrementado quando um campo altera de significado ou é removido.

`ai-memory setup --web` mostra o mesmo plano como uma página no seu navegador em vez
do terminal. Ele imprime um endereço de uso único em `127.0.0.1` (e o abre,
a menos que `--no-open` ou não haja desktop), onde você marca os agentes a serem configurados
e clica em Apply. Apply executa `ai-memory setup --yes --agent <each one you
picked>` e transmite sua saída para a página, de modo que o resultado seja o mesmo que o
comando de terminal; `--no-service` é repassado. A página responde apenas
a solicitações que informem seu próprio endereço, portem a parte secreta da URL e,
para uma alteração, venham da própria página, de modo que outro site aberto no mesmo
navegador não possa controlá-la. Ela não mantém nenhum token. Ela para quando você clica em Finish,
após 15 minutos sem solicitação ou com Ctrl-C. Em uma máquina remota,
encaminhe a porta impressa (`ssh -L <port>:127.0.0.1:<port> <host>`) e abra
o mesmo endereço localmente. Cada etapa é idempotente: uma segunda execução não reescreve nenhum
arquivo e mantém um serviço em execução intocado. Uma etapa que falha não interrompe as
outras; o `setup` lista cada `problem:` no final e encerra com código diferente de zero. Quando todas as etapas
forem aprovadas, ele termina nomeando os agentes a serem reiniciados e a pergunta a ser feita a um
deles, "O que você lembra sobre este projeto?": responder a isso exige uma chamada
às ferramentas do ai-memory, portanto, a resposta demonstra que a configuração funciona. O token
nunca é impresso nem passado em linha de comando, e nenhuma configuração de harness o mantém:
os hooks leem os arquivos de credenciais que o `init` grava, e os clientes MCP alcançam o
servidor por meio do `ai-memory mcp-bridge`.

O serviço serve a interface web em `http://127.0.0.1:49374/web`, e o `setup`
imprime esse endereço no final. Uma instalação nova tem um token bearer e nenhuma conta
de usuário, portanto, o navegador solicita um nome de usuário e senha: digite qualquer nome de usuário,
e o conteúdo de `<data_dir>/auth-tokens/default` como a senha. Para entrar
com uma senha própria, crie a primeira conta root conforme descrito em
[root pela primeira vez](#primeiro-acesso-do-root-greenfield).

`uninstall --apply` remove o que o `setup` configurou, incluindo o serviço. Toda
unidade gerada pelo `setup` contém uma linha de identidade contendo o SHA-256 do restante
do arquivo, e o `uninstall` remove apenas uma unidade cuja linha esteja presente e ainda
coincida: no Linux ele executa `systemctl --user disable --now ai-memory.service`,
exclui o arquivo e executa `systemctl --user daemon-reload`; no macOS ele descarrega
o LaunchAgent e exclui o plist. Uma unidade que o `setup` não gravou, ou uma
editada desde então, é mantida e o `uninstall` avisa isso. Se precisar remover
essa unidade, ou se o gerenciador de serviços recusou, faça manualmente:

```bash
# Linux
systemctl --user disable --now ai-memory.service
rm ~/.config/systemd/user/ai-memory.service
systemctl --user daemon-reload
# macOS
launchctl bootout "gui/$(id -u)/com.github.akitaonrails.ai-memory"
rm ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist
```

Um agente de código instalando o ai-memory para seu usuário pode seguir
[`INSTALL_FOR_AGENTS.md`](../../INSTALL_FOR_AGENTS.md), que se resume a
`ai-memory setup --yes --agent <self>`.

`scripts/onboarding-acceptance.sh` avalia esse caminho em uma home temporária
limpa em relação a um release compilado localmente: dois comandos e cerca de um segundo do
primeiro comando até um `memory_query` que retorna um evento de hook capturado (uma
compilação de depuração em uma estação de trabalho Linux, com embeddings locais desativados para que nenhum download de modelo
seja contabilizado). A meta é 60 segundos.

## Plugins de marketplace

A partir da versão 3.0, este repositório também é um marketplace de plugins. Um plugin configura um
agente em uma única etapa: hooks de ciclo de vida, ferramentas MCP do ai-memory, as skills
de roteamento e uma skill `status` que executa `ai-memory doctor` e o resume.
É a alternativa para `install-hooks` + `install-mcp` + `install-skills`
para esse agente; o `ai-memory setup` faz o restante (diretório de dados, serviço, outros
agentes) de qualquer forma.

O plugin não contém o ai-memory. Instale o binário primeiro (acima, ou
Homebrew, AUR, RPM, o wrapper Docker, o app macOS) e depois adicione o plugin.
Os plugins exigem o ai-memory 3.0 ou posterior: um binário mais antigo exige
`--server-url` em cada hook, portanto, o inicializador tenta novamente contra
`http://127.0.0.1:49374` com uma dica de upgrade de uma linha no stderr, e a entrada
MCP não se conecta até que o binário seja atualizado.
Cada hook executa `scripts/ai-memory-hook`, um script POSIX curto que localiza
o binário e entrega o evento a ele com `ai-memory hook`, o mesmo comando nativo
que o `install-hooks` configura. A política de captura (`ignore_paths`, modo
allowlist), o escopo do marcador e os perfis de servidor se aplicam exatamente como
para hooks em arquivos de configurações. Sem o binário, o plugin não captura nada,
imprime uma linha no início da sessão e nunca bloqueia o agente. Os plugins
não têm telemetria e não mantêm credenciais: os hooks e a ponte MCP usam as
credenciais que a CLI já armazena (`ai-memory setup`, `auth login`).

Os catálogos fixam cada plugin na tag de release, de modo que uma adição do marketplace apenas
instale conteúdo de plugin lançado. Os arquivos gerados ficam sob `plugins/`
(consulte [`plugins/README.md`](../../plugins/README.md)).

### Claude Code

```sh
claude plugin marketplace add akitaonrails/ai-memory
claude plugin install ai-memory@ai-memory
```

(ou `/plugin marketplace add akitaonrails/ai-memory` e `/plugin install
ai-memory@ai-memory` dentro do Claude Code). O plugin funciona na CLI e na
aba Code do aplicativo desktop. As opções, todas opcionais, são definidas com `/plugin
configure ai-memory@ai-memory`:

| Opção | Padrão | Uso |
| --- | --- | --- |
| `server_url` | vazio | Vazio usa o `server_url` da sua configuração do ai-memory, para que o plugin e a CLI nunca discordem (o inicializador omite `--server-url`). Defina-o apenas para apontar este plugin para outro local. |
| `ai_memory_bin` | `ai-memory` | Caminho absoluto para o binário quando ele não estiver no `PATH` visto pelo app; um caminho relativo é ignorado, pois resolveria dentro do checkout. O inicializador também tenta `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin` e `/usr/bin`. |
| `data_dir` | vazio | Apenas para uma instalação com diretório de dados não padrão. |

- O Claude Code não atualiza automaticamente um marketplace de terceiros
  a menos que você ative isso. O `ai-memory upgrade` executa `claude plugin
  marketplace update ai-memory` e `claude plugin update ai-memory@ai-memory`
  após substituir o binário; com um binário gerenciado por pacote (Homebrew, AUR,
  RPM), execute esses dois comandos você mesmo após a atualização do pacote.
- Se o `install-hooks` já adicionou hooks do ai-memory em
  `~/.claude/settings.json` ou no `.claude/settings.local.json` de um checkout,
  eles têm precedência: o plugin ignora cada evento que eles entregarem, para que nada
  seja capturado duas vezes. Apenas um hook com matcher vazio ou `*` conta; um que
  você restringiu a algumas ferramentas deixa o plugin capturar as outras. O `ai-memory doctor` relata a instalação dupla e qual
  cópia remover. Enquanto o plugin estiver habilitado, o `ai-memory setup` e
  o `ai-memory run` param de gravar hooks nas configurações, entrada MCP e
  skills do Claude; execute `ai-memory install-hooks --agent claude-code --apply` (ou
  `install-mcp`, `install-skills`) para adicioná-los de qualquer maneira.
- Sessões sem pasta (um espaço de trabalho temporário no desktop sem pasta de projeto)
  não são capturadas através do plugin.
- Dez eventos são configurados: `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `PostToolUseFailure`, `PreCompact`, `Stop`, `SessionEnd`,
  `SubagentStart` e `SubagentStop`. O Claude Code dispara `PostToolUseFailure`
  *em vez de* `PostToolUse` quando uma ferramenta falha (saída diferente de zero, erro
  disparado, resultado de erro MCP). A observação registra `outcome: error` e
  o texto do erro, delimitado como qualquer trecho de ferramenta e nunca os
  argumentos da ferramenta; a página da sessão conta chamadas com falha e cita a mais recente;
  o handoff automático adiciona uma linha `Last failed tool call`. Uma instalação feita
  antes desta correção tem nove eventos; reexecute `ai-memory install-hooks --agent
  claude-code --apply` (ou `ai-memory setup`) para adicionar o décimo.
- O Factory Droid pode instalar este plugin por meio do seu fallback
  do marketplace do Claude. Seus eventos são gravados como `droid`, e quando os hooks de
  `install-hooks --agent droid` entregam o mesmo evento (`~/.factory/hooks.json`, ou
  a chave `hooks` de `~/.factory/settings.json` enquanto `hooks.json` não
  existir), o plugin o ignora, assim como faz para os hooks de configurações do Claude.
- O Meta Muse Code lê pacotes `.claude-plugin/plugin.json`, mas o Muse Code
  1.4.7 recusa-se a instalar este (`plugin declares a retained unsupported
  MCP boundary`) porque seu `.mcp.json` usa marcadores
  `${user_config.*}`; use `install-hooks --agent muse` e `install-mcp --client
  muse` em vez disso. O inicializador está pronto para um Muse que o carregue: ele distingue o Muse
  pelo `MUSE_PLUGIN_ID` e `MUSE_SESSION_ID` que o Muse define para hooks
  de plugin e grava os eventos como `muse`, ignora um evento que os hooks de
  `install-hooks --agent muse` já entregam (um matcher genérico
  sob a chave `hooks` de `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`)
  e descarta os eventos `SubagentStart` e `SubagentStop` do plugin, os quais
  o `install-hooks --agent muse` nunca configura.
- No Windows, o Claude Code executa hooks de plugin via Git Bash, o qual
  já é exigido por ele, de modo que o inicializador não precisa de mais nada.
- Com o wrapper Docker, os hooks de plugin precisam de um wrapper `bin/ai-memory` que execute
  `ai-memory hook` por meio do seu cliente nativo do host, não em um contêiner
  (caso contrário, cada evento incorre em um `docker run` e `127.0.0.1` aponta para dentro
  do contêiner). Execute `ai-memory upgrade` para obter o wrapper com esse
  roteamento.
- Cada release anexa
  `ai-memory-claude-plugin.zip` para **Customize > Plugins > Upload plugin**.
  No Cowork, a captura é executada no seu computador e precisa do ai-memory instalado
  nele; as ferramentas de memória ainda funcionam.
- Remova-o com `claude plugin uninstall ai-memory@ai-memory`;
  o `ai-memory uninstall` também faz isso.

### Codex

```sh
codex plugin marketplace add akitaonrails/ai-memory
codex plugin add ai-memory@ai-memory
```

Em seguida, abra o Codex, execute `/hooks` e confie nos hooks do ai-memory. O Codex executa os
hooks de um plugin apenas depois que você confiar neles, e ignora um hook não confiável sem
mensagem. A confiança é baseada no comando do hook antes que `${PLUGIN_ROOT}` seja
expandido, e os comandos permanecem idênticos entre releases, portanto, uma atualização
de plugin a mantém; um release que precise alterar um comando avisa isso no
changelog. O `ai-memory doctor` e a skill `status` relatam hooks que o Codex
ignora. O plugin do Codex não tem opções: a URL do servidor e as credenciais vêm
da sua configuração do ai-memory, e a entrada MCP executa `ai-memory mcp-bridge
--no-session-id` a partir do `PATH`. No Windows, os hooks chamam `ai-memory hook`
diretamente (`commandWindows`), portanto, o `ai-memory` deve estar no `PATH`. Essa chamada
direta é uma limitação conhecida: ela ignora o inicializador, de modo que um evento no Windows não é
marcado como iniciado por plugin (sessões sem pasta são capturadas, como em um
hook de configuração), e sem o binário o Codex relata um hook com falha em vez
da operação silenciosa do inicializador. Alterar isso também alteraria o comando
em que o Codex baseia a confiança do hook.

O Codex executa hooks de plugin e hooks de `~/.codex/hooks.json` lado a lado, e um
hook de configuração não confiável é ignorado silenciosamente, portanto, o plugin não pode descartar
com segurança sua cópia da mesma forma que o plugin do Claude Code faz. Mantenha um: o `ai-memory setup`
não adiciona hooks de configuração do Codex enquanto o plugin estiver ativado, e o `doctor`
sinaliza uma instalação dupla. Para atualizar, execute `codex plugin marketplace upgrade
ai-memory` e `codex plugin add ai-memory@ai-memory` (o `ai-memory upgrade`
os imprime). Remova com `codex plugin remove ai-memory@ai-memory` ou
`ai-memory uninstall`.

## Servidor em uma máquina diferente

Quando o servidor ai-memory roda em uma máquina na LAN (homelab, servidor headless)
e você usa Claude Code / Codex / etc. em um laptop:

### Lado do servidor (o host do homelab)

```bash
docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 0.0.0.0:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
    -e AI_MEMORY_LLM_PROVIDER=anthropic \
    -e ANTHROPIC_API_KEY=sk-ant-... \
    akitaonrails/ai-memory:latest
```

Consulte [Segurança](../../README.md#security) no README para saber por que
`AI_MEMORY_AUTH_TOKEN` e `AI_MEMORY_ALLOWED_HOSTS` são ambos obrigatórios para
ligações normais que não sejam de loopback. A autenticação bearer não criptografa o tráfego: use os modelos
prontos de [Caddy](../../docker/compose.tls.caddy.yml) ou
[Cloudflare Tunnel](../../docker/compose.tls.cloudflared.yml) do
[guia de proxy reverso HTTPS](https-via-proxy.md) para acesso na LAN ou remoto.
Quando o proxy servir `/web` via HTTPS, defina também
`AI_MEMORY_AUTH__SECURE_COOKIE=true` no ambiente do servidor e feche ou
redirecione o acesso HTTP direto para esse hostname. Não o defina para HTTP direto:
os navegadores reterão corretamente o cookie da sessão nesse caso.

### Lado do cliente (o laptop)

```bash
export AI_MEMORY_SERVER_URL="http://<server-ip>:49374"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"

ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

Isso configura os hooks para todo o usuário (`~/.claude/settings.json`), para que cada sessão
do Claude Code seja capturada. Para optar por repositório, execute de dentro
do checkout:

```bash
ai-memory install-hooks --agent claude-code --scope project --apply
```

Isso grava o `.claude/settings.local.json` ignorado pelo git na raiz do repositório
git (a raiz do checkout principal a partir de um worktree), que é onde o Claude Code
lê esse arquivo mesmo quando iniciado a partir de um subdiretório (consulte
[onde o Claude Code procura por cada arquivo](https://code.claude.com/docs/en/settings#where-claude-code-looks-for-each-file)).
No Windows, e quando a raiz do repositório for seu diretório home, o Claude Code
lê o diretório de inicialização em vez disso, portanto, o instalador grava no diretório atual
nesse caso: execute-o, e inicie o Claude Code, a partir do mesmo diretório.
O arquivo no nível de usuário permanece intocado. O Claude Code mescla os hooks do
arquivo de projeto com quaisquer outros de nível de usuário, portanto escolha um escopo por máquina; o instalador
avisa quando o outro escopo já contém hooks do ai-memory.
O instalador avisa quando o arquivo não está ignorado pelo git (o Claude Code adiciona
`**/.claude/settings.local.json` às suas exclusões globais apenas quando ele mesmo cria
o arquivo) e recusa-se a incorporar um token bearer nele quando o token
não puder ser persistido sob o diretório de dados. Quando ele atualiza um arquivo existente, o
backup vai para `<data_dir>/backups/claude-settings-local/` (somente proprietário), não
ao lado do arquivo, para que nada novo apareça em `git status`.
`ai-memory uninstall --only hooks --apply` executado de dentro do checkout
remove as entradas novamente; executado em outro lugar, ele não acessa o
repositório, portanto, execute-o em cada checkout com escopo de projeto antes de remover
o ai-memory. Caso contrário, esses arquivos continuarão chamando o binário e diretório
de dados removidos. O `upgrade` e a autoconfiguração do `ai-memory run` mantêm o arquivo de nível de usuário
intocado enquanto uma instalação com escopo de projeto for a única presente. O `upgrade`
substitui o binário chamado pelos comandos de hook, mas não reescreve arquivos de
projeto; reexecute `install-hooks --scope project --apply` em cada checkout para incorporar
alterações nos comandos de hook. `setup-agent` e `backup-agents` conhecem apenas o
arquivo de nível de usuário. Apenas Claude Code; outros agentes mantêm seus arquivos
de hook de nível de usuário.

`--session-aware` é um modo MCP opcional do Claude Code:

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

Ele substitui a entrada MCP HTTP estática por uma ponte stdio local do ai-memory que
ainda se conecta ao servidor remoto e token bearer configurados, enquanto
encaminha o id de sessão de ciclo de vida do Claude. Em uma instalação com wrapper Docker, o
wrapper executa a ponte por meio de seu cliente de host nativo verificado por checksum, para que
a URL de servidor de loopback configurada seja resolvida no host. Combine-o com
`[auto_scope] mode = "per_session"` quando o mesmo operador executar sessões simultâneas
do Claude Code em projetos diferentes. O registro MCP HTTP
estático padrão continua adequado para um projeto ativo por vez.

Se `CLAUDE_CONFIG_DIR` estiver definido, os instaladores do claude-code acompanham a
resolução de configuração do próprio Claude Code: `install-mcp` grava o registro MCP em
`$CLAUDE_CONFIG_DIR/.claude.json` (em vez de `~/.claude.json`),
`install-hooks` / `setup-agent` direcionam para `$CLAUDE_CONFIG_DIR/settings.json`
(em vez de `~/.claude/settings.json`), e `install-skills --scope global`
usa `$CLAUDE_CONFIG_DIR/skills` (em vez de `~/.claude/skills`). `uninstall`
varre os caminhos realocados ativos junto com os padrões da home. Ele não consegue
descobrir um `CLAUDE_CONFIG_DIR` arbitrário mais antigo que não esteja mais definido.
`install-hooks --scope project` ignora a variável: o Claude Code resolve
arquivos de projeto a partir do checkout, não do diretório de configuração. O
wrapper Docker encaminha a variável para raízes de configuração sob sua montagem
bind `$HOME` existente; use o binário nativo quando a raiz realocada estiver fora
de `$HOME`.

Os comandos da CLI (`bootstrap`, `status`, `search`, `lint`, `auto-improve`,
`curator`, `pending-writes`, etc.) herdam as duas variáveis de ambiente automaticamente. O mesmo vale para
`install-mcp`, `install-hooks` e
`setup-agent`: com `AI_MEMORY_SERVER_URL` definido, `install-mcp` deriva o
endpoint `/mcp` e `install-hooks` usa a origem básica do servidor.

Após atualizar o ai-memory, atualize o pacote de roteamento gerenciado em projetos
existentes para que Claude Code/OpenCode/Codex/Gemini obtenham as novas diretrizes de ferramentas e
regras de recuperação proativa. A partir de um agente, solicite "atualize o roteamento do ai-memory
neste projeto"; a partir do terminal, execute `ai-memory install-instructions` (ou passe
`--target AGENTS.md` para arquivos de instruções que não sejam do Claude). A atualização é idempotente:
trechos longos legados entre `<!-- ai-memory:start -->` /
`<!-- ai-memory:end -->` são substituídos no local pelo trecho enxuto, e
as Agent Skills gerenciadas são instaladas ou atualizadas ao lado dele.

Se você instalar no `AGENTS.md` e o projeto também for usado a partir do Claude Code,
faça o `CLAUDE.md` importá-lo com uma primeira linha simples contendo `@AGENTS.md`. O Claude Code
2.1.277 e posterior lê `AGENTS.md` apenas quando nenhum `CLAUDE.md`,
`.claude/CLAUDE.md` ou `CLAUDE.local.md` existir no diretório de trabalho ou em
qualquer diretório acima dele. Sem essa importação, sessões que leem apenas
`CLAUDE.md` (Claude Code mais antigo, plugin AGENTS.md desativado, a configuração de
instruções de projeto `claude-md` ou o próprio `CLAUDE.local.md` de um desenvolvedor) perdem o
bloco instalado no início da sessão. Consulte
[Claude Code memory](https://code.claude.com/docs/en/memory#agents-md).

---

## Configurando a URL da CLI e a autenticação

O binário `ai-memory` é um cliente leve HTTP. Ele nunca abre o wiki
ou o SQLite diretamente; comandos que alteram estado passam pelo servidor
em execução, que é o único gravador.

A configuração consiste em duas variáveis de ambiente opcionais:

| Variável | Padrão | Quando definir |
|---|---|---|
| `AI_MEMORY_SERVER_URL` | `http://127.0.0.1:49374` | Quando o servidor é executado em outro lugar que não a mesma máquina, como `http://192.168.0.90:49374`. |
| `AI_MEMORY_AUTH_TOKEN` | não definido | Quando o servidor tem autenticação bearer ativada. |

Para um servidor loopback em um único laptop, não defina nenhuma variável. Para um
servidor remoto ou homelab, coloque ambas no seu arquivo rc de shell ou direnv:

```bash
export AI_MEMORY_SERVER_URL="http://192.168.0.90:49374"
export AI_MEMORY_AUTH_TOKEN="<token>"
```

As flags explícitas `--server-url` e `--auth-token` em `install-mcp`,
`install-hooks` e `setup-agent` substituem o ambiente. Isso é
útil quando você está gerando configuração para um cliente que conversa com
um servidor diferente do destino padrão da sua CLI.

Se você executar `install-mcp --apply` primeiro e depois executar `install-hooks --apply`
sem variáveis de ambiente ou flags, os hooks reutilizam a entrada MCP do ai-memory existente para
esse agente quando possível. Isso mantém a configuração MCP remota e a captura de ciclo de vida
apontadas para o mesmo servidor em vez de recorrer ao loopback.

Todos os modos `--apply` do instalador preservam arquivos de configuração em symlink: a
atualização atômica é gravada no destino do symlink, incluindo um destino final ausente,
enquanto o backup com carimbo de data/hora permanece ao lado do caminho de configuração
visível ao usuário. Isso mantém instalações gerenciadas por dotfiles com stow, chezmoi e similares vinculadas.

`init`, `serve` e `generate-auth-token` não precisam dessas variáveis de ambiente porque
eles criam arquivos locais ou iniciam o próprio servidor.

### Resolução de projeto padrão (`--project-strategy`)

Por padrão, um checkout não declarado com um remote `upstream` ou `origin` válido
usa o nome de projeto canônico baseado no caminho do repositório; os clientes atuais enviam essa
escolha explicitamente para segurança de atualização. Repositórios sem um remote válido usam
`basename(cwd)`, inclusive a partir de um subdiretório git. Como o shell de um agente mantém seu diretório de trabalho entre chamadas de ferramentas, um único
`mkdir sub && cd sub` reatribui o restante da sessão a um projeto fantasma
chamado `sub`. Para fazer com que cada sessão de uma instalação resolva seu projeto a partir da
raiz do repositório git em vez disso, agrupando subdiretórios e worktrees, fixe a
estratégia nos hooks:

```bash
ai-memory install-hooks --apply --agent claude-code --project-strategy repo-root
```

`--project-strategy` aceita `basename` (o padrão em novas instalações; não fixa nada)
ou `repo-root`. Omiti-lo durante um `--apply` posterior preserva a estratégia
já gravada nos hooks do ai-memory daquele agente, inclusive durante a atualização
automática pós-upgrade do wrapper. Passe `basename` explicitamente para remover um
padrão `repo-root` existente. Isso funciona para qualquer agente e caminho de entrega. A
própria configuração de `project_strategy` / `project` do marcador `.ai-memory.toml` por repositório
ainda tem precedência; consulte
[a referência do arquivo de marcador](marker-file.md#padrão-para-toda-a-instalação-sem-marcador).

---

## Homebrew (macOS e Linux)

A fórmula `akitaonrails/tap` instala o binário da release e as fontes de hook dele
para macOS (Apple Silicon e Intel) e Linux (x86_64 e aarch64).
No Linux ela funciona com o Homebrew no Linux, que é como distribuições atômicas
como Bazzite e Fedora Silverblue costumam adicionar ferramentas de linha de comando.

```bash
brew install akitaonrails/tap/ai-memory
ai-memory setup
```

O `ai-memory setup` cria o diretório de dados e seu bearer token, instala
e inicia seu próprio serviço de usuário (um LaunchAgent no macOS, uma unidade de usuário do systemd
no Linux), configura os agentes que encontra e verifica o resultado. A fórmula também
define uma entrada de `brew services`. Use uma ou outra, não ambas: com
`brew services start ai-memory`, execute `ai-memory init` primeiro e configure os
agentes com `ai-memory setup --no-service`. O `brew upgrade ai-memory` substitui
o binário em um caminho versionado do Cellar, de modo que um serviço iniciado por
`ai-memory setup` deve apontar para o caminho estável `$(brew --prefix)/bin/ai-memory`
(veja a nota sobre locais versionados de pacotes acima).

## Fedora (RPM)

Baixe o RPM `x86_64` ou `aarch64` do
[release mais recente do GitHub](https://github.com/akitaonrails/ai-memory/releases/latest)
e instale-o com `sudo dnf install ./ai-memory-*.rpm`. O pacote inclui
o binário, fontes dos hooks, unidades systemd de sistema e de usuário, e a configuração
do serviço de sistema. Em uma máquina de usuário único, execute `ai-memory setup`, como na
[instalação nativa em uma etapa](#instalação-nativa-em-uma-etapa-installsh-ai-memory-setup).
Para o caminho manual, siga as etapas de [serviço em nível de usuário](#serviço-em-nível-de-usuário) ou
[serviço de sistema](#serviço-em-nível-de-sistema) abaixo.

## Pacotes nativos do Arch Linux (AUR)

Use os pacotes nativos quando desejar `/usr/bin/ai-memory` com unidades systemd
em vez do wrapper Docker. O pacote instala o binário e as fontes de hooks
uma vez; cada usuário ainda prepara seus scripts de hook de agente em seu próprio diretório home
com `install-hooks --apply`.

### Escolha do pacote

```bash
yay -S ai-memory-bin    # prebuilt Linux x86_64/aarch64 binary, fastest install
yay -S ai-memory        # builds from source, works on x86_64 and aarch64
```

Ambos os pacotes instalam o mesmo layout de tempo de execução:

| Caminho | Finalidade |
|---|---|
| `/usr/bin/ai-memory` | Binário nativo de CLI/servidor. |
| `/usr/share/ai-memory/hooks/` | Pacote de fontes de hooks empacotado usado por `install-hooks`. |
| `/usr/lib/systemd/system/ai-memory.service` | Unidade de serviço de todo o sistema. |
| `/usr/lib/systemd/user/ai-memory.service` | Unidade de serviço por usuário. |
| `/usr/lib/sysusers.d/ai-memory.conf` | Cria o usuário de sistema `ai-memory`. |
| `/usr/lib/tmpfiles.d/ai-memory.conf` | Cria `/var/lib/ai-memory` para o serviço de sistema. |
| `/etc/ai-memory/config.toml` | Arquivo de configuração do serviço de sistema, rastreado como backup do pacman. |
| `/etc/ai-memory/env` | Arquivo de variáveis/segredos do serviço de sistema, rastreado como backup do pacman. |

O binário em si não faz suposições entre modo de sistema e de usuário. O arquivo de unidade
escolhe explicitamente:

| Modo | Diretório de dados | Configuração | Env/segredos | Requer sudo? |
|---|---|---|---|---|
| Serviço de usuário | `~/.local/share/ai-memory` | `~/.config/ai-memory/config.toml` | `~/.config/ai-memory/env` | Não |
| Serviço de sistema | `/var/lib/ai-memory` | `/etc/ai-memory/config.toml` | `/etc/ai-memory/env` | Sim |

Não execute ambos os serviços no mesmo endereço de bind. Eles podem coexistir em disco, mas
apenas um pode escutar em `127.0.0.1:49374`, a menos que você altere `bind` em uma das configurações.

### Serviço em nível de usuário

Use isto em uma estação de trabalho de usuário único. Não requer sudo após a instalação do pacote e
mantém todo o estado em seu diretório home.

O caminho mais curto é `ai-memory setup`, que faz tudo abaixo e configura
seus agentes também (consulte a
[instalação nativa em uma etapa](#instalação-nativa-em-uma-etapa-installsh-ai-memory-setup)).
Ele grava sua própria unidade em `~/.config/systemd/user/ai-memory.service`, que
tem precedência sobre a empacotada de mesmo nome, com a configuração em
`~/.local/share/ai-memory/config.toml`. As etapas manuais abaixo usam a unidade
empacotada e seus caminhos em vez disso.

```bash
mkdir -p ~/.config/ai-memory ~/.local/share/ai-memory
ai-memory \
  --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml \
  init
```

O `init` em um diretório de dados novo já ativou a autenticação bearer (o token está em
`auth-tokens/default` sob o diretório de dados). Edite as configurações de provedor se
quiser consolidação com LLM:

```bash
$EDITOR ~/.config/ai-memory/config.toml
$EDITOR ~/.config/ai-memory/env
```

O `init` em um diretório de dados novo também grava um token bearer root aleatório em
`~/.local/share/ai-memory/auth-tokens/default` (somente proprietário) e registra
`bearer_token_file = "auth-tokens/default"` na configuração, de modo que o serviço
o exija em todas as rotas, exceto `/healthz`. Você nunca o copia manualmente:
`install-hooks --apply` e `install-mcp --apply` o encontram no diretório de dados,
os hooks o leem de lá, e o Claude Code é registrado como a
ponte stdio `ai-memory mcp-bridge`, que lê o arquivo na inicialização. O Codex e o
Prime Agent referenciam `$AI_MEMORY_AUTH_TOKEN` em vez disso; exporte-o onde você
os inicia:

```bash
export AI_MEMORY_AUTH_TOKEN="$(cat ~/.local/share/ai-memory/auth-tokens/default)"
```

Todos os outros clientes aceitos por `install-mcp` recebem uma entrada stdio local que
executa `ai-memory mcp-bridge --no-session-id`, que também lê o arquivo de token
na inicialização, de modo que `ai-memory` deve estar no `PATH` visto pelo cliente. O ZCode aceita
apenas entradas HTTP e é recusado; adicione sua entrada manualmente com o cabeçalho se
você aceitar o token em sua configuração.

Uma instalação criada antes desse padrão continua em execução sem autenticação.
O `ai-memory status` exibe então `auth: none (loopback)`; execute
`ai-memory auth enable` e reinicie o serviço para ativar o token.

Inicie e inspecione o serviço:

```bash
systemctl --user daemon-reload
systemctl --user enable --now ai-memory.service
systemctl --user status ai-memory.service
journalctl --user -u ai-memory.service -f
```

Se o serviço deve continuar em execução após você se desconectar:

```bash
loginctl enable-linger "$USER"
```

Verifique o servidor HTTP:

```bash
curl -s http://127.0.0.1:49374/healthz
# Expect {"status":"ok"}. /healthz needs no token; `curl -sI http://127.0.0.1:49374/mcp`
# answers 401 without one.
```

### Serviço em nível de sistema

Use isto para uma estação compartilhada, máquina na LAN ou host estilo homelab onde o
servidor deve ser executado independentemente de qualquer usuário conectado.

Certifique-se de que o usuário e diretório de estado criados pelo pacote existam e, em seguida, inicialize
o layout de dados como aquele usuário do serviço:

```bash
sudo systemd-sysusers /usr/lib/sysusers.d/ai-memory.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/ai-memory.conf
sudo -u ai-memory ai-memory \
  --data-dir /var/lib/ai-memory \
  --config /etc/ai-memory/config.toml \
  init
```

Edite a configuração do sistema e segredos:

```bash
sudoedit /etc/ai-memory/config.toml
sudoedit /etc/ai-memory/env
```

O pacote instala `/etc/ai-memory/env` com permissão de leitura apenas para root porque ele pode
conter chaves de API. Mantenha esse arquivo fora de backups ou logs que outros usuários possam ler.

O `init` também gravou o token bearer root em
`/var/lib/ai-memory/auth-tokens/default`, legível apenas pelo usuário
`ai-memory`. Para conectar os agentes do seu próprio login a este servidor, copie-o para o
diretório de dados do seu usuário uma vez; `install-mcp` e `install-hooks` o encontrarão lá:

```bash
install -d -m 0700 ~/.local/share/ai-memory/auth-tokens
sudo cat /var/lib/ai-memory/auth-tokens/default \
  | install -m 0600 /dev/stdin ~/.local/share/ai-memory/auth-tokens/default
```

Para exposição na LAN, defina um bind que não seja de loopback e hosts permitidos em
`/etc/ai-memory/config.toml`. O bearer gravado pelo `init` já protege todas as
rotas; defina `AI_MEMORY_AUTH_TOKEN` em `/etc/ai-memory/env` apenas para substituí-lo:

```toml
bind = "0.0.0.0:49374"
allowed_hosts = ["homelab", "192.168.0.90", "localhost", "127.0.0.1"]
```

`server_url` não precisa ser definido aqui. Quando nem `config.toml` nem
`AI_MEMORY_SERVER_URL` informarem um, os comandos de cliente leve o derivam de
`bind`, para que um `ai-memory status` local alcance este servidor em vez da
porta padrão. Um bind não especificado é mapeado para loopback (`0.0.0.0` torna-se
`127.0.0.1`, `[::]` torna-se `[::1]`). Defina `server_url` apenas quando clientes
alcançarem o servidor em um endereço ao qual ele não faz bind, como atrás de um proxy
reverso ou túnel:

```toml
server_url = "https://memory.example"
```

```bash
# optional: replace the token init wrote
TOKEN=$(ai-memory generate-auth-token)
printf 'AI_MEMORY_AUTH_TOKEN=%s\n' "$TOKEN" | sudo tee -a /etc/ai-memory/env
```

Inicie e inspecione o serviço:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now ai-memory.service
sudo systemctl status ai-memory.service
journalctl -u ai-memory.service -f
```

Verifique a partir do host:

```bash
curl -sI http://127.0.0.1:49374/handoff
# 401 Unauthorized without the bearer token.
```

### Login no provedor LLM com serviços nativos

> **Você não precisa de uma chave de API de plataforma paga.** Os recursos de LLM do ai-memory
> (consolidação, lint, auto-aprimoramento) são opcionais e, ao ativá-los, você
> pode autenticar com uma **assinatura que você já paga** em vez de uma
> chave de API tarifada: um plano ChatGPT Plus/Pro/Codex via `openai-oauth`
> (`ai-memory auth login openai-oauth`) ou um plano GitHub Copilot via `copilot`
> (`ai-memory auth login copilot`). (Claude Pro/Max via `anthropic-oauth` é
> atualmente recusado pela Anthropic; consulte
> [abaixo](#anthropic-via-claude-subscription-oauth).) Consulte
> [`docs/llm-providers.md`](llm-providers.md) para a tabela completa. E você pode
> dispensar totalmente um LLM: o caminho padrão zero-LLM ainda captura, busca
> (FTS) e grava resumos baseados em regras sem nenhum provedor, e
> [`docs/local-embeddings.md`](local-embeddings.md) torna a busca vetorial
> sem chave também.

Provedores com chave de API vão no arquivo env correspondente:

```bash
# User service
printf 'AI_MEMORY_LLM_PROVIDER=anthropic\nANTHROPIC_API_KEY=sk-ant-...\n' >> ~/.config/ai-memory/env
systemctl --user restart ai-memory.service

# System service
sudoedit /etc/ai-memory/env
sudo systemctl restart ai-memory.service
```

Provedores estilo OAuth gravam tokens no diretório de dados selecionado. Execute o login
com o mesmo par de `--data-dir` e `--config` do serviço:

```bash
# User service
ai-memory \
  --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml \
  auth login openai-oauth

# System service
sudo -u ai-memory ai-memory \
  --data-dir /var/lib/ai-memory \
  --config /etc/ai-memory/config.toml \
  auth login openai-oauth
```

Use `auth login copilot` da mesma forma para o GitHub Copilot. Para autenticação
nativa de hooks por desenvolvedor contra um emissor OIDC, execute `auth login oidc-device` no
diretório de dados selecionado pelo desenvolvedor em vez disso:

```bash
ai-memory auth login oidc-device \
  --issuer "https://issuer.example.com/realms/team" \
  --client-id "ai-memory-cli"
```

O emissor, e os endpoints de dispositivo e token listados em seu documento de descoberta,
devem usar `https`. O `http` simples é aceito apenas em `localhost`, `127.0.0.1`
e `::1`, para um emissor de teste local. As solicitações de autenticação não seguem
redirecionamentos, e a renovação revalida o endpoint de token armazenado por releases mais antigos
antes de enviar um refresh token.

O access token OIDC armazenado também é usado pelos comandos HTTP de cliente leve
(`status`, `search`, `read-page`, `write-page`, `backup`, `embed` e
semelhantes) quando nenhum `AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` estático estiver
configurado. A autenticação bearer estática ainda tem precedência. Isso se destina a
gateways/pontes externos com reconhecimento de OIDC; a autenticação nativa do servidor ai-memory ainda usa
tokens estáticos de root bearer / usuário do banco de dados, e `/admin/*` permanece exclusivo para root, a menos que um gateway
traduza a autenticação OIDC aceita em autenticação upstream que o ai-memory aceite.

As claims `sid` de OIDC/Keycloak descrevem a sessão do provedor de login, não a
sessão do agente de código que o ai-memory usa para isolamento em `[auto_scope]`. Os gateways podem
propagar os cabeçalhos autenticados de usuário/cliente/agente, mas
`X-Memory-Actor-Session-Id` deve conter apenas um id real de sessão de hook de ciclo de vida
a partir de uma ponte com reconhecimento de sessão.

Reinicie o serviço após alterar as configurações de provedor:

```bash
systemctl --user restart ai-memory.service      # user mode
sudo systemctl restart ai-memory.service        # system mode
```

### Conectando CLIs de agentes após instalação nativa

Para um servidor loopback local (a CLI encontra o arquivo de token gravado pelo `init`, portanto nenhum
token vai na linha de comando):

```bash
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

Para sessões simultâneas do Claude Code, defina `[auto_scope] mode = "per_session"` na
configuração do servidor e adicione `--session-aware` ao comando `install-mcp`. Isso
funciona para servidores locais e na LAN; a ponte stdio gerada continua usando
`AI_MEMORY_SERVER_URL` / `--server-url` e `AI_MEMORY_AUTH_TOKEN`.

Para um servidor local ou na LAN protegido por bearer, exporte o endpoint primeiro. A
URL do MCP inclui `/mcp`; a URL do hook é a origem básica.

```bash
export AI_MEMORY_SERVER_URL="http://127.0.0.1:49374"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"

ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

O `install-hooks` localiza as fontes de hooks empacotadas sob `/usr/share/ai-memory/hooks`,
preparando em seguida cópias executáveis sob `~/.local/share/ai-memory/hooks/<agent>/` para que
o agente possa executar arquivos pertencentes ao seu usuário. Reexecute `install-hooks --apply`
após atualizações de pacote para renovar essas cópias preparadas.

### Expectativas de latência de hooks

Agentes compatíveis com hooks iniciam um processo de hook de curta duração por evento de ciclo de vida. Uma
chamada de ferramenta bem-sucedida normalmente dispara eventos pré-ferramenta e pós-ferramenta, portanto a
sobrecarga por invocação é incorrida duas vezes. Como referência de ordem de magnitude, uma
avaliação independente da v1.29.0 no macOS aarch64 nativo mediu cerca de 145 ms por
invocação `posix-native` (cerca de 290 ms por chamada de ferramenta concluída) e cerca de 172
ms por invocação de `.sh` legado.

Esses números são medições de um único host, não um benchmark ou garantia
de desempenho. Inicialização de processos, sistema de arquivos e softwares de segurança, o
diretório de dados selecionado, autenticação e carga do host podem alterar o resultado. O
caminho rápido de hook nativo ignora o carregamento completo de configurações e rastreamento, e normalmente
enfileira localmente em spool em vez de aguardar o servidor, portanto mantenha o padrão
nativo do instalador onde aceito. Implantações sensíveis a latência devem medir
seu próprio host de agente antes de optar por eventos capturados adicionais ou escolher
um fallback em script.

### Capacidade e atualização da política de captura

`[capture] ignore_paths` é imposto apenas por comandos nativos `ai-memory hook`
e integrações geradas do OpenCode/OMP/Pi/OpenClaw. Instaladores locais selecionam
comandos nativos onde aceito. O wrapper Docker para Linux/macOS também roteia
o `install-hooks` comum por meio de seu cliente nativo de host verificado por checksum, portanto
o caminho de início rápido está coberto. Hooks legados `.sh`/`.ps1`, uma definição explícita de
`AI_MEMORY_HOOK_PLATFORM=posix|windows`, `setup-agent` e pacotes de scripts Docker
somente remotos/manuais não o impõem. Reexecute `install-hooks --agent
<agent> --apply` ou atualize/reinstale plugins gerados após upgrades; a
saída de recursos do instalador reflete a integração selecionada. Consulte a
[referência canônica de exclusões de captura](marker-file.md#exclusões-de-captura).

Os corpos das observações de ciclo de vida são limitados separadamente do limite de requisição
HTTP de 10 MiB. Prompts do usuário e resumos pós-compactação retêm até 16 KiB;
notificações e trechos de ferramentas retêm até 2 KB. Comandos nativos `ai-memory hook`
truncam esses campos com segurança para UTF-8 antes de entrarem no spool local
ou na rede. O servidor repete os limites específicos por evento para todas as integrações,
incluindo clientes em script e gerados, aplicando em seguida um teto final de 16 KiB após
a higienização antes que qualquer observação alcance o SQLite ou FTS. Comandos de hook nativos
invocam o binário instalado diretamente, portanto atualizar esse binário é suficiente para
receber o limite do lado do cliente.

**Desative a captura de prompts do Claude Code.** Máquinas regulamentadas ou de confiança mista podem
manter o restante da captura de ciclo de vida do Claude Code ativado enquanto evitam
que o texto de `UserPromptSubmit` entre no spool local ou na rede:

```bash
ai-memory install-hooks --agent claude-code --no-capture-prompts --apply
```

O instalador remove apenas o hook de prompt do ai-memory e preserva hooks de terceiros
registrados sob o mesmo evento. Um `install-hooks --apply` simples posterior
(incluindo a atualização após upgrade) herda o estado desativado. Reative a captura de prompts
explicitamente com `--capture-prompts`. Essas opções são exclusivas do Claude Code:
alguns outros agentes usam seu hook de prompt para injetar o contexto de handoff, portanto removê-lo
quebraria a continuidade. Desativar a captura de prompts reduz os resumos de sessão
e a qualidade de recuperação porque a intenção do usuário não faz mais parte do fluxo de observações;
a captura de limites de sessão e ferramentas continua inalterada.

**Capture apenas repositórios que optarem explicitamente.** Os controles acima restringem *o que* é
capturado; este restringe *onde*. Por padrão, um repositório sem marcador
`.ai-memory.toml` ainda é capturado, para que uma máquina que trabalha em múltiplos
checkouts capture cada um novo automaticamente. Esquecer um marcador significa
capturar mais, não menos. O modo allowlist inverte isso:

```bash
ai-memory install-hooks --apply --capture-mode allowlist
```

Um repositório sem marcador então não emite **nenhum evento de ciclo de vida**, nem mesmo
um reduzido. O evento é descartado no processo do hook antes que possa alcançar o
spool local ou a rede, portanto nada é gravado em disco para um repositório que
nunca optou explicitamente. Para habilitar um repositório, coloque um marcador
`.ai-memory.toml` nele, que é o mesmo arquivo que já configura roteamento e
`ignore_paths`.

**Isso é imposto apenas por comandos nativos `ai-memory hook`**, a mesma
fronteira que já se aplica a `[capture] ignore_paths`, e pelo mesmo
motivo: a validação é executada dentro do binário do hook, imediatamente antes de colocá-lo no spool.

Isso é o que um `install-hooks --apply` normal grava no Linux, macOS e
Windows, portanto a instalação usual está coberta. São as instalações de *scripts* que
não estão: os hooks empacotados em shell/PowerShell fazem POST diretamente ao servidor e nunca
executam o binário, portanto nada lê o modo. Na prática, isso significa o
override de plataforma legado `posix`/`windows` (`AI_MEMORY_HOOK_PLATFORM`), o
fluxo remoto `setup-agent` e instalações manuais de pacotes de scripts Docker. O
caminho comum de `install-hooks` do wrapper Docker para Linux/macOS usa seu cliente
nativo de host e está coberto. O `install-hooks --apply` imprime o modo e avisa quando
a instalação que está gravando não puder impô-lo.

Dentro desse limite, o modo não é por agente: ele é armazenado uma vez no diretório de dados
e todo comando de hook nativo o lê, qualquer que seja o agente que o invocou.
Isso também significa que um `install-hooks --apply` simples posterior,
incluindo a atualização automática dentro de `ai-memory upgrade`,
o mantém inalterado por construção em vez de redetectá-lo. Todo
`--apply` imprime o modo em vigor. Retorne ao padrão com
`--capture-mode denylist`.

Verifique isso em qualquer repositório sem alterar nada:

```bash
printf '{"cwd":"%s"}' "$PWD" | ai-memory hook --event user-prompt-submit \
    --agent claude-code --server-url "$AI_MEMORY_SERVER_URL" --check-capture
```

`--check-capture` inspeciona a política sem enfileirar em spool, drenar ou contatar o
servidor, e precisa de uma carga útil JSON informando o diretório a testar. Na saída,
`"admits_capture": false` significa que esse repositório não captura nada;
`"marker_present"` mostra se um marcador foi encontrado na busca recursiva para cima.

Observe o trade-off: a recuperação é perdida para cada repositório que você não marcar, e um
repositório que você *pretendia* capturar permanece silencioso até que você adicione seu marcador.

**Envie diferentes repositórios para servidores diferentes.** Uma instalação pode entregar
capturas para vários servidores ai-memory, por exemplo, um por organização. Registre
cada servidor uma vez com `ai-memory server add <name> --url <url> --root <dir>
--auth-token-stdin`, definindo em seguida `server = "<name>"` no
`.ai-memory.toml` de cada repositório. Repositórios sem a chave continuam usando o servidor
que o `install-hooks` configurou. Um perfil que não resolve descarta o evento
em vez de recorrer a um fallback. Comandos de hook nativos roteiam perfis; as integrações
geradas em TypeScript e os hooks em script descartam repositórios roteados. Consulte
[`marker-file.md`](marker-file.md#roteamento-de-captura-para-outro-servidor-server).

Alguns harnesses de agentes anexam o turno final do assistente ao seu evento `Stop`;
o Claude Code o envia como um `last_assistant_message` bruto. Por padrão, esse texto
nunca é persistido: o binário do hook nativo remove o campo bruto antes que ele possa alcançar
o spool local ou a rede, e o servidor o remove defensivamente ao recebê-lo.

**Captura opt-in (#196).** Você pode optar por armazenar um trecho higienizado e limitado
a 2 KB do turno final do assistente como o corpo do Stop. É uma **adesão
dupla**: ative primeiro no servidor e depois no cliente:

1. **Servidor:** defina `capture_assistant = true` no
   `<data_dir>/config.toml` ativo (ou no arquivo TOML configurado do serviço), ou defina
   `AI_MEMORY_CAPTURE_ASSISTANT=true`, reiniciando em seguida o `ai-memory serve`.
2. **Cliente:** reinstale os hooks do Claude Code (ou Codex) com a flag:

   ```bash
   ai-memory install-hooks --agent claude-code --capture-assistant --apply
   # Codex is supported too — its Stop payload carries last_assistant_message:
   ai-memory install-hooks --agent codex --capture-assistant --apply
   ```

O cliente higieniza (padrões embutidos) e trunca o trecho antes que ele
toque o spool ou a rede; o servidor limpa novamente com seus padrões `[sanitize]`
antes de armazenar. Se qualquer um dos lados estiver desligado, ou o marcador estiver malformado, o Stop
permanece vazio. Executar novamente `install-hooks` sem `--capture-assistant` preserva
uma opção já ativada. Para removê-la, execute `ai-memory uninstall --only hooks
--apply --yes` e reinstale sem a flag. `--capture-assistant` é aceito apenas para Claude Code, Codex e
OpenCode 2 (`--agent opencode2`) em uma plataforma de hook nativa; em qualquer outro agente ou no fallback em script, o
instalador o recusa em vez de ativar algo que não surtirá efeito. O texto do assistente é
sensível em termos de privacidade. Leia as notas em `SECURITY.md` sobre o que ele pode conter e para onde
ele flui (prompts de consolidação/revisão e para fora, para um provedor LLM em nuvem, se houver um
configurado) antes de ativá-lo.

A atualização do binário é suficiente para instalações nativas do Claude Code, e eventos
pendentes em spool são drenados com o campo bruto removido também. O wrapper Docker
para Linux/macOS instala o mesmo comando de hook nativo por meio de seu cliente de host
verificado por checksum. Instalações que selecionam explicitamente o fallback de compatibilidade
`.sh`/`.ps1` (`AI_MEMORY_HOOK_PLATFORM=posix|windows`) ou usam um pacote de scripts Docker
remoto/manual não conseguem higienizar o texto do assistente, de modo que uma carga útil de `Stop`
que ainda porte o campo bruto é descartada inteiramente pelo script em vez de enviada
literalmente via POST. Mesmo quando esse fallback é mantido, o servidor remove qualquer campo bruto
ao receber antes da persistência.

Comandos nativos `ai-memory hook --event ...` colocam eventos em spool localmente. O pacote em
shell POSIX e o pacote em PowerShell (`.ps1`) também colocam em spool, mas apenas em caso de
falha: eles tentam o POST primeiro e gravam o evento no mesmo contrato em
`<data_dir>/hook-spool/` quando o servidor está inacessível, responde com
5xx, ou responde com 408, 425 ou 429 (um servidor saturado solicita nova tentativa),
descarregando em seguida o backlog após a próxima entrega bem-sucedida. Qualquer outro 4xx
é uma rejeição permanente e não é repetido. Os drenos de script
deixam as entradas OIDC nativas intocadas para o `ai-memory hook-drain`,
que resolve ou renova suas credenciais a partir de `auth.json`; eles continuam
com as outras entradas elegíveis dentro da mesma varredura limitada.
O início da sessão
faz uma drenagem curta e limitada de limpeza antes de buscar um handoff; eventos de fronteira
propensos a cancelamento (`stop`, `pre-compact` e `session-end`) iniciam um assistente
`hook-drain` desacoplado para que a entrega não dependa da sobrevivência de um único hook de desligamento.
Os pacotes de script atribuem uma chave de idempotência antes de seu POST inicial e
mantêm essa chave se o evento for para o spool. Um servidor que processou um evento
mas perdeu a resposta não duplicará sua observação ou efeitos de encerramento de sessão
já concluídos; se o processamento parou após a confirmação da observação, a nova tentativa reexecuta
o trabalho downstream. SessionEnd confirma
atomicamente sua marca d'água final com seu handoff automático; uma nova tentativa que encontre essa
transação concluída finaliza qualquer commit pendente no wiki, enfileiramento em provedor
durável e conclusão de chave de ingestão sem adicionar um segundo handoff. Esses
efeitos incompletos permanecem com garantia de pelo menos uma entrega até que o servidor marque o evento como concluído.

Se o harness for encerrado antes de poder enviar `SessionEnd` (incluindo esgotamento de
cota de API ou corte por 429), o próximo início de sessão não precisa de um fechamento sintético para
recuperar o contexto. O servidor escolhe a sessão aberta substantiva mais recentemente ativa
no mesmo projeto e limite de proprietário e injeta um instantâneo somente leitura
de suas 64 observações higienizadas mais recentes, limitado a 6.000 caracteres. Ele não faz
cópia contínua de transcrição, não lê observações de outro projeto e não
fecha a sessão de origem, que ainda pode ser um agente paralelo válido. Prompts
recentes e metadados limitados de ferramentas e ciclo de vida estão sempre disponíveis. O status mais
recente do assistente aparece apenas quando a captura de assistente do Claude Code foi ativada conforme
descrito acima. Use `finalize-session` mais tarde, quando tiver certeza de que a origem
abandonada não está mais ativa.

No Unix, o assistente usa um inicializador confiável `setsid` quando disponível e recorre
a um grupo de processos separado caso contrário; o Windows usa flags de processo desacoplado/independente.
O spool possui limite, de modo que um backlog permanentemente não drenado é
eventualmente podado em vez de ilimitado, mas eventos antigos não entregues podem ser perdidos.

Drenagens nativas de hooks descartam um evento após 8 tentativas com falha por padrão, incluindo
tentativas em que o servidor está inacessível. Defina `max_attempts = 0` sob
`[hook_spool]` em `<data_dir>/config.toml` (ou defina
`AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0`) para continuar
tentando até que o limite de idade de 7 dias ou o teto de 10.000 arquivos no spool se aplique. Um valor
positivo define o número de passagens com falha antes do descarte. Os mesmos limites se aplicam às
inicializações offline degradadas do `ai-memory run`, que dependem deste spool (consulte
[Inicializações offline degradadas](managed-workstreams.md#inicializações-offline-degradadas)).

Os limites de tempo integrados permanecem curtos nos caminhos visíveis ao agente, mas instâncias com alta
latência ou grande backlog podem aumentá-los com variáveis de ambiente de tempo de execução em minutos
inteiros no ambiente do agente; nenhuma reexecução do `install-hooks` é necessária:

| Variável de ambiente | Padrão integrado | Limite máx. | O que ela limita |
|---|---:|---:|---|
| `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES` | 3 segundos | 60 minutos | POST de cada evento durante uma drenagem |
| `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES` | 3 segundos | 60 minutos | o GET síncrono de handoff em `session-start` |
| `AI_MEMORY_HOOK_START_BUDGET_MINUTES` | 3 segundos | 60 minutos | tempo total que `session-start` pode gastar aguardando o lock de drenagem e a limpeza |
| `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` | 5 minutos | 60 minutos | tempo total que o assistente desacoplado `hook-drain` pode gastar após uma fronteira de drenagem em segundo plano |
| `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD` | 32 eventos | inteiro positivo | tamanho do backlog no spool que aciona uma drenagem incremental de 250 ms em `post-tool-use` |
| `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS` | 8 passagens com falha | inteiro não negativo | passagens com falha na drenagem antes de descartar um evento; `0` desativa apenas descartes por contagem de tentativas |

A configuração de novas tentativas também pode ser gravada como `max_attempts` sob `[hook_spool]` em
`<data_dir>/config.toml`; o ambiente de execução tem precedência quando ambos estão definidos. Um valor
definido mas inválido em `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS` (não numérico) recorre ao
padrão embutido de 8, substituindo qualquer valor em `config.toml`, a mesma convenção
das variáveis de temporização. Um valor
zero ainda deixa ativos o limite de idade de 7 dias e o teto de 10.000 arquivos no spool.

Os valores de temporização devem ser minutos inteiros positivos. Valores ausentes, vazios, não numéricos ou
zero recorrem aos padrões embutidos; valores acima de 60 são limitados a 60. O
limite incremental é uma contagem positiva de eventos; valores inválidos recorrem a 32.

A ingestão de hooks no servidor também possui um limitador opcional por fonte para instalações compartilhadas ou
remotas que precisam de proteção contra uma sessão desgovernada de agente. Defina
`AI_MEMORY_HOOK_RATE_PER_SEC` no servidor com a taxa de reposição de tokens por
fonte de ator/sessão; `0` ou não definido desativa o limitador. Defina
`AI_MEMORY_HOOK_RATE_BURST` para substituir o tamanho da rajada (o padrão é a taxa de
reposição, mínimo de um token quando ativado). O limitador é delimitado tanto em contagem de chaves
quanto em bytes de chaves, e drenagens via `/hook/batch` podem ignorar fontes acima do orçamento enquanto
ainda aceitam fontes não relacionadas posteriores.

### Operações de serviço nativo

```bash
# User service
systemctl --user restart ai-memory.service
systemctl --user stop ai-memory.service
journalctl --user -u ai-memory.service -n 100

# System service
sudo systemctl restart ai-memory.service
sudo systemctl stop ai-memory.service
journalctl -u ai-memory.service -n 100
```

Os backups continuam usando a mesma CLI, apenas a aponte para o diretório de dados do serviço:

```bash
# User service
ai-memory --data-dir ~/.local/share/ai-memory backup --to ~/ai-memory-backup.tar.gz

# System service
sudo -u ai-memory ai-memory --data-dir /var/lib/ai-memory backup --to /var/lib/ai-memory/backup.tar.gz
```

A remoção do pacote não exclui dados. Pare o serviço e remova o estado apenas
quando quiser intencionalmente apagar a memória:

```bash
systemctl --user disable --now ai-memory.service
sudo systemctl disable --now ai-memory.service

# Optional destructive cleanup:
rm -rf ~/.local/share/ai-memory ~/.config/ai-memory
sudo rm -rf /var/lib/ai-memory /etc/ai-memory
```

### Teste de integração do mantenedor

O CI normal executa `scripts/check-native-packaging.sh`, uma verificação
de regressão segura para o host que usa uma raiz alternativa temporária para `systemd-analyze`,
`systemd-sysusers` e `systemd-tmpfiles`. Ela verifica a sintaxe das unidades, caminhos
esperados, saída do sysusers, regras do tmpfiles, modo do arquivo env e sintaxe shell do AUR
sem gravar no `/usr`, `/etc`, `/var` do host nem interferir em serviços reais.

O repositório também inclui uma esteira de integração manual do Arch que é intencionalmente
mantida fora do CI de rotina porque cria um distrobox descartável, instala
pacotes, inicia serviços reais do systemd e pode levar vários minutos:

```bash
scripts/test-native-arch-systemd-distrobox.sh
```

Ela verifica o formato dos metadados do AUR, compila a árvore de trabalho atual, instala
o layout nativo no contêiner Arch descartável, inicia o serviço de sistema
com `systemctl`, inicia o comando de perfil de usuário sob supervisão transitória
do systemd e verifica se as fontes de hooks empacotadas em
`/usr/share/ai-memory/hooks` podem ser preparadas por `install-hooks`.

A parte destrutiva desse script recusa-se a executar a menos que detecte um
ambiente de contêiner/distrobox.

Parâmetros úteis:

```bash
AI_MEMORY_NATIVE_TEST_BOX=ai-memory-native-test scripts/test-native-arch-systemd-distrobox.sh
AI_MEMORY_NATIVE_TEST_KEEP_BOX=1 scripts/test-native-arch-systemd-distrobox.sh
AI_MEMORY_NATIVE_TEST_IMAGE=quay.io/toolbx/arch-toolbox:latest scripts/test-native-arch-systemd-distrobox.sh
```

---

## Nix / NixOS

A configuração do módulo NixOS voltada ao usuário reside na [seção NixOS do
README](../../README.md#nixos) (`nixosModules.default`,
`services.ai-memory.enable`). Esta seção é a esteira de testes do mantenedor
para o flake e módulo: o que o CI executa e o que cada camada comprova.

O pacote não inclui o `git`: o módulo adiciona `pkgs.git` ao
`PATH` do serviço, e uma instalação apenas do pacote necessita do `git` instalado ao lado dele.
Não encapsule o binário para fornecê-lo (consulte a seção do README para entender o motivo).

### Esteira de testes do mantenedor

1. **Teste básico do pacote**: `nix build .#packages.<system>.default` e depois
   `scripts/check-nix-packaging.sh ./result` (binário `--version`, árvore
   de hooks, modelo de configuração, `nix run`). O Linux é executado em todo PR com filtro
   de caminho `nix.yml`; o Darwin é apenas por agendamento / `workflow_dispatch` / label `nix` ou
   `full-ci`.
2. **Contratos de avaliação**: `nix build .#checks.x86_64-linux.nixos-module-eval`
   e `nixos-sandbox-parity`. Asserções rápidas exclusivas para Linux para enable/bind/
   configuração de `--config`/segredos, montagens somente de API vs web, mensagens de recusa (exclusão mútua age+sops, segredos em
   `settings.auth`, `settings.bind`), `ExecStart` com escape (`--data-dir`,
   `serve`, `--transport http`), `StateDirectory` padrão vs `dataDir`
   personalizado em tmpfiles/`ReadWritePaths`, e paridade de chaves de sandbox com
   `nix/systemd-sandbox.nix`. Executado em PRs com caminhos filtrados juntamente com o job do pacote
   Linux.
3. **Toplevel → OCI → teste básico em contêiner**: um caminho de closure. Compilar
   `packages.x86_64-linux.nixos-ai-memory-docker` compila
   `system.build.toplevel` uma vez (por meio do tarball de imagem docker do nixpkgs);
   não há um job separado de toplevel puro no CI. Em seguida:

```bash
scripts/test-nixos-systemd-container.sh
```

   Esse script importa o rootfs, executa um contêiner privilegiado com systemd,
   verifica `systemctl is-active ai-memory` e `curl /healthz` dentro do contêiner
   (o bind de loopback padrão do módulo é inacessível via Docker `-p`),
   grava um
   marcador sob `/var/lib/ai-memory`, reinicia a unidade e (por padrão)
   remonta um volume nomeado uma vez. Restrito no `nix.yml` para agendamento /
   `workflow_dispatch` / label `nix` ou `full-ci` (não em qualquer bump
   de Cargo.lock).

**Fora do escopo** (não considere esses pontos como cobertos pela esteira acima):

- Matrizes de upgrade de flake/pacote de A→B ou migração de SQLite-wiki
- Cobertura exaustiva de configurações tipadas (chaves TOML desconhecidas usam `freeformType`)
- Systemd simulado/suave sem inicialização real de unidade
- Afirmar que a imagem Docker do aplicativo publicada cobre o caminho do módulo NixOS
- Darwin / OCI NixOS multiarquitetura (apenas Linux x86_64 por design)

Parâmetros úteis para o teste básico em contêiner:

```bash
AI_MEMORY_NIXOS_TEST_KEEP=1 scripts/test-nixos-systemd-container.sh
AI_MEMORY_NIXOS_TEST_VOLUME=0 scripts/test-nixos-systemd-container.sh
AI_MEMORY_NIXOS_TEST_IMAGE=ai-memory-nixos-test scripts/test-nixos-systemd-container.sh
AI_MEMORY_DOCKER=podman scripts/test-nixos-systemd-container.sh
```

---

## Aplicativo da barra de menus do macOS

No Mac, o aplicativo autônomo da barra de menus é a instalação GUI: ele empacota o
binário nativo `ai-memory` e a árvore `hooks/`, controla o LaunchAgent existente
(`com.github.akitaonrails.ai-memory`) e abre o `/web`, `ai-memory status`,
`config.toml`, o diretório de dados e os logs. Ele não substitui essas ferramentas
por um segundo painel.

Necessita de uma toolchain Rust e Xcode / Swift 6 (o mesmo que uma compilação pelo código-fonte):

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory
./companions/ai-memory-macos/build.sh
open "companions/ai-memory-macos/dist/AI Memory.app"
```

Arraste o **AI Memory.app** para `/Applications`, depois clique em **Install & Start Server**
no menu extra (sem ícone no Dock). Quando o item de status estiver verde, configure um
agente com o binário empacotado para que o `install-hooks` localize a árvore
irmã `hooks/`:

```bash
BIN="/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"
"$BIN" install-mcp --client claude-code --apply
"$BIN" install-hooks --agent claude-code --apply
```

A memória durável permanece em `~/Library/Application Support/ai-memory`. A substituição
do `.app` é uma atualização e não reescreve essa árvore. Os caminhos de tarball pré-compilado,
compilação pelo código-fonte, wrapper Docker e launchd instalado manualmente continuam em
[`docs/macos.md`](macos.md). Código-fonte do companion:
[`companions/ai-memory-macos`](../../companions/ai-memory-macos).

---

## Configurando outras CLIs de agentes

> `install-mcp --server-url` aceita tanto a origem simples do servidor quanto o endpoint
> completo do MCP e anexa um `/mcp` ausente exatamente uma vez.
> `install-hooks --server-url` recebe a **origem** simples do servidor
> (ex.: `http://homelab:49374`); os scripts de hook anexam `/hook`, `/handoff`,
> etc. por conta própria.

Cada CLI de agente precisa de duas coisas:

1. **Registro MCP** - para que o agente possa chamar `memory_query`,
   `memory_recent`, `memory_handoff_accept`.
2. **Hooks de ciclo de vida** - para que o servidor capture automaticamente os eventos da sessão.
   Sem isso, o agente ainda pode consultar a memória, mas a captura
   torna-se manual.

Claude Desktop, VS Code Copilot e Zed são atualmente somente MCP. Os
clientes compatíveis com hooks na [Matriz de Suporte do README](../../README.md#support-matrix),
incluindo Pi e Zero, possuem caminhos de captura de ciclo de vida por meio de `install-hooks`.

> **Padrão de instalação de hooks.** Perfis locais aceitos usam como padrão comandos
> nativos do host. O Claude Code pode usar seu formato de execução aceito no Windows (`command` =
> `ai-memory.exe` real, `args` = tokens argv para `hook --event ...`); outros
> agentes usam strings de comando único nativas de acordo com o esquema de seus hooks.
> Pacotes de scripts em PowerShell/Git Bash são fallbacks de compatibilidade e não
> impõem a política de captura v1. O wrapper Docker para Linux/macOS baixa um
> cliente nativo de host verificado por checksum para chamadas comuns de `install-hooks`.
> Instalações de scripts Docker manuais/somente remotas continuam usando o
> caminho em duas etapas: (1) `docker cp` dos scripts empacotados para seu diretório home, (2)
> `docker run --rm install-hooks` gera o trecho de configuração.
> OpenClaw, OpenCode, OMP e Pi são diferentes: eles usam arquivos de plugin/extensão
> TypeScript gerados, portanto nenhuma extração de script shell é
> necessária para esses clientes.

### OpenAI Codex

```bash
# MCP snippet (merge into $CODEX_HOME/config.toml, default ~/.codex/config.toml):
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client codex \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Hooks — extract scripts + render config:
docker cp ai-memory:/usr/local/share/ai-memory/hooks ~/.ai-memory/
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent codex \
        --hooks-dir ~/.ai-memory/hooks \
        --server-url "http://homelab:49374" \
        --auth-token "$TOKEN"
```

A entrada do Codex gravada por `install-mcp` define `default_tools_approval_mode =
"approve"`, de modo que as ferramentas de leitura do ai-memory executem sem confirmação, e adiciona
`[mcp_servers.<name>.tools.<tool>]` com `approval_mode = "prompt"` para cada
ferramenta classificada como destrutiva (`memory_delete_page`, `memory_forget_sweep`,
`memory_restore_page`, as ferramentas de aceitar e cancelar handoffs, as ferramentas de pop e
cancelar mensagens, e `memory_feedback`), de modo que o Codex solicite confirmação antes de executá-las. Uma
reinstalação preserva qualquer `approval_mode` por ferramenta que você mesmo tenha definido. Ambas as chaves estão na
[referência de configuração do Codex](https://developers.openai.com/codex/config-reference).

Hooks nativos de ferramentas do Codex usam os campos de nível superior `tool_name`, `tool_input`, `tool_response`,
e `tool_use_id` (verificado contra a CLI 0.154.0). O ai-memory registra a
família da ferramenta e o ID da chamada em `PreToolUse` e `PostToolUse`; ferramentas reconhecidas,
como `Bash` e `apply_patch`, também retêm um trecho de resposta higienizado em
`PostToolUse`, limitado a 2 KB incluindo metadados. Respostas estruturadas em JSON são
achatadas usando o mesmo caminho de trecho delimitado. Entradas não são copiadas para as
observações, e ferramentas desconhecidas (incluindo nomes MCP não reconhecidos) retêm apenas
metadados. `PostToolUse` sozinho não comprova sucesso, portanto os resultados do Codex continuam
`unknown`.

As exclusões de captura ainda são executadas antes do spool nativo. O `apply_patch` do Codex
passa o texto do patch em `tool_input.command`, o que não fornece caminhos de arquivo
diretos para a política de captura. Com `ignore_paths` ativo, esses eventos retêm
apenas metadados; o ai-memory não analisa textos de patch ou shell para inferir caminhos.
Eventos de ferramentas são entregues no limite normal de 32 eventos ou em uma
fronteira de drenagem de ciclo de vida, de modo que um turno ativo curto pode ainda ter eventos na fila.

A CLI do Codex 0.145.0 e posteriores expõem um hook nativo `SessionEnd`. `Stop` é capturado
como uma fronteira de turno e mantém a sessão aberta; um `SessionEnd` nativo aciona
o resumo final, handoff e elegibilidade para auto-aprimoramento. Consulte o
[ciclo de vida de hooks do Codex](https://learn.chatgpt.com/docs/hooks#sessionend) para saber quando
o Codex encerra uma sessão. Para clientes mais antigos ou uma entrega perdida de fim de sessão, execute:

```bash
ai-memory finalize-session
# add --all to close every matching open Codex session in this workspace/project
# or target one exact concurrent session (mutually exclusive with --all)
ai-memory finalize-session --session-id <uuid>
```

### Hooks de ciclo de vida do Antigravity

O Antigravity (a CLI `agy`, o App desktop e a IDE, que leem o mesmo
`~/.gemini/config/hooks.json` global) não possui evento de fim de conversa, e nenhuma
das cargas úteis de seus hooks carrega o prompt do usuário. `install-hooks --agent
antigravity-cli` configura cinco de seus
[hooks documentados](https://antigravity.google/docs/hooks/):

- `SessionStart` registra o início da sessão e injeta um handoff pendente como
  uma mensagem de sistema `injectSteps`, que permanece na conversa para os
  turnos seguintes.
- `PreInvocation` é executado antes de cada chamada de modelo. Apenas `invocationNum = 0`, o
  início de um turno do usuário, executa alguma ação: o hook nativo lê o prompt do turno
  a partir da transcrição da conversa (`transcriptPath`) e o registra como um
  prompt do usuário. Em uma compilação que não dispare `SessionStart`, o `PreInvocation` do primeiro
  turno registra o início e assume o handoff em vez disso. Um
  marcador por conversa restringe qualquer um dos inícios a uma única vez, de modo que um handoff criado
  enquanto a conversa é finalizada permaneça aberto para a próxima sessão.
- `PreToolUse` e `PostToolUse` capturam chamadas de ferramentas.
- `Stop` encerra um loop de execução. Quando o modelo concluiu o turno
  (`terminationReason` `NO_TOOL_CALL`, denominado `model_stop` na página web)
  e nada mais permaneceu em execução (`fullyIdle: true`), o servidor grava um checkpoint
  de turno: ele atualiza a página de resumo da sessão e seu único handoff
  aberto e deixa a conversa aberta, o mesmo caminho que o OpenCode 2 usa. Prompts
  que a transcrição obteve desde o início do turno são gravados antes do
  `Stop`, para que o checkpoint os inclua. Um loop cancelado, com falha ou
  com limite de orçamento atingido não é um checkpoint.

A transcrição é lida apenas quando `transcriptPath` aponta para o próprio
`~/.gemini/<app>/brain/<conversationId>/.system_generated/logs/transcript.jsonl` da conversa
(ou o `transcript_full.jsonl` ao lado dele, relatado pela CLI), sendo
`<app>` um entre `antigravity`, `antigravity-cli` ou `antigravity-ide`. Outro
arquivo, a transcrição de outra conversa e um diretório em symlink que aponte para
fora da conversa são recusados. Um hook lê no máximo 8 MiB sob um
limite de tempo de sistema de arquivos de 100 ms; um timeout mantém o cursor intocado para que o próximo
hook tente novamente. O cursor delimitado sob
`<data_dir>/hook-state/antigravity-transcripts/` retoma de onde a leitura
anterior parou. Passos que um hook injetou são ignorados, o texto dentro
do encapsulador `<USER_REQUEST>` da CLI é mantido, e cada prompt é limitado e
limpo como qualquer outro antes que o servidor o higienize novamente. Sua chave de ingestão
provém da conversa e do passo, portanto uma leitura repetida o armazena uma única vez. Os
hooks de fallback em shell e PowerShell não analisam a transcrição, e um
checkout não autorizado pela allowlist de captura não é lido de forma alguma.

Nada distingue uma janela fechada do Antigravity de uma conversa ainda em uso,
portanto o handoff de um checkpoint vai para uma nova sessão no mesmo projeto apenas depois
que a conversa estiver inativa por 10 minutos. Para transferi-la imediatamente ou
enfileirar a consolidação opt-in de SessionEnd, feche a conversa explicitamente:

```bash
ai-memory finalize-session --agent antigravity-cli
# add --all only to close every matching open Antigravity session in this scope
# or add --session-id <uuid> to close one exact concurrent session
# if the conversation continued after a first finalize, re-close it to cover
# the new observations (a re-run with nothing new is a harmless no-op)
ai-memory finalize-session --agent antigravity-cli --reopen --session-id <uuid>
```

Para uma sessão cujos prompts não foram capturados pelos hooks em tempo real (os hooks
de fallback em shell, ou uma instalação anterior à captura de transcrições),
o `finalize-session` reproduz seus prompts digitados a partir do
`~/.gemini/antigravity-cli/history.jsonl` do `agy` antes do fim de sessão (somente linhas
cuja conversa e espaço de trabalho coincidam com a sessão), e a página de resumo e
handoff são construídos com eles. Cada prompt é identificado como o `backfill` o identifica, portanto um
refechamento o armazena uma vez e um prompt digitado após a primeira finalização chega
com o próximo. Um checkout não autorizado pela allowlist de captura não envia nenhum,
e `--all-owners` não reproduz nenhum (os prompts de outros usuários não estão no histórico
desta máquina). Quando o histórico está ausente ou não produz prompt, a exportação recorre à
transcrição delimitada e verificada de contenção do brain da conversa selecionada e
mantém apenas etapas explícitas de entrada do usuário. `agy -p` não grava linha em `history.jsonl`,
portanto este fallback fornece seu prompt.

Reexecute `ai-memory install-hooks --agent antigravity-cli --apply` após
a atualização. Uma instalação de um release anterior executa seu tratador de `session-start`
no `PreInvocation` e não possui entrada de `SessionStart`; o hook nativo lê isso
como o fallback do primeiro turno, com prompts da transcrição e checkpoints de turno, até
que a reinstalação adicione `SessionStart`.

### Devin CLI

Devin usa `~/.devin/config.json` para servidores MCP e `~/.devin/hooks.v1.json`
para hooks de ciclo de vida por padrão. Se você preferir um único arquivo combinado de configuração do Devin,
passe `--config-file ~/.devin/config.json` para `install-hooks`; ai-memory então
mescla as entradas de hook sob a chave `hooks` daquele arquivo.

```bash
ai-memory install-mcp --client devin --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent devin --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

ai-memory install-skills --agent devin
```

O vocabulário de hooks do Devin é próximo ao do Claude Code, com duas diferenças
importantes:

- Devin emite `PostCompaction` após compactação e inclui um campo `summary`;
  ai-memory o registra como `post-compaction`.
- Devin não expõe hooks de início/fim de subagentes, portanto ai-memory não consegue capturar
  fronteiras aninhadas de subagentes para o Devin.

O hook `SessionStart` injeta handoffs pendentes por meio de
`hookSpecificOutput.additionalContext` do Devin. Cargas úteis reais de `SessionStart` e
`PostToolUse` do Devin podem omitir `session_id` e `cwd`; ai-memory agora infere cwd
de `DEVIN_PROJECT_DIR` ou do diretório de trabalho do processo de hook quando a carga útil
o omite, e gera/reutiliza um id de sessão por host a partir do estado do hook quando necessário,
para que esses eventos continuem sendo capturados. Um valor fornecido pela carga útil sempre tem precedência.

### Kimi Code

Kimi Code mantém servidores MCP em `~/.kimi-code/mcp.json` e hooks de ciclo de vida em
`~/.kimi-code/config.toml`; ambos se movem juntos quando `$KIMI_CODE_HOME` está definido.
A CLI também aceita `--agent kimi` como um alias. `install-mcp` grava a
URL do servidor com uma query `?flavor=moonshot` porque a API da Moonshot rejeita
`anyOf`/`oneOf`/`allOf` no nível raiz em esquemas de parâmetros de ferramentas ("moonshot
flavored json schema"). O servidor ai-memory responde a requisições com flavor usando
esquemas planos, e todos os outros clientes mantêm o formato upstream.

```bash
ai-memory install-mcp --client kimi-code --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent kimi-code --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

`install-hooks` mescla entradas `[[hooks]]` em `config.toml`, preservando as
configurações de provedor/modelo que o mesmo arquivo contém. As entradas cobrem 10 eventos
(`SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure` (Kimi Code relata falhas de ferramentas separadamente de
chamadas bem-sucedidas; ele reutiliza o tratador de post-tool-use), `Stop`,
`SubagentStart`, `SubagentStop`, e `PreCompact`) e usam como padrão
comandos nativos `ai-memory hook --event … --agent kimi-code` em instalações locais
(spool local mais entrega em lotes, política de captura v1 imposta); o pacote de
scripts preparado sob `~/.local/share/ai-memory/hooks/kimi-code/` é o
fallback de compatibilidade (faz POST para `/hook`, enfileirando uma entrega com falha para uma
drenagem posterior, sem imposição da política de captura v1). Um handoff pendente
é injetado em `UserPromptSubmit` através da saída padrão do hook, que Kimi Code
anexa ao contexto do modelo como uma mensagem do usuário antes do turno; Kimi Code
dispara `SessionStart` mas descarta a saída padrão desse hook, de modo que hooks instalados por
uma versão mais antiga consumiam handoffs sem entregá-los. Comandos de hook nativos
existentes invocam o binário `ai-memory` atual e assumem o comportamento corrigido
de entrega na atualização. Reexecute
`ai-memory install-hooks --agent kimi-code --apply` apenas para uma
instalação com fallback de script para que seus scripts preparados sejam renovados.

Entradas de hook do Kimi Code aceitam apenas `event`, `matcher`, `command` e
`timeout`; campos extras fazem todo o `config.toml` falhar ao carregar, portanto prefira
`install-hooks --apply` a edições manuais.

### Command Code

Command Code mantém as configurações de MCP e hooks no escopo do usuário em arquivos JSON
separados sob `~/.commandcode/`. Instale ambas as integrações com:

```bash
ai-memory install-mcp --client command-code --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent command-code --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Os aliases `commandcode`, `cmdc` e `cmd` são aceitos. `install-mcp`
mescla uma entrada HTTP nativa em `~/.commandcode/mcp.json`; `install-hooks`
mescla apenas os quatro eventos estáveis do Command Code (`SessionStart`, `PreToolUse`,
`PostToolUse` e `Stop`) em `~/.commandcode/settings.json`, preservando
outras configurações e tratadores de hook. As definições de hook deliberadamente omitem
`matcher`: Command Code documenta a omissão como "todas as ferramentas", enquanto qualquer matcher
em `SessionStart` ou `Stop` impede que esse hook de ciclo de vida dispare.

Instalações locais usam o comando nativo `ai-memory hook`, de modo que o
`session_id` e o `cwd` nativos do Command Code sejam atribuídos diretamente;
cargas úteis reconhecidas de `shell_command`, `read_file`, `write_file` e `edit_file`
passam pela mesma política delimitada de exclusão de captura que outras
integrações nativas. Um handoff pendente é injetado por meio de
`hookSpecificOutput.additionalContext` em `SessionStart`.

O evento estável `Stop` do Command Code encerra um turno, não uma sessão. Finalize a
sessão aberta após o último turno quando precisar de consolidação imediata e de um
handoff:

```bash
ai-memory finalize-session --agent command-code
ai-memory finalize-session --agent command-code --session-id <uuid>
```

ai-memory não instala Command Code Mods. Mods executam código arbitrário fora
de sandbox e não são necessários para os caminhos de hooks estáveis ou sessões gerenciadas.

Sessões gerenciadas são opcionais:

```bash
ai-memory run command-code
ai-memory run command-code --yolo --model <model-id>
```

Os aliases `commandcode`, `cmdc` e `cmd` selecionam o mesmo adaptador. O
executável padrão é `command-code` no Unix e `cmdc` no Windows nativo.
Sessões novas mantêm o UUID nativo do Command Code; sessões retornadas usam
`--session <uuid>` exato. O adaptador somente leitura aceita apenas o cabeçalho v3 observado,
exige que o nome de arquivo com UUID e o `cwd` canônico coincidam com o checkout, exclui
arquivos auxiliares de checkpoint/prompt, raciocínio oculto, imagens e metadados de provedor,
e preserva `parentId` em eventos visíveis para procedência de ramificação. Uma versão de
transcrição futura desconhecida falha de modo fechado até que seu esquema seja auditado. Inicializações
diretas de `cmd`, `cmdc` ou `command-code` permanecem inalteradas.

### IBM Bob

IBM Bob IDE e Bob Shell compartilham suas configurações globais sob `~/.bob/settings`.
Instale a captura de MCP e de ciclo de vida juntas:

```bash
ai-memory install-mcp --client ibm-bob --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent ibm-bob --apply \
    --server-url "https://memory.example" \
    --auth-token "$TOKEN"
```

O alias `bob` é aceito. MCP é mesclado em `mcp.json` como uma entrada nativa
`streamable-http` do Bob com `protocolNegotiation: "legacy"`; isso pula a sondagem de
descoberta não padrão do Bob e usa o handshake MCP padrão. Hooks são
mesclados no objeto raiz `hooks` de `settings.json`, preservando outras
configurações e tratadores. O instalador usa apenas os cinco eventos comuns à
IDE e ao Shell, entrega o handoff pendente como contexto simples em `SessionStart`
e impõe exclusões de captura para eventos de ferramentas.

Bob não possui um evento real de fim de sessão. Use `ai-memory finalize-session --agent
ibm-bob` após o turno final quando precisar do resumo e do handoff imediatamente.
A retomada gerenciada com `ai-memory run ibm-bob` não é compatível porque Bob não
documenta um armazenamento de transcrição nativo estável. Consulte [os detalhes
de MCP e hooks do IBM Bob](mcp-install.md#ibm-bob).

### Qwen Code

Qwen Code (`qwen`) mantém servidores MCP e hooks de ciclo de vida no mesmo arquivo
do usuário, `~/.qwen/settings.json`. Instale ambos juntos:

```bash
ai-memory install-mcp --client qwen-code --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent qwen-code --apply \
    --server-url "https://memory.example" \
    --auth-token "$TOKEN"
```

Os aliases `qwen` e `qwencode` são aceitos. MCP é mesclado sob
`mcpServers` como uma entrada `httpUrl` streamable-HTTP do Qwen (`timeout` em
milissegundos); a chave simples `url` é o transporte SSE legado. Uma instalação com
arquivo de token grava uma entrada stdio `ai-memory mcp-bridge --no-session-id` em vez de
colocar o bearer em `headers`. Hooks são mesclados no objeto raiz `hooks`,
preservando outras configurações e tratadores. Cada entrada executa o comando nativo
`ai-memory hook --event … --agent qwen-code` com um matcher vazio;
não há pacote de scripts. Dez eventos são configurados: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure` (Qwen
o dispara em vez de `PostToolUse` quando uma ferramenta falha), `PreCompact`, `Stop`,
`SessionEnd`, `SubagentStart` e `SubagentStop`. Exclusões de captura se aplicam às
ferramentas do Qwen semelhantes ao Gemini (`write_file`, `read_file`, `run_shell_command`,
`grep_search`, `glob`). A captura de prompts do usuário usa apenas o `submitted_prompt`
do Qwen que carrega procedência; conteúdo automatizado de continuação ou nova tentativa
que apareça apenas em `prompt` não é armazenado nem interpretado como solicitação de recarregamento.

O hook `SessionStart` entrega o handoff da sessão anterior por meio de
`hookSpecificOutput.additionalContext`. Sem nada a entregar, `SessionStart`
e `UserPromptSubmit` não imprimem nada, pois Qwen injeta essa saída padrão na
sessão. Um `SessionStart` interativo pode chegar sem `cwd`; o hook
usa então `QWEN_PROJECT_DIR`, que Qwen exporta em cada hook de comando. Quando
nenhum deles indica um diretório, o hook ainda registra o evento, mas não busca nenhum
handoff; o bastão permanece aberto para `memory_handoff_accept`. Qwen dispara um `SessionEnd`
real, portanto `finalize-session` não é necessário.

`ai-memory setup` configura Qwen Code quando `~/.qwen` existe.
`ai-memory uninstall` remove as entradas. Qwen lê `AGENTS.md` e
`QWEN.md`; uma execução de `backup-agents` exclusiva para o Qwen preserva ambos os arquivos,
`~/.qwen/settings.json`, `~/.qwen/skills` e o compartilhado `~/.agents/skills`.
A retomada gerenciada com `ai-memory run qwen` não é compatível porque Qwen
não documenta um armazenamento de transcrição nativo estável. Inicie uma nova sessão do Qwen
após a instalação para que ele recarregue `settings.json`. Consulte [os detalhes
de MCP e hooks do Qwen Code](mcp-install.md#qwen-code).

### Qoder

Qoder CLI e IDE compartilham `~/.qoder/settings.json`. Instale a captura de
MCP e de ciclo de vida juntas:

```bash
ai-memory install-mcp --client qoder --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent qoder --apply \
    --server-url "https://memory.example" \
    --auth-token "$TOKEN"
```

A entrada MCP executa `ai-memory mcp-bridge --no-session-id` usando o formato stdio
do Qoder. Portanto, ela alcança servidores ai-memory locais ou remotos sem iniciar
um segundo servidor, mas continua sendo um cliente estático: chamadas MCP com escopo de projeto devem passar
`workspace` e `project`. O instalador de hooks preserva outras configurações,
servidores e tratadores ao adicionar `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PreCompact`, `Stop` e
`SessionEnd`. Um handoff pendente é entregue como SessionStart
`hookSpecificOutput.additionalContext`; hooks de contexto somente de captura permanecem silenciosos.
Quando uma carga útil omite `cwd`, apenas o `QODER_PROJECT_DIR` documentado pelo Qoder é utilizado.

`ai-memory setup` detecta `~/.qoder`; `backup-agents` preserva suas configurações e
os ativos compartilhados `~/.agents/skills` / `AGENTS.md` quando Qoder detém esses caminhos
compartilhados. Reinicie Qoder após alterar os hooks. A execução gerenciada com `ai-memory run qoder` e
o backfill de transcrição nativa não são oferecidos porque Qoder não documenta um
contrato estável de armazenamento de transcrições. Consulte [as notas detalhadas
de MCP e hooks](mcp-install.md#qoder).

### Kiro CLI

Kiro CLI tem uma superfície MCP e dois formatos incompatíveis de hooks de ciclo de vida.
ai-memory oferece suporte a ambos por meio de destinos de instalador explícitos: `kiro-cli` continua
sendo o destino v2, enquanto `kiro-cli-v3` seleciona o registro autônomo v3. O
arquivo global de MCP é `$KIRO_HOME/settings/mcp.json`, com padrão em
`~/.kiro/settings/mcp.json`; passe `--config-file .kiro/settings/mcp.json` para uma
entrada no escopo do projeto.

```bash
ai-memory install-mcp --client kiro-cli --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
```

O alias `kiro` é equivalente. A URL instalada inclui
`?flavor=bedrock` para que o backend Bedrock do Kiro receba esquemas sem
`anyOf`, `oneOf` ou `allOf` no nível raiz; esquemas aninhados e validação em tempo de execução
permanecem intactos. Kiro exige HTTPS para servidores remotos que não sejam de loopback, portanto a CLI
rejeita uma URL de homelab em HTTP simples antes de alterar o arquivo. Configure um proxy
reverso conforme descrito em [HTTPS via proxy reverso](https-via-proxy.md).

Instale hooks v2 com o valor de agente `kiro-cli`. Quando `--server-url` for omitido,
`install-hooks` pode inferir a origem do hook e o token bearer a partir da entrada MCP
gerenciada acima.

```bash
# Default v2 engine: merge hooks into every existing global agent config.
ai-memory install-hooks --agent kiro-cli --apply

# A project-local v2 agent overrides a same-named global agent. Update the
# selected local config explicitly instead of assuming the global copy runs.
ai-memory install-hooks --agent kiro-cli --apply \
    --config-file .kiro/agents/<agent-name>.json
```

O motor v2 armazena hooks em camelCase dentro de arquivos JSON do agente. ai-memory atualiza
apenas arquivos `$KIRO_HOME/agents/*.json` existentes; ele não criará um agente
que Kiro nunca selecione. Crie e selecione um agente primeiro quando esse diretório
estiver vazio. Kiro dá [precedência a agentes locais de projeto sobre agentes globais](https://kiro.dev/docs/cli/custom-agents/configuration-reference/),
portanto use `--config-file` quando a definição ativa residir sob
`.kiro/agents/`. Todos os arquivos de destino são analisados antes que qualquer um seja alterado, e
campos não relacionados do agente, hooks de terceiros e o `--project-strategy` existente
de cada agente permanecem intactos.

A instalação registra captura de inicialização, prompt do usuário, pré-ferramenta, pós-ferramenta e parada,
permanece com tolerância a falhas quando ai-memory não está disponível e entrega um handoff pendente
por meio da saída padrão bem-sucedida de `agentSpawn`. Cargas úteis de ferramentas v2 verificadas impõem
`[capture] ignore_paths`; um formato de carga útil de ferramenta de arquivo não reconhecido é armazenado como
metadados delimitados em vez de expor o conteúdo do arquivo.

Instale hooks v3 com o destino explícito `kiro-cli-v3`. Esta distinção é
intencional: `kiro` e `kiro-cli` continuam significando v2 para que uma atualização não possa
reescrever silenciosamente uma instalação existente para um formato incompatível. O
registro autônomo foi testado em aceitação com uma sessão interativa do Kiro CLI
2.16.2 `--v3`.

```bash
# Global v3 registration under $KIRO_HOME/hooks (default ~/.kiro/hooks).
ai-memory install-hooks --agent kiro-cli-v3 --apply

# Project-local v3 registration.
ai-memory install-hooks --agent kiro-cli-v3 --apply \
    --config-file .kiro/hooks/ai-memory.json
```

O instalador v3 grava o esquema autônomo documentado `version: "v1"` com
gatilhos em PascalCase. Ele preserva entradas de terceiros em um arquivo compartilhado,
recusa uma versão de esquema incompatível ou uma colisão de terceiros com um
nome de hook reservado pelo ai-memory, e limita comandos somente de captura a um segundo.
SessionStart recebe cinco segundos para que a busca delimitada de handoff do ai-memory possa terminar.
Ambos os motores usam a mesma fronteira de entrada de hooks higienizada: operações
de arquivo documentadas e ativas de `tool_name`/`tool_input` respeitam `[capture] ignore_paths`, enquanto
formatos desconhecidos de carga útil de ferramentas de arquivo degradam para captura apenas de metadados.

O evento `stop` do Kiro v2 encerra um turno, não a sessão. Após o turno final, feche
a sessão correspondente explicitamente; use o id exato quando várias sessões do Kiro
estiverem abertas no mesmo projeto:

```bash
ai-memory finalize-session --agent kiro-cli
ai-memory finalize-session --agent kiro-cli --session-id <uuid>
```

`ai-memory uninstall --only hooks --apply --yes` remove apenas entradas exatas do ai-memory
de agentes globais v2, do diretório `.kiro/agents` do projeto atual
e do registro global/projeto atual v3 do ai-memory. Um arquivo v3 puramente
gerado é excluído; entradas de terceiros em um arquivo compartilhado permanecem. `ai-memory run
kiro` (alias `kiro-cli`) gerencia o motor v2 padrão e respeita `$KIRO_HOME`;
adicione `--v3`, `--mode` ou `--agent-engine v3` para retomada segura da versão v3. Uma vez
vinculado, um início posterior direto do Kiro recupera o motor armazenado de forma transparente, e
o `ai-memory run` simples considera sessões locais do checkout de ambos os armazenamentos
incompatíveis. Consulte
[workstreams gerenciados](managed-workstreams.md#comportamento-do-adaptador-nativo).

### Pool (Poolside Agent CLI)

O Pool lê hooks de ciclo de vida de um `.poolside/settings.yaml` com escopo de projeto na
raiz de cada repositório em que é executado; não há arquivo global de hooks de usuário para
o ai-memory mesclar. `install-hooks --agent pool` (alias `poolside`), portanto,
prepara os scripts de hook no local global estável do usuário e imprime um
trecho pronto para colar `hooks:`; o ai-memory deliberadamente não grava
arquivos com commit dentro dos seus repositórios (o `.claude/settings.local.json`
ignorado pelo git que `--scope project` grava para o Claude Code é
a única exceção).

```bash
# Stage the scripts and print the snippet to paste into
# <repo>/.poolside/settings.yaml:
ai-memory install-hooks --agent pool --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

O trecho configura os cinco eventos documentados do Pool: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse` e `Stop` (nomes no formato
do Claude, payload JSON em snake_case na stdin, verificado contra a CLI
v1.0.16 do Poolside). Instalações locais usam o comando nativo `ai-memory hook`, de modo que as
operações de arquivo documentadas de `tool_name`/`tool_input` do Pool respeitem `[capture]
ignore_paths` e formatos desconhecidos de payload de ferramenta de arquivo degradem para captura
apenas de metadados.

O Pool não possui um evento real de fim de sessão; `Stop` é uma fronteira de turno. Após o
turno final, feche a sessão explicitamente; use o id exato quando várias
sessões do Pool estiverem abertas no mesmo projeto:

```bash
ai-memory finalize-session --agent pool
ai-memory finalize-session --agent pool --session-id <uuid>
```

O Pool tolera a saída padrão do hook, mas a injeção de contexto visível ao modelo a partir da
saída padrão de `SessionStart` não foi demonstrada, portanto o hook de início de sessão captura
apenas e nunca busca o handoff (de uso único). Recupere o handoff de uma
sessão anterior por meio da ferramenta MCP `memory_handoff_accept`. Nenhum cliente
de primeira classe em `install-mcp` e nenhum workstream gerenciado (`ai-memory run pool`) são
oferecidos: o contrato nativo de armazenamento de sessões do Pool não foi demonstrado, conforme
[contribuições de harnesses gerenciados](managed-harness-contributions.md).

### ZCode (z.ai)

O ZCode configura hooks de ciclo de vida no bloco raiz `hooks` de
`~/.zcode/cli/config.json`, o mesmo arquivo que contém os outros registros da
CLI. `install-hooks --agent zcode` (alias `zai`) mescla as entradas do ai-memory
nesse bloco em torno de quaisquer hooks de terceiros que você já possua, e é
idempotente: executar novamente remove apenas as entradas do próprio ai-memory (marcadas por
`statusMessage: "ai-memory capture"`) e as reescreve no local.

```bash
ai-memory install-hooks --agent zcode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Preview the exact hooks block without writing anything:
ai-memory install-hooks --agent zcode \
    --server-url "http://homelab:49374"
```

As entradas têm formato exec `{"type": "process", "command": <ai-memory>, "args":
[…]}`. O ZCode as inicia com o JSON do evento na stdin e sem shell, o que o
comando nativo `ai-memory hook` lê diretamente, aplicando-se o spool local, autenticação
bearer e exclusões de `[capture] ignore_paths`. Cada chave emitida pertence ao
esquema de hook documentado do ZCode (`type`, `command`, `args`, `enabled`,
`timeoutMs`, `statusMessage`); o ZCode descarta entradas com chaves não documentadas,
portanto nenhuma é emitida. Seis gatilhos documentados são configurados: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure` e
`Stop` (verificado em tempo real contra o motor integrado v0.16.5).
`PostToolUseFailure` dispara *em vez de* `PostToolUse` quando uma ferramenta falha e
é encaminhado para o mesmo canal de captura com o erro preservado como o
resultado da observação. `PermissionRequest` deliberadamente não é instalado: sua
cadeia de hooks concorre com o cliente interativo de permissão, e uma decisão rápida do cliente
aborta o hook perdedor, tornando a captura passiva desse evento instável por
design.

O ZCode injeta a saída padrão de `SessionStart` como contexto do modelo
(`hookSpecificOutput.additionalContext`), portanto, diferentemente do Pool, Zero e Grok, o
handoff da sessão anterior é entregue automaticamente no início da sessão. Não
há evento real de fim de sessão (`Stop` dispara no fim de cada turno), portanto feche
sessões concluídas explicitamente; use o id exato quando várias sessões do ZCode estiverem
abertas no mesmo projeto:

```bash
ai-memory finalize-session --agent zcode
ai-memory finalize-session --agent zcode --session-id <uuid>
```

Nenhum cliente de primeira classe em `install-mcp` e nenhum workstream gerenciado
(`ai-memory run zcode`) são oferecidos ainda.

### GitHub Copilot CLI

O Copilot CLI mantém sua configuração em `~/.copilot` (ou `$COPILOT_HOME`).
`install-mcp --client copilot-cli` mescla a entrada do servidor remoto em
`mcp-config.json` lá, e `install-hooks --agent copilot-cli` grava
`hooks/ai-memory.json` no formato autônomo `{"version": 1, "hooks": {…}}` do Copilot,
mesclando em torno de quaisquer entradas de terceiros já existentes em cada arquivo:

```bash
ai-memory install-mcp --client copilot-cli --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent copilot-cli --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Consulte [o guia de MCP](mcp-install.md#github-copilot-cli) para a entrada exata e
as alternativas `.mcp.json` / `.github/mcp.json` no nível do projeto.

Os eventos são configurados com nomes em PascalCase (`SessionStart`,
`PreToolUse`, …), o que faz o Copilot enviar a carga útil snake_case compatível com
o VS Code/Claude (`session_id`, `cwd`, `tool_name`, `tool_input`,
`tool_result`), portanto a extração existente do ai-memory se aplica e comandos nativos
impõem `[capture] ignore_paths`. Dez eventos estão conectados, os mesmos do
Claude Code, incluindo `PostToolUseFailure`, que o Copilot dispara em vez de
`PostToolUse` quando uma ferramenta falha. As entradas ficam diretamente em cada array de eventos sem `matcher`: o Copilot
rejeita `matcher` em `SessionStart`, `Stop`, `SessionEnd` e nos eventos de
subagente, e omiti-lo em outros lugares significa "todas as invocações". A saída da ferramenta é
lida de `tool_result.text_result_for_llm`.

O hook `SessionStart` entrega o handoff da sessão anterior: o Copilot lê um
`additionalContext` de nível superior da stdout de `SessionStart` (não o envelope
`hookSpecificOutput` do Claude Code), e o hook imprime exatamente isso, ou `{}` quando
nada estiver pendente.

`--scope project` não é compatível intencionalmente para o Copilot CLI. Os arquivos de
hooks de repositório do Copilot (`.github/hooks/*.json`) são versionados, compartilhados com a
equipe e carregados pelo agente de nuvem do Copilot, enquanto as entradas de hooks do ai-memory
carregam os caminhos absolutos de executável e diretório de dados desta máquina; commitá-los
apontaria cada colega de equipe para a instalação e servidor de uma única pessoa.
`ai-memory run copilot` (alias `copilot-cli`) inicia o Copilot CLI como um
harness de workstream gerenciado (consulte [workstreams gerenciados](managed-workstreams.md));
`install-mcp --client copilot` continua sendo o cliente do VS Code Copilot.

### Factory Droid

O Droid (`droid`) mantém sua configuração de usuário em `~/.factory`. `install-mcp --client
droid` (alias `factory-droid`) mescla a entrada do servidor em `mcp.json` lá,
e `install-hooks --agent droid` mescla os hooks de ciclo de vida em `hooks.json`,
mantendo todas as entradas que você já possui em qualquer um dos arquivos (#1240):

```bash
ai-memory install-mcp --client droid --apply \
    --server-url "http://homelab:49374/mcp"
ai-memory install-hooks --agent droid --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

A entrada MCP é `{"type": "http", "url": …}` sob `mcpServers`. O Droid
expande `${NAME}` em `headers`, mas nunca em `url`, portanto a URL é gravada
literalmente. Quando o servidor tem um token bearer, a entrada recebe
`"headers": {"Authorization": "Bearer ${AI_MEMORY_AUTH_TOKEN}"}` em vez
do token, portanto exporte `AI_MEMORY_AUTH_TOKEN` no shell que inicia o droid:
o Droid falha essa conexão, nomeando a variável, quando ela não está definida. O
`.factory/mcp.json` de um projeto também funciona (passe-o com `--config-file`), mas
a documentação do Droid recomenda manter cabeçalhos de autenticação fora desse arquivo com commit. Sem um
token, `droid mcp add ai-memory http://127.0.0.1:49374/mcp --type http` grava
a mesma entrada.

`~/.factory/hooks.json` contém o próprio mapa de eventos, sem encapsulador `hooks`.
O Droid envia JSON no formato do Claude Code na stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_response`), de modo que cada entrada executa o comando nativo
`ai-memory hook --event … --agent droid`; não há pacote de scripts,
e aplicam-se o spool local, autenticação bearer e `[capture] ignore_paths`. As
ferramentas `Read`, `Create`, `Edit`, `ApplyPatch` (um envelope de patch no estilo Codex),
`LS`, `Grep`, `Glob` e `Execute` do Droid são mapeadas para as famílias de arquivo, busca e
shell usadas por essas exclusões, verificadas contra os esquemas de ferramentas do Droid 0.240.0.
Cargas úteis do Droid não carregam ID de chamada de ferramenta. Sete eventos são configurados:
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`,
`Stop` e `SessionEnd`. `SubagentStop` é omitido porque o Droid não tem
`SubagentStart` para parear com ele.

O Droid lê a chave `hooks` de `~/.factory/settings.json` apenas enquanto
`hooks.json` não existir. Se seus hooks residem lá, `--apply` recusa-se a
criar `hooks.json`, o que os desativaria, e informa isso. Mova
aquele objeto `hooks` para `~/.factory/hooks.json` e execute o comando novamente, ou
mescle as entradas que `ai-memory install-hooks --agent droid` imprime (sem
`--apply`) sob a chave `hooks` de `settings.json` manualmente.

O hook `SessionStart` entrega o handoff da sessão anterior por meio de
`hookSpecificOutput.additionalContext`, que o Droid adiciona à sessão, e
o próprio `SessionEnd` do Droid fecha a sessão, de modo que `finalize-session` não é
necessário. Inicie uma nova sessão do droid após a instalação para que ele carregue os hooks.
`ai-memory setup --agent droid` executa ambos os instaladores; o `setup` não detecta
o Droid por conta própria, e nenhum workstream gerenciado (`ai-memory run droid`) é oferecido.

### Meta Muse Code

O Muse Code (`muse`, também instalado como `muse-code`) mantém suas configurações de usuário em
`${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, um único arquivo tanto para seus
hooks quanto para seus servidores MCP. `install-hooks --agent muse` (aliases `muse-code`,
`musecode`) mescla hooks de ciclo de vida sob sua chave `hooks` e
`install-mcp --client muse` mescla a entrada MCP, mantendo
`"schema_version": 1`, todas as outras chaves e todas as entradas que você já possui:

```bash
ai-memory install-mcp --client muse --apply
ai-memory install-hooks --agent muse --apply
```

O Muse falha todos os comandos na inicialização quando `settings.json` não possui
`schema_version`, portanto ambos os instaladores adicionam `"schema_version": 1` quando
estiver ausente e nunca reescrevem um valor existente. Cada `--apply` grava um backup
ao lado do arquivo quando o altera, e executá-lo novamente não altera nada.

O Muse envia JSON no formato do Claude Code na stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_use_id`, `tool_response`), portanto cada entrada
executa o comando nativo `ai-memory hook --event … --agent muse` com um matcher
vazio e um `timeout` de 30 segundos. Não há pacote de scripts. O Muse executa hooks
com um ambiente limpo (`HOME`, `PATH`, `PWD` e suas próprias variáveis `MUSE_*`),
portanto a URL do servidor e o diretório de dados são gravados em cada
comando, e variáveis `AI_MEMORY_*` exportadas no seu shell não chegam aos
hooks; execute o instalador novamente após alterar qualquer um deles. O spool local,
autenticação bearer e `[capture] ignore_paths` se aplicam. As ferramentas `read_file`,
`write_file`, `edit_file`, `apply_patch`, `search`, `bash`, `monitor`,
`web_fetch` e de memória do Muse mapeiam para as famílias de arquivo, busca e shell
que essas exclusões usam, e um `path` relativo é resolvido contra o `cwd` da sessão.
Um caminho de memória no escopo de `project` fica sob `.agents/memory/`; os
escopos de memória pessoal ficam fora do repositório e são armazenados apenas
como metadados enquanto um marcador de `[capture]` estiver ativo.

Oito eventos são configurados: `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `PostToolUseFailure` (o Muse o dispara em vez de `PostToolUse`
quando uma ferramenta falha), `PreCompact`, `Stop` e `SessionEnd`. `SubagentStart` e
`SubagentStop` são omitidos: o Muse os dispara a cada turno para seus observadores
de lembrete integrados, cada um sob seu próprio id de sessão, de modo que configurá-los
registraria várias sessões vazias por prompt. O hook `SessionStart` entrega o
handoff da sessão anterior por meio de `hookSpecificOutput.additionalContext`,
que o Muse adiciona à sessão como contexto de desenvolvedor, e o próprio `SessionEnd`
do Muse encerra a sessão, de modo que `finalize-session` não é necessário. Sem
handoff para entregar, os hooks `SessionStart` e `UserPromptSubmit` não imprimem
nada, pois o Muse adiciona essa saída padrão ao contexto do modelo também.

A entrada MCP é um stdio `ai-memory mcp-bridge --agent muse` com
`"mode": "optional"` (o padrão `required` do Muse aborta a execução quando um servidor
não consegue iniciar). O Muse fornece aos seus servidores stdio a variável `MUSE_SESSION_ID`, e a ponte
a encaminha, de modo que o Muse é um cliente com reconhecimento de sessão: chamadas de memória sem escopo
seguem o próprio projeto da sessão. Uma entrada streamable-HTTP não carregaria id de
sessão. A entrada vai sob `mcpServers`, ou sob o mapa legado `mcp_servers`
quando o arquivo já possuir um, e nunca sob ambos: o Muse ignora todos os servidores
MCP quando ambos os mapas estão presentes, portanto `--apply` recusa um arquivo que já
tenha ambos e o `ai-memory doctor` relata isso. A entrada indica o diretório de dados
da instalação (`--data-dir`) porque a ponte inicia no ambiente limpo do Muse,
e a ponte lê o token de lá; um token passado explicitamente com `--auth-token`
vai no `env` da entrada em vez disso, portanto trate o arquivo como privado nesse caso.
`ai-memory` deve estar no `PATH` com o qual o Muse inicia.

As skills de roteamento do ai-memory não precisam de etapa extra: o Muse lê
`~/.agents/skills`, que o `install-skills` grava, e lê `AGENTS.md`
primeiro entre os arquivos de instrução. `ai-memory setup` detecta o Muse pelo
seu diretório de configuração ou de dados e executa todos os três instaladores;
`ai-memory setup --agent muse` o indica explicitamente. O Muse também lê o
`.muse/hooks.json` de um projeto em um espaço de trabalho confiável; o ai-memory não o
grava, porque seus comandos contêm os caminhos desta máquina. Nenhum workstream gerenciado
(`ai-memory run muse`) e nenhum backfill de transcrição a partir de
`${XDG_DATA_HOME:-~/.local/share}/muse/sessions` foram reivindicados ainda.

### CodeBuddy Code

O CodeBuddy Code (`codebuddy`), agente de código da Tencent, lê os hooks de ciclo
de vida a partir da chave `hooks` de `~/.codebuddy/settings.json`.
`install-hooks --agent codebuddy` (alias `code-buddy`) mescla as entradas do
ai-memory lá, mantendo suas outras configurações e quaisquer hooks que você já possua
(#1299):

```bash
ai-memory install-hooks --agent codebuddy --apply
```

Sem `--apply`, o comando imprime o bloco para mesclar manualmente, e
`ai-memory setup-agent --agent codebuddy` imprime o mesmo bloco para um host
que executa o CodeBuddy contra um servidor remoto. Executar `--apply` novamente não
altera nada.

O CodeBuddy envia JSON no formato do Claude Code na stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_use_id`, `tool_response`) com os nomes de ferramentas
do Claude Code, portanto cada entrada executa o comando nativo
`ai-memory hook --event … --agent codebuddy` com um matcher vazio.
Não há pacote de scripts; o spool local, a autenticação bearer e
`[capture] ignore_paths` se aplicam. Sete eventos são conectados: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop` e
`SessionEnd`.

O hook `SessionStart` entrega o handoff da sessão anterior por meio de
`hookSpecificOutput.additionalContext`, que o CodeBuddy anexa à
sessão. Sem nada a entregar, os hooks `SessionStart` e `UserPromptSubmit`
não imprimem nada, pois o CodeBuddy anexa a saída deles também. Uma sessão interativa
pode iniciar sem um `cwd` na carga útil; o hook usa então
`CODEBUDDY_PROJECT_DIR`, que o CodeBuddy define para seus hooks. Quando nenhum deles
nomeia um diretório, o hook ainda registra o evento, mas não busca nenhum handoff,
porque o servidor não pode determinar qual handoff de projeto reivindicar; o
handoff permanece aberto para `memory_handoff_list` e `memory_handoff_accept`.

`ai-memory setup` conecta o CodeBuddy quando `~/.codebuddy` existe, e
`ai-memory uninstall` remove as entradas. Não há cliente `install-mcp`
para o CodeBuddy ainda, nenhum backfill de transcrição, nenhum encerramento de sessão para
execuções headless de `codebuddy -p` e nenhum workstream gerenciado. Esta integração segue
a documentação de hooks do CodeBuddy e as cargas úteis relatadas na v2.164 até
v2.167; os mantenedores ainda não a executaram de ponta a ponta. Inicie uma nova
sessão do CodeBuddy após a instalação para que ele carregue os hooks.

### WorkBuddy

O WorkBuddy (`workbuddy`), o agente de código da Tencent, lê os hooks de ciclo
de vida da chave `hooks` de `~/.workbuddy/settings.json` e os servidores MCP do
mapa `mcpServers` de `~/.workbuddy/mcp.json`. Ambos os instaladores imitam seus
equivalentes Tencent CodeBuddy e Claude Code:

```bash
ai-memory install-hooks --agent workbuddy --apply
ai-memory install-mcp --client workbuddy --apply
ai-memory install-skills --scope global --agent workbuddy
```

Sem `--apply`, cada comando imprime o bloco a mesclar manualmente, e
`ai-memory setup-agent --agent workbuddy` imprime o bloco de hooks para um host
que roda o WorkBuddy contra um servidor remoto. Rodar `--apply` de novo não muda
nada.

O WorkBuddy adota o contrato de hook no formato Claude Code do CodeBuddy: ele
envia `session_id`, `cwd`, `tool_name`, `tool_input` e `tool_response` no stdin
com os nomes de ferramenta do Claude Code, então cada entrada roda o comando
nativo `ai-memory hook --event … --agent workbuddy` com um matcher vazio. Não há
pacote de scripts; o spool local, a autenticação por bearer e
`[capture] ignore_paths` se aplicam. Seis eventos são conectados: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact` e `Stop`.

O hook `SessionStart` entrega o handoff da sessão anterior por meio de
`hookSpecificOutput.additionalContext`, que o WorkBuddy anexa à sessão. Sem nada
a entregar, os hooks `SessionStart` e `UserPromptSubmit` não imprimem nada, já
que o WorkBuddy também anexa a saída deles.

**O WorkBuddy nunca dispara um evento `SessionEnd`.** `Stop` é um limite de
turno, não um fim de sessão, então uma sessão WorkBuddy ociosa permanece aberta.
Encerre-a com `ai-memory finalize-session --agent workbuddy` após o turno final;
isso grava a página de resumo e passa o bastão para a próxima sessão. Não há
workstream gerenciado nem backfill de transcrição.

A entrada MCP usa o formato HTTP do Claude Code (`type: "http"`, `url`, `headers`
opcional) sob `mcpServers`. Quando o bearer da instalação vem de
`[auth].bearer_token_file`, `install-mcp` grava a entrada stdio do `mcp-bridge`
sem sessão, de modo que o token nunca entra em `mcp.json`; não se assume um
cliente com id de sessão próprio, então passe `workspace` e `project` explícitos
nas chamadas MCP com escopo de projeto.

**O novo servidor só é utilizável depois que o WorkBuddy confia nele.** Após
`install-mcp --apply`, abra as configurações de **conector personalizado**
(自定义连接器) do WorkBuddy e aprove o servidor MCP `ai-memory` recém-adicionado.
Até ser confiável, a entrada fica em `mcp.json`, mas as ferramentas do ai-memory
não aparecem na sessão.

As skills vão para `~/.workbuddy/skills` (global) ou `.workbuddy/skills`
(projeto). `ai-memory setup` conecta o WorkBuddy quando `~/.workbuddy` existe, e
`ai-memory uninstall` remove as entradas de hook. Inicie uma nova sessão
WorkBuddy após instalar para que ela carregue os hooks.

### Hermes Agent (Nous Research)

O Hermes declara hooks de ciclo de vida no bloco `hooks:` de
`~/.hermes/config.yaml` (`$HERMES_HOME/config.yaml`). `install-hooks --agent
hermes` (alias `hermes-agent`) imprime o bloco para colar lá:

```bash
ai-memory install-hooks --agent hermes --server-url "http://homelab:49374"
```

O Hermes divide cada `command` configurado com `shlex.split` e o executa
**sem shell**, passando o JSON do evento na stdin, de modo que a entrada gerada invoca
o comando nativo `ai-memory hook` diretamente (o mesmo formato exec que o Zero e
o ZCode usam) e não há pacote de scripts para preparar. O ai-memory nunca grava o
arquivo, inclusive com `--apply`: `~/.hermes/config.yaml` é um YAML que você também edita, e
o Hermes restringe hooks de usuário sob sua própria confirmação de aceitação (`hooks_auto_accept`).
Cole o bloco, deixe o Hermes aceitá-lo e reinicie o Hermes e seu gateway.

O Hermes executa um hook apenas quando seu par exato `(event, command)` estiver no
`shell-hooks-allowlist.json` daquele home, ou quando `hooks_auto_accept`,
`--accept-hooks` ou `HERMES_ACCEPT_HOOKS` o aprovar. Cada perfil
(`~/.hermes/profiles/<name>`) é um home próprio com sua própria allowlist, portanto
colar o bloco no `config.yaml` de um perfil não é suficiente: aprove-o
lá uma vez com `hermes -p <name> --accept-hooks chat`, ou execute `hermes -p
<name>` e aceite a confirmação. Um gateway não tem terminal para exibir prompts, portanto ele
ignora um hook não aprovado e registra `not allowlisted — skipped` em cada evento.
`ai-memory doctor` verifica a raiz do home e cada perfil e nomeia cada
perfil cujos hooks do ai-memory não estão aprovados. Ele lê ambos os arquivos e nunca
os altera, e não consegue visualizar um `HERMES_ACCEPT_HOOKS` definido apenas no
ambiente do gateway.

Seis eventos são configurados, cada um com um timeout de 20 segundos:

| Hermes event | ai-memory event | What it does |
| --- | --- | --- |
| `pre_tool_call`, `post_tool_call` | `pre-tool-use`, `post-tool-use` | Observações de ferramentas, títulos de famílias de ferramentas e imposição de `[capture] ignore_paths`. |
| `on_session_start` | `session-start` | Registra o início. O Hermes ignora a saída desse hook, portanto nenhum handoff é buscado aqui. |
| `pre_llm_call` | `user-prompt-submit` | Captura o prompt de `extra.user_message` (apenas partes de texto, nunca o histórico da conversa). A cada turno da sessão primária, verifica se há um handoff pendente e o imprime como `{"context": "..."}`, que o Hermes adiciona àquele turno. Isso também funciona após a retomada de uma sessão nativa do Hermes. |
| `on_session_end` | `stop` | Uma fronteira de turno: o Hermes o dispara após cada turno, não no fim da sessão. |
| `on_session_finalize` | `session-end` | Encerra a sessão na saída da CLI, em `/new` ou no desligamento do gateway. Após um `/new` no gateway, apenas a sessão finalizada é fechada. |

Um filho de `delegate_task` executa como uma sessão Hermes própria (`platform`
`subagent`). Seus eventos de ciclo de vida são descartados: seu objetivo não é um prompt de usuário,
seu primeiro turno não deve assumir o handoff e nada jamais o finalizaria.
Seus eventos de ferramentas ainda são capturados.

Uma sessão de gateway que expira ociosa nunca é finalizada pelo Hermes. Feche-a
a partir do diretório do projeto da sessão, ou nomeie-a com `--session-id`:

```bash
ai-memory finalize-session --agent hermes
ai-memory finalize-session --agent hermes --session-id <id>
```

O Hermes relata o diretório do seu próprio processo como o `cwd` da carga útil; para um gateway,
isso é onde quer que o gateway tenha iniciado. O hook roteia por `TERMINAL_CWD`
em vez disso, a variável pela qual o Hermes exporta `terminal.cwd`, quando ela indica um
diretório absoluto existente. Um gateway multiplexador atende a todos os perfis a partir
do processo do perfil padrão e exporta apenas o `terminal.cwd` do perfil
padrão, de modo que dentro de um gateway a variável é usada apenas para eventos que o
perfil `default` disparou. Eventos de outro perfil mantêm o diretório do processo,
a menos que o home do próprio perfil no Hermes (`~/.hermes/profiles/<name>`, sob o
`HERMES_HOME` do gateway quando definido) contenha um `.ai-memory.toml`; então o
hook os roteia por esse marcador. É assim que os bots de um gateway alcançam o
projeto para o qual trabalham:

```toml
# ~/.hermes/profiles/sc-seo/.ai-memory.toml
workspace = "default"
project = "smart-compost"
```

O marcador é um marcador local comum, de modo que suas exclusões de `[capture]` e
perfil `server` governam os eventos daquele perfil. Um perfil sem marcador é mantido
onde estava. Uma chamada de ferramenta ainda pode mudar para outro checkout
quando sua localização absoluta comprovar isso: o `workdir` do `terminal`, o `path` de uma ferramenta
de arquivo ou o `path` de `search_files`. Sem nenhum dos dois, um evento permanece com o
diretório do processo e, sob `capture_mode = allowlist`, é capturado apenas se
esse diretório contiver um marcador. Consulte [marker-file.md](marker-file.md) para as
regras de roteamento.

O provedor de memória da comunidade [`ai-memory-hermes-plugin`](https://github.com/MrLuciano/ai-memory-hermes-plugin)
também envia prompts e encerramentos de sessão e assume o handoff.
Quando ele está instalado em `$HERMES_HOME/plugins/ai-memory`, `install-hooks` e
`setup-agent` imprimem apenas os dois eventos de ferramenta, de modo que prompts não sejam capturados duas vezes
e o handoff não seja assumido duas vezes. Se você instalar o plugin posteriormente, exclua
as quatro entradas de ciclo de vida. A verificação pergunta apenas se aquele diretório existe
onde o comando é executado: um plugin desativado com `hermes plugins disable`, mas
mantido no local, ainda seleciona apenas os eventos de ferramenta, e um plugin instalado
sob o `HERMES_HOME` de um perfil é visto apenas quando o comando é executado com aquele
`HERMES_HOME`. Ajuste o bloco colado manualmente em qualquer um dos casos.

Para MCP, `ai-memory install-mcp --client hermes` imprime uma entrada `mcp_servers:`
para o mesmo arquivo (consulte [mcp-install.md](mcp-install.md#hermes-agent)). O Hermes
não lê instruções de servidores MCP, portanto instale também as skills:
`ai-memory install-skills --scope global --agent hermes` as grava em
`~/.hermes/skills`. Nenhum workstream gerenciado (`ai-memory run hermes`) é reivindicado.

### OpenCode

```bash
# Host-side commands probe the installed executable and select V1/V2.
ai-memory install-mcp --client opencode --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent opencode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Docker cannot inspect the host executable. Generate V1 artifacts explicitly:
docker run --rm akitaonrails/ai-memory:latest \
    setup-agent --agent opencode --opencode-dialect v1 --to /tmp/unused \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

O OpenCode 1.x executa cada subagente `task` em uma sessão filha, e a ferramenta de tarefa pode
retomar uma anterior pelo seu `task_id`. O plugin reconhece uma sessão filha a partir
do pai que o OpenCode relata para ela: um subagente nunca reivindica o handoff pendente, e
seu término, incluindo aquele que o plugin publica quando o OpenCode sai, não deixa nenhum handoff
para trás.

Reinicie o OpenCode após instalar ou alterar o plugin; plugins são
carregados na inicialização. A grafia genérica de cliente/agente `opencode` executa o
`opencode --version` instalado diretamente, aceita apenas a versão principal 1 ou 2, e seleciona o
plugin e o esquema MCP correspondentes. Sob `ai-memory run`, a busca do executável, a sondagem de versão
e a inicialização compartilham o mesmo ambiente capturado mais as substituições de `--env` / `--env-file`.
Uma saída malformada, um tempo limite ou uma versão principal futura incompatível falha com
um erro acionável em vez de adivinhar.

O OpenCode 1 e o 2 carregam plugins do mesmo diretório `~/.config/opencode/plugins/`,
e o OpenCode 2 recusa o plugin V1 com "Plugin must export a
default definition with an id and an effect or setup function". Depois de atualizar
o próprio OpenCode para a 2.x, execute `ai-memory install-hooks --agent opencode --apply`
de novo (ou `ai-memory upgrade`, que sonda a versão principal instalada do mesmo modo):
ele grava `ai-memory-opencode2.ts` e remove o `ai-memory.ts` V1 que ele possui.

Uma camada de compatibilidade com o Claude Code dentro do OpenCode (como o
`claude_code.hooks` do oh-my-opencode) também pode executar os hooks de `~/.claude/settings.json`. No
Windows nativo, essas entradas usam a forma exec do Claude Code: `command` é o caminho simples
para `ai-memory.exe` e os argumentos ficam em `args`, que o Claude Code
inicia sem shell, de modo que um caminho com espaços está correto ali. Uma camada que
executa `command` por meio de um shell e descarta `args` divide esse caminho no primeiro
espaço e falha com `unrecognized subcommand`. Desative os hooks do Claude dela; o
plugin do OpenCode já captura esses eventos.

### Compatibilidade com o OpenCode V2

O plugin gerado tem como alvo a API de eventos do OpenCode 2.0.10+ (verificado com
2.0.14). O OpenCode V2 executa um serviço de longa duração por trás de cada CLI, portanto fechar um
terminal não é o fim de uma sessão: cada turno raiz concluído, em vez disso, grava um
checkpoint determinístico (sem chamada de LLM) de `sessions/<id>.md` e renova o
handoff automático da sessão, mantendo um bastão aberto por sessão ativa. A próxima
sessão assume o checkpoint mais recente de uma sessão que não capturou nada por
dez minutos. Nada distingue um terminal fechado de uma sessão paralela ainda em
execução no mesmo diretório, portanto esta é uma heurística: uma sessão em uso mantém seu
bastão, o turno de uma sessão nunca encerra o bastão de outra sessão ativa, e uma
reivindicação encerra apenas bastões mais antigos de sessões silenciosas. Uma chamada de ferramenta ou de modelo que
execute por mais de dez minutos sem um evento capturado parece silenciosa. O contexto de inicialização é assumido uma vez por sessão
raiz e retido em cada solicitação posterior de modelo; sessões filhas nunca o reivindicam
nem publicam um bastão.

O texto do assistente permanece opcional, como para Claude Code e Codex: defina
`capture_assistant = true` no servidor e instale com
`ai-memory install-hooks --agent opencode --capture-assistant --apply`; a
grafia genérica verifica primeiro se o executável instalado é o V2. O
plugin entrega o último texto concluído ao hook nativo, que o higieniza e
limita antes que ele alcance o spool ou a rede; o trecho então viaja no
handoff automático da próxima sessão. Um re-apply simples preserva a opção ativada.

Para um `opencode.jsonc` com comentários, visualize com `install-mcp --client opencode2` e
mescle a entrada manualmente no objeto `mcp.servers` existente: o caminho de aplicação
grava JSON estrito.

As grafias `opencode2`, `opencode-v2` e `open-code2` são aliases de compatibilidade
legados que forçam os contratos do V2 sem sondagem. Eles compartilham o diretório de
configuração e o armazenamento de sessões genéricos do OpenCode e não são um harness
separado. Prefira a grafia genérica `opencode` para instalações normais:

```bash
# Host-side generic commands auto-detect V2; compatibility aliases force it.
ai-memory install-hooks --agent opencode2 --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Docker cannot inspect the host executable. Generate V2 artifacts explicitly:
docker run --rm akitaonrails/ai-memory:latest \
    setup-agent --agent opencode --opencode-dialect v2 --to /tmp/unused \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

O V2 aninha servidores sob `mcp.servers` e usa uma API de plugin diferente. Um
`--apply` bem-sucedido remove o plugin irmão incompatível apenas quando seus
marcadores exatos de propriedade do ai-memory coincidirem. Da mesma forma, migra uma entrada MCP
alternativa apenas quando seu endpoint e formato comprovarem a propriedade do ai-memory. A instalação
de hooks e MCP reutiliza seu endpoint e bearer através de uma transição principal; entradas
conflitantes pertencentes ao V1/V2 falham para resolução explícita, enquanto entradas criadas pelo usuário são
preservadas. Repetir qualquer uma das instalações é
idempotente em bytes, e o `uninstall` continua a reconhecer ambos os arquivos
históricos e locais de MCP.

> **O serviço em segundo plano compartilhado do OpenCode V2 tem como padrão a porta 49374, que é
> também o padrão do ai-memory.** Executar ambos nos padrões impede o serviço do OpenCode
> (`Managed service port 49374 ... is already in use`). Mude um dos lados:
> `opencode service set port <free-port>`, ou inicie o ai-memory com
> `--bind 127.0.0.1:<free-port>` (e instalações com `--server-url` correspondente).
> `ai-memory run --jail opencode` evita a colisão de serviço compartilhado usando o
> servidor `--standalone` privado do OpenCode dentro da jail.

**Em um modelo Gemini/Vertex, sirva esquemas seguros para o Gemini.** O OpenCode encaminha
esquemas de ferramentas MCP ao provedor configurado literalmente, e o `Schema` do Google
(parâmetros de `functionDeclaration.parameters` do Vertex/Gemini) aceita apenas um único `type` por
campo. O `schemars` renderiza cada argumento opcional de ferramenta como uma união anulável
(`"type": ["integer", "null"]`), de modo que o provedor o reescreve em `any_of` com
`description` ainda ao lado e o Vertex falha toda a sessão em
`tools/list`:

```
Unable to submit request because `ai-memory_memory_auto_improve` functionDeclaration
`parameters.max_proposals` schema specified other fields alongside any_of.
When using any_of, it must be the only field set.
```

Defina `gemini_safe_schemas = true` no `config.toml`
(`AI_MEMORY_GEMINI_SAFE_SCHEMAS=true`) no servidor, o que recolhe essas
uniões para `"type": "integer"` mais `nullable: true` para todos os clientes, ou anexe
`?flavor=gemini` apenas à URL MCP deste cliente. A validação em tempo de execução permanece inalterada
de qualquer forma. O Gemini CLI e o Antigravity CLI normalizam esquemas no lado do cliente e
não necessitam de nenhum dos dois.

### Oh My Pi / OMP

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client omp \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Extension — write to ~/.omp/agent/extensions/ai-memory-omp.ts.
# If you have the local wrapper installed, prefer `--apply`:
ai-memory install-hooks --agent omp --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Reinicie o OMP após instalar ou alterar a extensão; extensões são
carregadas na inicialização. A CLI do ai-memory aceita `--client omp` (ou
`--client oh-my-pi`) para MCP e `--agent omp` (ou `--agent oh-my-pi`)
para hooks; ambos visam a superfície nativa de integração `.omp` do OMP.

O OMP executa subagentes (a ferramenta task, eval `agent()`, clones `/tan`) dentro do seu
próprio processo e carrega a extensão em cada um. A extensão lê o
agente a partir do qual uma sessão é executada no `ctx.agent` do OMP (OMP 18.3.2 e posteriores) e envia os
eventos de sessão de um subagente com `agent_id`. Um subagente nunca assume o
handoff pendente, seu término não deixa handoff, e um projeto que define
`drop_subagent_captures` o descarta. Chamadas de ferramentas de um advisor permanecem com a
sessão que ele assessora. Um OMP mais antigo não relata `ctx.agent`, portanto cada sessão
lá conta como nível superior.

### Pi

O Pi não lê um `mcp.json` nativo. O ai-memory suporta o Pi através de uma
extensão TypeScript gerada em `~/.pi/agent/extensions/ai-memory-pi.ts`; o
mesmo arquivo captura eventos de ciclo de vida e faz a ponte das ferramentas MCP HTTP do ai-memory para
o Pi com `pi.registerTool`. Quando `PI_CODING_AGENT_DIR` está definido (ele realoca
todo o diretório home `~/.pi/agent` do Pi), a extensão é gravada em
`$PI_CODING_AGENT_DIR/extensions/ai-memory-pi.ts` em vez disso.

As extensões do Pi e do OMP usam nomes de arquivo distintos (`ai-memory-pi.ts` e
`ai-memory-omp.ts`) para que instalar uma nunca sobrescreva a outra. Elas
não são intercambiáveis: apenas a do Pi faz a ponte das ferramentas MCP.

#### OMP profiles

`omp --profile <name>` realoca a home do agente do OMP para
`~/.omp/profiles/<name>/agent`. Aponte os instaladores para o mesmo perfil para que
a extensão e a entrada MCP sejam instaladas naquele diretório:

```bash
ai-memory install-hooks --agent omp --profile work --apply
# Or pass OMP_PROFILE to each installer:
OMP_PROFILE=work ai-memory install-hooks --agent omp --apply
OMP_PROFILE=work ai-memory install-mcp --client omp --apply
```

`--profile` tem precedência sobre `OMP_PROFILE`. A variável legada `PI_PROFILE` é
usada apenas quando `OMP_PROFILE` não está definida. Nomes são aparados e validados usando
as regras do OMP. Um valor de ambiente vazio, um nome apenas com espaços em branco ou `default`
seleciona o perfil padrão; um `--profile ""` explícito é rejeitado.

Um perfil nomeado ignora `PI_CODING_AGENT_DIR`. O perfil padrão o utiliza
a menos que ele aponte para o diretório de perfil nomeado exportado por um processo
OMP pai. `PI_CONFIG_DIR` altera a raiz `.omp` em relação ao seu diretório home usando
as regras de junção de caminhos do OMP; ele não substitui um diretório explícito de agente.
A extensão e o `mcp.json` permanecem no diretório do agente mesmo quando as sessões
se movem para o armazenamento XDG. Consulte [comportamento do adaptador nativo](managed-workstreams.md#comportamento-do-adaptador-nativo)
para os caminhos de sessão.

`uninstall --profile <name>` remove os arquivos de integração do perfil. Ele também
verifica o perfil padrão e os locais legados de `~/.omp`, incluindo quando
`PI_CONFIG_DIR` seleciona uma raiz diferente.

```bash
ai-memory install-hooks --agent pi --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# `install-mcp --client pi` prints this guidance instead of writing mcp.json:
ai-memory install-mcp --client pi --server-url "http://homelab:49374/mcp"
```

Reinicie o Pi após instalar ou alterar a extensão. OMP / Oh My Pi permanece
separado e continua usando os caminhos de `.omp`.

#### Pi codemode

Por padrão, a extensão declara todas as ferramentas do ai-memory ao modelo, com o esquema
incluído. Para alcançá-las pela ferramenta `codemode` do Pi, defina isto
no `config.toml` do ai-memory (ou `AI_MEMORY_PI_TOOL_EXPOSURE=codemode`) e
regenere a extensão:

```toml
pi_tool_exposure = "codemode"
```

```bash
ai-memory install-hooks --agent pi --apply
```

Cada ferramenta passa então a ser registrada com o
[`exposure: "codemode"`](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md#tool-exposure) do Pi:
scripts de codemode podem listá-la e chamá-la, e o modelo deixa de receber o esquema
dela, a menos que você ative a ferramenta por conta própria. Os nomes das ferramentas, os cabeçalhos de sessão
que roteiam cada chamada para o projeto atual e a captura de ciclo de vida
permanecem os mesmos. A saída de pré-visualização e todo `--apply` posterior (inclusive a atualização que o
`ai-memory upgrade` executa) leem a mesma configuração, de modo que regenerar mantém a sua
escolha. Volte para `direct`, ou remova-a, para retornar ao padrão.

### Montagens bind vs docker cp

O subcomando `setup-agent` faz a extração + renderização em uma única etapa
usando uma montagem bind:

```bash
docker run --rm -v "$HOME/.ai-memory:/host" \
    akitaonrails/ai-memory:latest \
    setup-agent --agent claude-code --to /host/hooks \
        --host-prefix "$HOME/.ai-memory/hooks" \
        --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

Isso funciona perfeitamente quando o UID do usuário do contêiner coincide com o UID
do usuário do host (por exemplo, no homelab onde ambos são 1000). Isso **falha no
Docker rootless** e em hosts com `userns-remap` ativado — o
contêiner não pode gravar em um diretório do host pertencente a um UID
fora do mapeamento de namespace de usuário.

O padrão `docker cp` recomendado acima contorna tudo isso
porque o `docker cp` é mediado pelo daemon do docker e gera
arquivos pertencentes ao usuário que executa o comando. Prefira-o como o
padrão; recorra ao `setup-agent` apenas quando souber que sua configuração
do docker não remapeia UIDs.

### Outros clientes MCP

Consulte [**`docs/mcp-install.md`**](mcp-install.md) para o caminho do arquivo de
configuração MCP por cliente e o snippet, ou execute diretamente via:

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client cursor          --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent cursor         --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client claude-desktop  --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client gemini-cli      --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent gemini-cli     --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client qwen-code       --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent qwen-code      --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client antigravity-cli --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent antigravity-cli --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client grok            --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent grok            --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client openclaw        --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent openclaw       --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client kiro-cli        --auth-token "$TOKEN" \
    --server-url "https://memory.example/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent kiro-cli       --auth-token "$TOKEN" \
    --server-url "https://memory.example"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client command-code    --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent command-code   --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client vscode-copilot  --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client zed             --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client zcode           --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client dsh             --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client prime-agent     --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"
```

Cursor, Gemini CLI, Qwen Code, Antigravity CLI, Grok Build CLI, Kiro CLI, Command Code e OpenClaw oferecem suporte a ambos
`install-mcp` e `install-hooks`. O `install-mcp --client grok` do Grok grava
`$GROK_HOME/config.toml` (padrão `~/.grok/config.toml`); seus hooks ficam sob
`$GROK_HOME/hooks` (padrão `~/.grok/hooks`). O `install-hooks --agent grok`
captura eventos de ciclo de vida.
O Grok ignora a stdout de `SessionStart` e descarta um `UserPromptSubmit` permissivo,
portanto esses hooks não aceitam o handoff. `install-hooks --agent grok --apply`
também grava `$GROK_HOME/rules/ai-memory-handoff.md` para que o primeiro turno chame
`memory_handoff_list`. O Grok acessa ferramentas MCP apenas por meio de sua metaferramenta
`use_tool`, de modo que a regra entrega ao modelo a chave de catálogo qualificada
`ai-memory__memory_handoff_list` em vez de deixá-la para `search_tool`. Essa
chamada ocorre antes de qualquer leitura de arquivo, e a regra corrige `workspace`/`project`
a partir de `./.ai-memory.toml` somente quando a lista retornar vazia ou com erro de escopo. O
primeiro `PostToolUse` imprime
`hookSpecificOutput.additionalContext` (handoff pendente, mais um
`[briefing]` ativado). O modelo o vê após o resultado dessa ferramenta. Uma sessão sem
chamada de ferramenta e sem essa regra deixa o handoff para
`memory_handoff_accept`. Claude Desktop, VS Code Copilot, Zed, ZCode e
Prime Agent são somente MCP aqui, portanto você precisará
induzir o modelo a chamar `memory_query` / `memory_handoff_accept` manualmente.
O DeepSeek Harness (DSH) também não possui stdout de hook para transportar o bastão: sua
ponte `@deepseek-ai/dsh-hooks-claude-code` executa `SessionStart` desanexado, de modo que
`install-hooks --agent dsh` captura eventos de ciclo de vida, mas a recuperação de handoff
ocorre por meio de `memory_handoff_list` e depois `memory_handoff_accept`, e sessões finalizadas
se encerram com `ai-memory finalize-session --agent dsh`.
O snippet do Prime Agent referencia `$AI_MEMORY_AUTH_TOKEN` em vez de embutir o
token, portanto exporte essa variável no ambiente onde você inicia `prime-agent`.
Para clientes compatíveis com `install-hooks`, o caminho de captura gerencia
a injeção de handoff no início da sessão ou no equivalente mais próximo do cliente, exceto
pelo comportamento sem stdout do SessionStart do Zero. O Grok entrega no primeiro
`PostToolUse` em vez disso (o Antigravity usa `SessionStart`, ou o `PreInvocation` do primeiro
turno em builds sem ele).

#### Cline CLI and FreeBuff

O Cline CLI é uma integração primária de hooks + MCP:

```bash
ai-memory install-mcp --client cline --apply
ai-memory install-hooks --agent cline --apply
```

O gravador de MCP lê `CLINE_MCP_SETTINGS_PATH` ou usa o padrão
`~/.cline/data/settings/cline_mcp_settings.json`; ele grava os campos de nível superior `type`,
`url` e os campos opcionais `headers`, e seleciona explicitamente `streamableHttp`
em vez do padrão SSE legado do Cline. O gravador de hooks tem como alvo os arquivos `~/.cline/hooks` da CLI;
use `--scope project` para `.cline/hooks` em um checkout. Reinicie o Cline após
a instalação. O modo Yolo desativa os hooks do Cline, e os hooks de início de tarefa são
desanexados, portanto recupere handoffs pendentes por meio de MCP. A convenção separada
de `~/Documents/Cline/Hooks` da extensão do VS Code não é modificada.

O FreeBuff é somente MCP:

```bash
ai-memory install-mcp --client freebuff --apply
```

Isso mescla uma ponte stdio sem sessão sob `mcpServers` em
`~/.agents/mcp.json`. O FreeBuff localiza esse arquivo ao navegar do diretório
atual até o diretório home e já lê `AGENTS.md`. Um token bearer do servidor baseado em
arquivo permanece no diretório de dados do ai-memory; um `--auth-token` explícito
é armazenado no mapa de ambiente da entrada stdio. Nenhum hook de ciclo de vida ou workstream
gerenciado é reivindicado.

#### Clientes sem um gravador `install-mcp`

Os clientes abaixo podem alcançar o ai-memory por meio de streamable HTTP, mas o `install-mcp`
não possui um valor de `--client` para eles, portanto você adiciona a entrada manualmente. Cada receita
segue a documentação própria do cliente na versão indicada. Nenhum deles foi
executado de ponta a ponta contra o ai-memory ainda; a
[matriz de compatibilidade](support-matrix.md) os marca como `MCP-only (documented)`.
Se algum deles não conectar conforme descrito, abra uma issue informando a versão
do cliente.

Todos eles são somente MCP aqui: o ai-memory não instala hooks de ciclo de vida para eles,
portanto não há captura automática nem injeção de handoff. Peça ao agente para
chamar `memory_handoff_begin` antes de você sair e `memory_handoff_accept` quando
você retomar. Nenhum deles envia um id de sessão ao ai-memory, portanto são clientes
estáticos: chamadas no escopo do projeto necessitam de `workspace` e `project`, o que o
snippet de roteamento do `ai-memory install-instructions --target AGENTS.md`
informa ao modelo.

Os exemplos usam o servidor local, `http://127.0.0.1:49374/mcp`. Para um servidor
remoto, altere a URL e adicione o cabeçalho bearer mostrado. Onde o cliente
expande variáveis de ambiente em sua configuração, o exemplo lê o token
a partir de `AI_MEMORY_AUTH_TOKEN`; exporte-a na shell que inicializa o cliente.
Onde o cliente não documenta expansão, o exemplo exibe `<token>`, e o
token deve ser gravado nesse arquivo, portanto mantenha o arquivo privado. Todos
esses clientes suportam MCP HTTP nativamente, de modo que `ai-memory mcp-bridge` não é necessário
para essas receitas manuais.

##### Warp

O Warp lê `~/.warp/.mcp.json` e, após você aprovar, o `.warp/.mcp.json` de um
projeto. Você também pode adicionar o servidor em **Settings > Agents >
MCP servers > + Add**. Um servidor URL aceita `url` e um mapa opcional `headers`,
sem nenhuma chave `type`:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

O Warp não documenta expansão de variáveis de ambiente em `headers`. Omita
`headers` quando o servidor não possuir um token bearer. A CLI separada do Warp Agent
mantém seu próprio arquivo com o mesmo formato de `mcpServers` (`~/.warp_cli/.mcp.json`
no macOS; a documentação não fornece o caminho no Linux). Documentado para Warp
v0.2026.10.07.08.29.
Fontes: <https://docs.warp.dev/agents/capabilities/mcp>,
<https://docs.warp.dev/agents/cli/configuration/>.

##### Kilo Code

A extensão do Kilo Code para VS Code e a Kilo CLI compartilham uma única configuração:
`~/.config/kilo/kilo.jsonc` (ou `kilo.json`) globalmente, ou `kilo.jsonc` /
`.kilo/kilo.jsonc` no projeto. Servidores ficam sob a chave `mcp`, e um
servidor remoto usa `type: "remote"`; o Kilo tenta streamable HTTP primeiro e
recorre a SSE como fallback. O Kilo expande `{env:NAME}` em valores de configuração:

```json
{
  "mcp": {
    "ai-memory": {
      "type": "remote",
      "url": "http://127.0.0.1:49374/mcp",
      "enabled": true,
      "oauth": false,
      "headers": { "Authorization": "Bearer {env:AI_MEMORY_AUTH_TOKEN}" }
    }
  }
}
```

O ai-memory usa um token bearer, não OAuth, portanto `"oauth": false` impede o Kilo de
iniciar um fluxo OAuth. Omita `headers` quando o servidor não possuir um token bearer.
Os arquivos mais antigos `mcp_settings.json` e `.kilocode/mcp.json` não constam
na documentação atual. Documentado para Kilo Code 7.8.8.
Fontes: <https://kilo.ai/docs/automate/mcp/using-in-kilo-code>,
<https://kilo.ai/docs/automate/mcp/using-in-cli>.

##### Goose

O Goose mantém extensões em `~/.config/goose/config.yaml`
(`%APPDATA%\Block\goose\config\config.yaml` no Windows). Execute `goose
configure`, escolha **Add Extension > Remote Extension (Streamable HTTP)**,
informe a URL e, para um servidor com um token bearer, adicione o cabeçalho
de requisição personalizado `Authorization: Bearer <token>`. A entrada resultante tem este formato;
observe que a chave da URL é `uri`:

```yaml
extensions:
  ai-memory:
    type: streamable_http
    name: ai-memory
    enabled: true
    uri: "http://127.0.0.1:49374/mcp"
    headers:
      Authorization: "Bearer <token>"
    timeout: 300
```

O Goose não documenta expansão de variáveis de ambiente em `headers`. Deixe
`headers` vazio (`{}`) quando o servidor não possuir token bearer. Para uma sessão única,
`goose session --with-streamable-http-extension "http://127.0.0.1:49374/mcp"`
carrega o servidor sem gravá-lo (essa flag não aceita cabeçalho). Documentado
para o Goose 1.54.0.
Fontes: <https://goose-docs.ai/docs/guides/config-files>,
<https://goose-docs.ai/docs/getting-started/using-extensions>,
<https://goose-docs.ai/docs/guides/goose-cli-commands>.

##### OpenHuman

O OpenHuman (tinyhumansai/openhuman) adiciona servidores por meio da aba **mcp.json**
da tela de MCP Servers. Esse documento é uma visualização sobre o repositório de
servidores do próprio OpenHuman, não um arquivo editado em disco. Um servidor com `url` é chamado
via streamable HTTP, e `headers` armazena suas credenciais:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

O OpenHuman não documenta expansão de variáveis de ambiente nesse
documento. Ele armazena os valores de cabeçalho no modo write-only e nunca os exibe novamente.
Omita `headers` quando o servidor não possuir token bearer. Documentado para
OpenHuman 0.64.15.
Fontes: <https://tinyhumans.gitbook.io/openhuman/features/integrations/mcp-and-skills>,
<https://tinyhumans.gitbook.io/openhuman/developing/architecture/mcp-registry>.

##### Aider

O Aider não possui suporte a MCP em nenhuma versão (verificado na 0.86.2 e no seu
histórico de lançamentos), portanto não há receita. A solicitação do recurso é
[Aider-AI/aider#3314](https://github.com/Aider-AI/aider/issues/3314).

---

## Instalação de hooks sem o docker

Se você só precisa usar o ai-memory *a partir* de uma máquina (ou seja, essa máquina não
executa o servidor), baixe e valide o instalador da release. O instalador então
baixa e valida o arquivo de hooks da release antes de gravar quaisquer scripts:

```bash
installer_base=https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-install-hooks
installer_tmp="$(mktemp -d)"
trap 'rm -rf "$installer_tmp"' EXIT
curl -fsSL "$installer_base" -o "$installer_tmp/ai-memory-install-hooks"
curl -fsSL "$installer_base.sha256" -o "$installer_tmp/ai-memory-install-hooks.sha256"
expected="$(awk 'NR == 1 { print $1 }' "$installer_tmp/ai-memory-install-hooks.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$installer_tmp/ai-memory-install-hooks" | awk '{ print $1 }')"
else
    actual="$(shasum -a 256 "$installer_tmp/ai-memory-install-hooks" | awk '{ print $1 }')"
fi
[ -n "$expected" ] && [ "$actual" = "$expected" ] || { echo "installer checksum mismatch" >&2; exit 1; }
chmod +x "$installer_tmp/ai-memory-install-hooks"
"$installer_tmp/ai-memory-install-hooks" --agent claude-code
rm -rf "$installer_tmp"
trap - EXIT

# Then render the JSON config (still wants `ai-memory` somewhere —
# either via docker as a one-shot, or installed locally):
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent claude-code \
        --hooks-dir "$HOME/.ai-memory/hooks" \
        --server-url "http://homelab:49374" \
        --auth-token "$TOKEN"
```

O instalador de script via curl suporta
`--agent claude-code|codex|command-code|cursor|gemini-cli|kimi-code|kiro-cli|antigravity-cli|grok|opencode|opencode2|openclaw|omp|oh-my-pi|pi`
e `--to <dir>`; `--help` imprime a lista completa de opções. OpenCode,
OpenClaw, OMP / Oh My Pi e Pi não necessitam de extração de scripts porque
`install-hooks` gera arquivos de plugin/extensão em TypeScript para eles
em vez disso. Para o Pi, a extensão gerada também fornece a ponte MCP.

As integrações geradas em TypeScript sobrevivem a um servidor inacessível da
mesma forma que os hooks nativos: uma entrega que falha no nível da rede
(ou recebe 5xx, 408, 425 ou 429) é gravada em `<data_dir>/hook-spool/` no formato exato
que `ai-memory hook-drain` lê, e o próprio plugin esvazia essa fila acumulada
assim que o servidor se torna acessível novamente. Um dia de trabalho de laptop fora
da rede do servidor é, portanto, capturado e não descartado silenciosamente. Cada entrada na fila
carrega uma chave de idempotência, de modo que o esvaziamento do próprio plugin e um
`hook-drain` manual podem concorrer sem ingestão duplicada. Observe que o spool
fica no diretório de dados do *host do agente* (`AI_MEMORY_DATA_DIR` ou o
padrão da plataforma), onde uma instalação nativa do `ai-memory` busca.

Este caminho é livre de atritos quando:
- Você possui curl + bash, mas não o docker
- Você não precisa rodar um servidor ai-memory local (você é cliente de
  um ai-memory em homelab/remoto)

### Caminhos de comando de hook através do limite de contêiner

`install-hooks --apply` prepara os scripts de hook no diretório de dados e
grava seus caminhos absolutos na configuração do agente. Quando a CLI roda
dentro de um contêiner mas o agente roda no host, esses caminhos preparados
seriam caminhos do contêiner que o host não enxerga. Defina
`AI_MEMORY_HOOKS_HOST_ROOT` como o diretório do *host* a partir do qual a
árvore `hooks/` preparada é montada e a configuração renderizada usará caminhos
de comando `<host-root>/<agent>/…` em vez disso. Os wrappers do docker
empacotados (`bin/ai-memory`, `bin/ai-memory.ps1`) encaminham esta variável
automaticamente; você só a define manualmente em configurações customizadas de contêiner.

---

## Execução do ai-memory sem o docker

A maioria dos usuários deve manter o wrapper do docker do Início rápido. Usuários do
Arch Linux dispõem dos [pacotes AUR](#pacotes-nativos-do-arch-linux-aur). Para qualquer
outro host, o `mise` instala um binário de release tagueada diretamente do GitHub,
sem necessidade de toolchain Rust:

```bash
mise use -g github:akitaonrails/ai-memory
```

Isso usa o [backend do GitHub do mise](https://mise.jdx.dev/dev-tools/backends/github.html),
que baixa o arquivo compactado de release correspondente ao seu SO/arquitetura
(`ai-memory-linux-x86_64.tar.gz`, `ai-memory-macos-aarch64.tar.gz`, etc.),
valida seu checksum contra o sidecar `.sha256` publicado, e em seguida o extrai
e coloca o `ai-memory` no seu `PATH`. (O mise também pode verificar atestação
de artefato do GitHub e proveniência SLSA onde um projeto os publica; o fluxo de
trabalho de release do ai-memory não emite nenhum dos dois atualmente, portanto apenas o checksum se aplica.)
Nenhum plugin dedicado do mise ou entrada em registro
é necessário; o backend funciona para qualquer repositório cujos artefatos de
release sigam essa convenção de nomenclatura. Por padrão o mise também retém
a versão mais recente por uma curta janela de segurança
([`minimum_release_age`](https://mise.jdx.dev/dev-tools/github-backend.html)),
portanto uma tag recém-lançada pode resolver para a versão anterior por um dia ou mais;
fixe uma tag exata com `mise use -g github:akitaonrails/ai-memory@1.30.0` para
contornar isso.

`cargo install ai-memory` não está disponível: o nome do crate já está em uso
por um projeto não relacionado no crates.io, assim como `ai-memory-core` (o crate
interno fundamental da workspace). Publicar exigiria renomear pelo menos esse
crate para o registro, uma decisão de nomenclatura que o projeto ainda não tomou.

Compile a partir do código-fonte apenas ao modificar o próprio ai-memory ou ao executar
em uma plataforma não coberta pelas opções acima. No macOS, releases tagueadas também publicam
arquivos compactados nativos `ai-memory-macos-aarch64.tar.gz` e `ai-memory-macos-x86_64.tar.gz`
quando você só precisa da CLI de cliente.

```bash
git clone https://github.com/akitaonrails/ai-memory ~/.ai-memory
cd ~/.ai-memory
cargo build --release --workspace
./target/release/ai-memory init                       # one-time
./target/release/ai-memory serve --transport http \
    --bind 127.0.0.1:49374                            # MCP + hook HTTP server
```

O diretório de dados tem como padrão `~/.local/share/ai-memory` no Linux,
`~/Library/Application Support/ai-memory` no macOS, e o diretório de dados locais
da plataforma no Windows, tipicamente
`%LOCALAPPDATA%\ai-memory`. Substitua com `AI_MEMORY_DATA_DIR=/path`.
`init` em um diretório de dados recém-criado já exige o token bearer que gravou em
`auth-tokens/default`; `AI_MEMORY_AUTH_TOKEN` no ambiente do servidor
o substitui.

#### Opções opcionais do serve

O subcomando `serve` também aceita:

| Flag | Env var | O que faz |
|---|---|---|
| `--enable-web` | `AI_MEMORY_ENABLE_WEB=true` | Monta o navegador web somente leitura e a API JSON `/api/v1`. |
| `--enable-api` | `AI_MEMORY_ENABLE_API=true` | Monta apenas a API JSON protegida e somente leitura `/api/v1` para companheiros e outros clientes não-navegador. |
| `--base-path /wiki` | `AI_MEMORY_BASE_PATH` | Hospeda toda a superfície HTTP (`/mcp`, `/hook`, `/admin/*`, `/api/v1`, `/web`) sob um subcaminho configurável, útil atrás de um proxy reverso que compartilha um nome de host. Segmentos `.` e `..` são rejeitados; caracteres inseguros causam um recuo para a raiz com um aviso. Consulte [`docs/https-via-proxy.md`](https-via-proxy.md#hospedando-sob-um-subcaminho). |
| `--web-slug /web` | `AI_MEMORY_WEB_SLUG` | Onde a UI web é montada dentro do base-path. Padrão `/web`; defina como `/` para montar a UI na raiz do base-path. |
| `--web-ui-dir <path>` | `AI_MEMORY_WEB_UI_DIR` | Serve uma SPA personalizada de `<path>` em vez do navegador integrado. O ai-memory injeta `<base href>` e `<meta name="ai-memory-base-path">` para que a SPA possa construir URLs relativas e chamadas de API sob o prefixo configurado. |
| `--cors-allow-origin <origin>` | `AI_MEMORY_CORS_ALLOW_ORIGINS` (CSV) | Permite que as origens listadas chamem `/api/v1`. A camada é delimitada apenas a essa rota; `/mcp`, `/hook`, `/admin` e `/web` permanecem bloqueados por origem. |
| _(config only)_ | `AI_MEMORY_HOOK_RATE_PER_SEC`, `AI_MEMORY_HOOK_RATE_BURST` | Token bucket opcional de ingestão de hook por ator/sessão. Taxa não definida/`0` o desativa; o burst tem como padrão a taxa (mínimo de um token quando ativado). |

No macOS, consulte [`docs/macos.md`](macos.md); use o arquivo compactado correspondente à sua
arquitetura: `aarch64` para Apple Silicon, `x86_64` para Intel. O
[aplicativo da barra de menus](#aplicativo-da-barra-de-menus-do-macos) é o caminho de GUI autônomo (empacota
o binário, inicializa o LaunchAgent, abre `/web` e status). No Windows, consulte
[`docs/windows.md`](windows.md).
A versão curta: execute os comandos de instalação a partir do mesmo ambiente que
inicializa o agente. Agentes iniciados no WSL2 precisam de caminhos do WSL e hooks POSIX `.sh`.
Agentes nativos do Windows podem usar o `ai-memory-windows-x86_64.zip` tagueado, o
wrapper do Docker Desktop ou uma compilação do fonte. O Claude Code nativo usa a forma
Claude exec com um `ai-memory.exe` real por padrão; o wrapper do Docker para Windows renderiza
outros agentes de script-hook nativos do Windows por meio de comandos de fallback
codificados em PowerShell `.ps1`.

Quando executado a partir do fonte, `install-hooks` encontra os scripts empacotados no
`hooks/` do repositório automaticamente. Arquivos compactados de release extraídos também
descobrem automaticamente o pacote irmão `hooks/` ao lado do binário `ai-memory`:

```bash
./target/release/ai-memory install-hooks --agent claude-code --apply
```

Os hooks leem o token do diretório de dados; passe `--auth-token` apenas para um
servidor cujo token resida em outro local.

(Não é necessário `setup-agent` neste caso — os scripts já residem
no caminho correto do host.)

---

## Níveis de provedor LLM

O ai-memory opera em três níveis de intensidade:

| Nível | O que você obtém | Variáveis de ambiente | Custo |
|---|---|---|---|
| **Zero-LLM** (padrão) | FTS5 + busca declarada manualmente por entidade + busca em grafo, resumos de sessão baseados em regras, auto-handoffs do histórico de prompts + chamadas de ferramentas | (nenhuma) | $0 |
| **+ consolidação por LLM** | O LLM reescreve páginas de sessão como narrativas coerentes; checkpoints de PreCompact; verificação de contradição orientada por LLM | `AI_MEMORY_LLM_PROVIDER=anthropic` + `ANTHROPIC_API_KEY` | ~$0.01–0.05 / sessão |
| **+ Anthropic via assinatura** | Os mesmos recursos de LLM usando uma assinatura Claude Pro/Max em vez de chave de API | `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` + `ANTHROPIC_OAUTH_TOKEN` | Usa sua assinatura do Claude |
| **+ ChatGPT/Codex OAuth** | Os mesmos recursos de LLM usando um login do ChatGPT Pro/Plus em vez de uma chave da OpenAI Platform | `AI_MEMORY_LLM_PROVIDER=openai-oauth` + `ai-memory auth login openai-oauth` | Usa sua assinatura do ChatGPT |
| **+ Reutilização de credenciais Codex** | Os mesmos recursos de LLM usando o login pertencente à CLI do Codex sem copiar ou deter seu token de atualização | `AI_MEMORY_LLM_PROVIDER=codex` + uma CLI do Codex autenticada | Usa sua assinatura do ChatGPT |
| **+ GitHub Copilot** | Os mesmos recursos de LLM usando uma assinatura do GitHub Copilot | `AI_MEMORY_LLM_PROVIDER=copilot` + `ai-memory auth login copilot` ou `COPILOT_GITHUB_TOKEN` | Usa sua assinatura do Copilot |
| **+ Assinatura Cursor** | Os mesmos recursos de LLM usando a CLI do Cursor Agent conectada. Somente leitura `--mode ask`; não passa `--yolo` | `AI_MEMORY_LLM_PROVIDER=cursor` + `agent` no `PATH` (ou `AI_MEMORY_CURSOR_AGENT`) | Usa sua assinatura do Cursor |
| **+ Reranking por LLM** | No máximo uma passada de relevância sobre até 30 candidatos delimitados de busca de projeto/escopos; a ordem normal é preservada em respostas inválidas, falhas, com tempo limite esgotado ou saturação de concorrência | `AI_MEMORY_RERANKER=llm` + qualquer provedor LLM configurado | Uma chamada de LLM por consulta elegível, no máximo quatro simultaneamente |
| **+ Recuperação híbrida** | Adiciona similaridade de cosseno vetorial ao RRF de FTS5 + entidade + grafo. Melhor recall em consultas parafraseadas | `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `OPENAI_API_KEY` (ou `EMBEDDING_API_KEY`) | ~$0.0001 / página no backfill |

### Modelos recomendados (escolhidos como padrões)

Se você definir apenas o provedor, o ai-memory escolhe um padrão sensato:

| Configuração | Padrão | Motivo |
|---|---|---|
| `AI_MEMORY_LLM_PROVIDER=anthropic` | `claude-haiku-4-5` | **Padrão recomendado.** Melhor equilíbrio entre velocidade, moderação e qualidade de classificação. Não é um modelo de raciocínio. Classifica consistentemente regras de projeto duráveis como `kind: rule`. |
| `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` | `claude-sonnet-4-6` | Anthropic via assinatura Claude. Execute `claude setup-token` uma vez e exporte o token impresso como `ANTHROPIC_OAUTH_TOKEN` (ou `CLAUDE_CODE_OAUTH_TOKEN`). Nenhuma `ANTHROPIC_API_KEY` necessária. Mesmo endpoint `/v1/messages`, autenticação Bearer token. Atualmente, a Anthropic recusa esses tokens de aplicativos de terceiros, geralmente como um `429` em cada requisição. |
| `AI_MEMORY_LLM_PROVIDER=openai` | `gpt-5.4-mini` | Alternativa mais rápida e econômica. Mesma confiabilidade de análise; leve sobreclassificação em sessões rasas. |
| `AI_MEMORY_LLM_PROVIDER=openai-oauth` | `gpt-5.5` | Backend ChatGPT/Codex. Execute `ai-memory auth login openai-oauth` uma vez; o ai-memory armazena o token de atualização em `<data_dir>/auth.json` e atualiza tokens de acesso automaticamente. O opcional `AI_MEMORY_LLM_REASONING_EFFORT` (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/`ultra`/`persistent`) é mapeado para o campo nativo de raciocínio de cada provedor; omita-o para manter o padrão do modelo. |
| `AI_MEMORY_LLM_PROVIDER=codex` | `gpt-5.6-luna` | Reutiliza apenas `access_token` e `account_id` do `auth.json` do Codex; a renovação do token é delegada a `codex app-server --stdio`. |
| `AI_MEMORY_LLM_PROVIDER=copilot` | `gpt-5.5` | Backend do GitHub Copilot Chat. O ai-memory armazena um token de usuário do GitHub em `<data_dir>/auth.json`, troca-o por um token de API de curta duração do Copilot e o renova antes de expirar. |
| `AI_MEMORY_LLM_PROVIDER=cursor` | `cursor-grok-4.6-high` | Assinatura Cursor via `agent --print --mode ask --trust`. Sem chave de API e sem `--yolo`. Este id é "Grok 4.6" em `agent --list-models`. Substitua o binário com `AI_MEMORY_CURSOR_AGENT`. Qualquer outro id dessa lista funciona como `AI_MEMORY_LLM_MODEL`. |
| `AI_MEMORY_LLM_PROVIDER=gemini` | `gemini-3.5-flash` | Opção hospedada do Google com um nível gratuito generoso. O ai-memory desativa o raciocínio dinâmico padrão do Gemini 3.5 Flash para que tokens de pensamento ocultos não truncem JSON estrito. Defina `GEMINI_API_KEY` (ou `GOOGLE_API_KEY`). |
| `AI_MEMORY_LLM_PROVIDER=opencode` | `mimo-v2.6-flash` | API em nuvem do [OpenCode](https://opencode.ai). O padrão é o endpoint **Go**, `opencode.ai/zen/go/v1`, um subconjunto de modelos otimizado para custos. Modelos Responses publicados, incluindo GPT-5.6 Luna e GPT-6 Luna, usam `/responses`; modelos Claude e Qwen Messages compatíveis usam `/messages`. MiniMax M3/M2.7 e Qwen3.8 Max usam Messages no Go e Chat Completions no Zen; modelos Go Muse Contributor usam Responses. Alterar a URL base seleciona novamente o transporte e preserva as configurações da requisição. Consulte as tabelas de endpoints do [Go](https://opencode.ai/docs/go/#endpoints) e [Zen](https://opencode.ai/docs/zen/#endpoints). Para o catálogo completo do **Zen**, defina `AI_MEMORY_LLM_BASE_URL=https://opencode.ai/zen/v1` mais um `AI_MEMORY_LLM_MODEL` dele, como `claude-sonnet-5-5`; o id do modelo padrão é o do Go. As requisições identificam o ai-memory por versão e reutilizam um cabeçalho de sessão em tentativas relacionadas. Ambos os endpoints aceitam `OPENCODE_API_KEY` (chave de `opencode.ai/auth`). Alias: `opencode-zen` (histórico); ele seleciona o Go como os outros, e o endpoint é escolhido pela URL base, não pelo alias, sem troca automática de produto de faturamento. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` | `text-embedding-3-small` (1536-dim) | 5× mais econômico do que `-3-large` com perda marginal de recall. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1` | `openai/text-embedding-3-small` via [OpenRouter](https://openrouter.ai) | Usa `EMBEDDING_API_KEY`, caso contrário reutiliza `LLM_API_KEY` ou `OPENAI_API_KEY`, com o cliente de embedding compatível com a OpenAI. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://api.orcarouter.ai/v1` | `openai/text-embedding-3-small` via [OrcaRouter](https://www.orcarouter.ai) | Usa `EMBEDDING_API_KEY`, caso contrário reutiliza `LLM_API_KEY`, com o cliente de embedding compatível com a OpenAI. |
| `AI_MEMORY_EMBEDDING_PROVIDER=voyage` | `voyage-3` (1024-dim) | Recomendação atual de uso geral da Voyage. |
| `AI_MEMORY_EMBEDDING_PROVIDER=google` / `gemini` | `gemini-embedding-001` (768-dim) | Embeddings hospedados pelo Google via `embedContent`. Defina `GEMINI_API_KEY` (ou `GOOGLE_API_KEY`). |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai-compat` | sem padrão; defina o modelo, dimensão e base URL explicitamente | Mecanismos auto-hospedados (Ollama, LM Studio, vLLM). Sem chave por padrão; `EMBEDDING_API_KEY`, ou `LLM_API_KEY`, é enviada como um token bearer quando presente (gateways). Exemplo: `AI_MEMORY_EMBEDDING_BASE_URL=http://localhost:11434/v1`, `AI_MEMORY_EMBEDDING_MODEL=nomic-embed-text`, `AI_MEMORY_EMBEDDING_DIM=768`. Mudar uma configuração existente de `openai`+base-URL para `openai-compat` altera a tripla `{provider, model, dim}` armazenada, portanto execute `ai-memory embed --force` para recriar os embeddings. Modelos assimétricos exigem `AI_MEMORY_EMBEDDING_QUERY_PREFIX` / `AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX`, por exemplo, `nvidia/Nemotron-3-Embed-1B-BF16` espera `query: ` / `passage: `. Consulte [`docs/llm-providers.md`](llm-providers.md) para isso e para Qwen3-Embedding/E5 com ajuste de instruções, que necessitam de um formato diferente (somente consulta). |
| `AI_MEMORY_EMBEDDING_PROVIDER=copilot` | `text-embedding-3-small` (1536-dim) | Reutiliza o login OAuth do provedor LLM `copilot` (`ai-memory auth login copilot`, `COPILOT_GITHUB_TOKEN` ou `GITHUB_COPILOT_API_TOKEN`), sem chave de API separada. Chama o endpoint `/embeddings` do Copilot seguindo o contrato compatível com a OpenAI que o Copilot documenta para chat; o formato exato desse endpoint não é coberto por um teste ao vivo contra o Copilot aqui, portanto trate-o como necessitando de um teste de fumaça com o Copilot real. |

> **O que não recomendamos:** modelos em modo de raciocínio (Claude com extended
> thinking, GPT-o3, variantes "thinking" do Gemini). Eles esgotam o orçamento de tokens em
> raciocínio interno e travam ou emitem respostas vazias com o prompt de consolidação
> em JSON estrito. Desative o raciocínio se você precisar usar um.

#### Credenciais de embedding (`EMBEDDING_API_KEY`)

`EMBEDDING_API_KEY` é opcional e credencia apenas a função de embedding. Defina-a
quando o integrador de embeddings deve se autenticar contra um provedor diferente do
LLM. Caso contrário, os integradores de embedding `openai` e `openai-compat` tomam emprestada a
`OPENAI_API_KEY` ou `LLM_API_KEY` do modelo de chat, que é então enviada para onde quer que
`AI_MEMORY_EMBEDDING_BASE_URL` aponte e é rejeitada com um 401.

Precedência para `AI_MEMORY_EMBEDDING_PROVIDER=openai`:

1. `EMBEDDING_API_KEY`
2. `OPENAI_API_KEY`
3. `LLM_API_KEY`, somente quando `AI_MEMORY_EMBEDDING_BASE_URL` estiver definida

`openai-compat` verifica `EMBEDDING_API_KEY`, depois `LLM_API_KEY`, e permanece
sem chave quando nenhuma das duas estiver definida. `voyage` e `google`/`gemini` não são afetados:
eles leem suas próprias `VOYAGE_API_KEY` e `GEMINI_API_KEY`/`GOOGLE_API_KEY` e
nunca tomam emprestada a chave de outra função. Com `EMBEDDING_API_KEY` não definida, a resolução
de chaves é exatamente o que era antes de a variável existir.

```bash
# Chat on api.openai.com — `openai` is the provider that sends
# `max_completion_tokens`, which gpt-5 / o-series models require and
# `openai-compat` never emits — with embeddings on another endpoint.
export AI_MEMORY_LLM_PROVIDER=openai
export OPENAI_API_KEY=sk-...
export AI_MEMORY_EMBEDDING_PROVIDER=openai
export AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1
export EMBEDDING_API_KEY=sk-or-v1-...
```

### Anthropic via Claude subscription (OAuth)

> [!WARNING]
> **Não oficial e contrário às políticas de uso da Anthropic. Use por sua conta e risco.**
> A Anthropic não fornece API pública de OAuth para a assinatura Claude Pro/Max;
> isso reutiliza a credencial de `claude setup-token` contra `/v1/messages`, o que
> **não é uma integração compatível ou sancionada**. Os termos da Anthropic reservam
> o acesso por assinatura (Claude Code) para uso interativo, e usá-lo como um
> backend de API automatizado pode violar esses termos e **pode fazer sua conta
> sofrer limite de taxa, ser sinalizada ou banida**. A receita de cabeçalhos também não é documentada
> e pode mudar sem aviso prévio. Se você deseja um caminho compatível, use o
> provedor `anthropic` com uma chave real de API da Platform. Disponibilizamos isso puramente como
> uma conveniência opcional e não oferecemos garantias sobre isso.

> [!IMPORTANT]
> **A Anthropic atualmente recusa esses tokens de aplicativos de terceiros.** Sua
> [página legal e de conformidade](https://code.claude.com/docs/en/legal-and-compliance)
> não permite que desenvolvedores terceiros roteiem requisições por meio de credenciais de
> planos Free, Pro ou Max, e a API os rejeita do lado do servidor. Com o
> ai-memory, isso normalmente aparece como
> `provider error 429: {"type":"error","error":{"type":"rate_limit_error",...}}`
> em todas as requisições, incluindo `ai-memory llm-test`, mesmo quando o mesmo token
> funciona na CLI `claude`. Trata-se de uma recusa por política, não de um limite real de taxa,
> e tentar novamente ou alterar cabeçalhos não resolverá isso. Use o provedor
> `anthropic` com uma chave de API do Anthropic Console (`ANTHROPIC_API_KEY`), ou outro
> provedor da tabela acima.

`anthropic-oauth` é para assinantes Claude Pro/Max que desejam usar sua
assinatura existente em vez de uma chave de API da Anthropic Platform. Ele acessa o
**mesmo** endpoint `/v1/messages` que o provedor `anthropic`; apenas os cabeçalhos
de autenticação diferem (Bearer token + `anthropic-beta: oauth-2025-04-20`).

```bash
# Obtain a token once using the Claude Code CLI. It prints the token and
# does not save it anywhere, so copy it:
claude setup-token

# Then export it yourself (CLAUDE_CODE_OAUTH_TOKEN is accepted too):
export ANTHROPIC_OAUTH_TOKEN=<paste token here>
export AI_MEMORY_LLM_PROVIDER=anthropic-oauth
ai-memory serve
```

Para o Docker, passe o token como uma variável de ambiente:

```bash
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=anthropic-oauth \
    -e ANTHROPIC_OAUTH_TOKEN=<token> \
    akitaonrails/ai-memory:latest
```

Tanto `ANTHROPIC_OAUTH_TOKEN` quanto `CLAUDE_CODE_OAUTH_TOKEN` são aceitos;
o ai-memory verifica `ANTHROPIC_OAUTH_TOKEN` primeiro. Quando qualquer uma das variáveis é exportada
no host, os wrappers Docker POSIX e PowerShell encaminham seu nome para
comandos auxiliares de curta duração como `llm-test`; o valor do token é herdado pelo
Docker em vez de colocado na linha de comando do wrapper. O contêiner do servidor de longa
duração ainda precisa do provedor e das variáveis de token em seu próprio ambiente,
como no exemplo acima.

Para ambos os provedores Anthropic, o ai-memory omite `temperature` para os modelos
Claude 4.7 e posteriores e Claude Mythos Preview porque esses modelos rejeitam
parâmetros de amostragem não padrão. `llm-test` começa deliberadamente com o mesmo
valor representativo de 0.2 que a inicialização e consolidação, exercitando então a
normalização de compatibilidade do provedor antes de enviar a requisição.

> [!TIP]
> **Escolha um modelo pequeno e rápido.** O trabalho de LLM do ai-memory (consolidação
> de sessão, lint e exploração) é de sumarização/extração, não de raciocínio
> complexo, portanto um modelo da classe Haiku é suficiente. É mais rápido, mais barato e muito mais amigável
> aos limites de taxa da assinatura do que Sonnet/Opus. Defina, por exemplo,
> `AI_MEMORY_LLM_MODEL=claude-haiku-4-5`. Reserve os modelos de raciocínio de alto esforço
> para o seu agente de código real.

### OpenAI OAuth / Codex

`openai-oauth` é para contas ChatGPT Pro/Plus/Codex. Ele **não** usa
`OPENAI_API_KEY` e **não** chama `api.openai.com`; requisições vão para o
backend de Respostas do ChatGPT/Codex com um token OAuth renovável.

Para o wrapper de início rápido do Docker, isso grava no mesmo volume nomeado que o
servidor monta em `/data`:

```bash
ai-memory auth login openai-oauth
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=openai-oauth \
    akitaonrails/ai-memory:latest
```

Para um host Docker remoto, execute o login nesse host contra o mesmo contêiner
ou volume de dados:

```bash
docker exec -it ai-memory ai-memory auth login openai-oauth
```

Use `ai-memory auth status` para verificar se um token está presente e
`ai-memory auth logout openai-oauth` para removê-lo.

> [!TIP]
> **Mantenha o modelo no padrão do provedor (`gpt-5.5`).** O backend
> Codex/ChatGPT por trás de `openai-oauth` aceita apenas um pequeno conjunto definido
> pelo servidor de ids de modelo e rejeita outros, incluindo `gpt-5-mini`, com um 400
> determinístico, portanto não defina `AI_MEMORY_LLM_MODEL` para este backend. Consolidação / lint /
> exploração são tarefas de sumarização, então se o raciocínio padrão for muito pesado,
> defina `AI_MEMORY_LLM_REASONING_EFFORT=none` ou `low` em vez disso, para que tokens de
> pensamento ocultos não consumam o orçamento de JSON. Reserve raciocínio de alto esforço
> para o seu agente de código.

### Reutilização de credenciais do Codex

O provedor independente `codex` lê `$CODEX_HOME/auth.json`, recorrendo ao
`.codex/auth.json` do home da plataforma. Ele materializa apenas
`tokens.access_token` e `tokens.account_id`, recarrega-os antes de cada chamada
e nunca copia ou grava o arquivo. No primeiro 401, ele solicita que o
`codex app-server --stdio` renove a credencial pertencente ao Codex e repete
a requisição de Respostas uma vez.

```bash
export AI_MEMORY_LLM_PROVIDER=codex
export AI_MEMORY_LLM_MODEL=gpt-5.6-luna
export AI_MEMORY_LLM_REASONING_EFFORT=medium
ai-memory llm-test --provider codex --model gpt-5.6-luna --prompt "Reply with OK"
ai-memory llm-test --provider codex --model gpt-5.6-luna --structured --prompt "Return a short answer"
```

`AI_MEMORY_CODEX_EXECUTABLE` seleciona opcionalmente outro binário do Codex. O
armazenamento de arquivos é compatível; `auto` é compatível quando resolve para o mesmo
`auth.json`. O armazenamento exclusivo em chaveiro e efêmero não são compatíveis. O Docker
está fora do caminho de configuração automática: tanto o executável quanto as credenciais devem estar
disponíveis dentro do mesmo contêiner/ambiente.

### GitHub Copilot

`copilot` usa um token de usuário do GitHub, trocando-o em seguida por um token de API
de curta duração do Copilot por meio de `https://api.github.com/copilot_internal/v2/token`. O token
bruto do GitHub nunca é enviado para `api.githubcopilot.com`.

Para o wrapper de início rápido do Docker:

```bash
ai-memory auth login copilot
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=copilot \
    akitaonrails/ai-memory:latest
```

Para um host Docker remoto, execute o login contra o mesmo volume de dados:

```bash
docker exec -it ai-memory ai-memory auth login copilot
```

Deploys não interativos podem definir `COPILOT_GITHUB_TOKEN` em vez disso. O ai-memory também
aceita `GH_TOKEN` e `GITHUB_TOKEN` ao executar nativamente; prefira o explícito
`COPILOT_GITHUB_TOKEN` no Docker para não repassar um token amplo por acidente.
Usuários avançados com um token de API do Copilot pré-gerado podem definir
`GITHUB_COPILOT_API_TOKEN` e opcionalmente `COPILOT_API_URL`.

`auth login copilot` usa por padrão o id de cliente de fluxo de dispositivo público do GitHub Copilot.
Passe `--client-id` ou defina `AI_MEMORY_COPILOT_CLIENT_ID` se você opera seu próprio
aplicativo OAuth.

### Provedores compatíveis com OpenAI (Ollama / vLLM / LM Studio / APIs hospedadas)

```bash
docker run -d --name ai-memory \
    -p 49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_LLM_PROVIDER=openai-compat \
    -e AI_MEMORY_LLM_BASE_URL=http://host.docker.internal:11434/v1 \
    -e AI_MEMORY_LLM_MODEL=qwen2.5-coder:14b \
    akitaonrails/ai-memory:latest
```

Não há modelo padrão seguro para `openai-compat`; a variável de ambiente é
obrigatória. Para o OpenRouter (Kimi, DeepSeek, etc.):

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1
-e AI_MEMORY_LLM_MODEL=moonshotai/kimi-k2.6
-e LLM_API_KEY=sk-or-v1-...
```

`AI_MEMORY_LLM_REASONING_EFFORT` é respeitado neste caminho: hosts do OpenRouter
enviam `reasoning: { effort, exclude: true }`; `https://api.x.ai` (Grok)
envia o `reasoning_effort` do Chat Completions (limitado a `low`/`medium`/`high`/
`xhigh`, porque o Grok não pode desativar o raciocínio); outros endpoints compatíveis enviam
o `reasoning_effort` no estilo OpenAI. Anthropic e Anthropic-OAuth mapeiam a mesma
chave para `output_config.effort` e pensamento adaptativo/desativado em modelos que
aceitam esses campos (Haiku 4.5 os omite porque os rejeita, enquanto Haiku
5.5 os recebe; Fable 5 / Mythos 5 / Mythos Preview / Opus 5.5 omitem
`thinking: disabled` porque esses modelos o rejeitam, e Sonnet 5.5, que
o rejeita também, recebe `thinking: between_tools` para `none`). `ultra` e `persistent` se ajustam a `max` em hosts no estilo
OpenAI. Gemini e Copilot ignoram a chave.

[Atlas Cloud](https://www.atlascloud.ai/models/qwen/qwen3.5-flash) usa o
mesmo provedor; nenhum provedor de ai-memory específico do Atlas é necessário. Passe sua chave de API
por meio da credencial genérica de compatibilidade:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.atlascloud.ai/v1
-e AI_MEMORY_LLM_MODEL=qwen/qwen3.5-flash
-e LLM_API_KEY="$ATLASCLOUD_API_KEY"
```

Substitua o modelo por outro id de modelo atual do Atlas quando necessário. O ai-memory
não seleciona um padrão para endpoints de compatibilidade hospedados.

[OrcaRouter](https://www.orcarouter.ai) usa o mesmo provedor; nenhum
provedor de ai-memory específico do OrcaRouter é necessário. Passe sua chave de API por meio da
credencial genérica de compatibilidade:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.orcarouter.ai/v1
-e AI_MEMORY_LLM_MODEL=openai/gpt-4o
-e LLM_API_KEY=sk-orca-...
```

Substitua o modelo por outro id de modelo atual do OrcaRouter (mesmo
formato `provider/model` do OpenRouter, por exemplo `anthropic/claude-sonnet-4.6` ou
`deepseek/deepseek-v4-flash`) quando necessário.

[Cheaper Inference](https://cheaperinference.com) é um gateway de LLM compatível
com a OpenAI que anuncia modelos abaixo da tabela de preços de cada laboratório. Ele usa
o mesmo provedor; nenhum provedor dedicado do ai-memory é necessário.
Passe sua chave de API por meio da credencial genérica de compatibilidade:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.cheaperinference.com/v1
-e AI_MEMORY_LLM_MODEL=gpt-5.4-mini
-e LLM_API_KEY=ci_live_...
```

Ids de modelo são simples, sem um prefixo `provider/`. Substitua o modelo por
outro id de modelo atual do Cheaper Inference (por exemplo `claude-haiku-4.5` ou
`deepseek-v4-flash`) quando necessário. O Cheaper Inference serve apenas modelos de chat
e não possui endpoint de embeddings; configure embeddings separadamente.

O [API Route](https://www.api-route.com/) também usa o provedor `openai-compat`
existente. Forneça uma chave do API Route e um ID de modelo do seu
[catálogo atual](https://www.api-route.com/pricing):

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://global.api-route.com/v1
-e AI_MEMORY_LLM_MODEL=gpt-5.5
-e LLM_API_KEY="$API_ROUTE_API_KEY"
```

Substitua `gpt-5.5` pelo ID exato do modelo que você pretende usar. Como em outros
endpoints de compatibilidade hospedados, nenhum provedor dedicado do ai-memory é necessário.
Configure embeddings separadamente se o modelo escolhido do API Route não
fornecer um endpoint de embeddings compatível com a OpenAI.

O [FutureInfra](https://futureinfra.ai/ai/) é um roteador de API de IA compatível
com a OpenAI e usa o mesmo provedor; nenhum provedor dedicado do ai-memory é necessário.
Sua base compatível com a OpenAI é `https://futureinfra.ai/v1/ai`. Esse caminho não
termina em um segmento de versão, portanto passe a URL completa do Chat Completions (o ai-memory
usa uma URL base que já termina em `/chat/completions` como está). Passe sua chave
de API por meio da credencial genérica de compatibilidade:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://futureinfra.ai/v1/ai/chat/completions
-e AI_MEMORY_LLM_MODEL=openai/gpt-4o-mini
-e LLM_API_KEY="$FUTUREINFRA_API_KEY"
```

Ids de modelo usam o formato `provider/model`, por exemplo `anthropic/claude-sonnet-4` ou
`deepseek/deepseek-chat`; `GET https://futureinfra.ai/v1/ai/models` lista os
ids atuais. Chaves são criadas no
[console do FutureInfra](https://futureinfra.ai/console/?screen=ai-router). Este
exemplo configura apenas o LLM; configure embeddings separadamente.

Chamadas estruturadas compatíveis com OpenAI usam o JSON Schema da operação por padrão:

```bash
-e AI_MEMORY_LLM_COMPAT_STRICT=true
```

Endpoints modernos do Ollama, vLLM, LM Studio, llama.cpp e gateways respeitam essa
requisição `response_format=json_schema` no estilo OpenAI. O ai-memory tenta novamente com seu
parser tolerante quando um endpoint rejeita explicitamente o campo de saída estruturada
ou retorna um formato de resposta malformado. Para um endpoint incompatível, desative:

```bash
-e AI_MEMORY_LLM_COMPAT_STRICT=false
```

Gateways hospedados que transmitem completações longas além do teto padrão de 300 segundos
por requisição falham com `http: error sending request`; aumente o
teto para corresponder ao tempo de geração do pior caso do gateway:

```bash
-e AI_MEMORY_LLM_TIMEOUT_SECS=900
```

#### Enviar um cabeçalho de identificação de chamador para um gateway que o exija

Toda requisição de chat já carrega `User-Agent: ai-memory/<version>`, para que um
gateway possa saber o que o está chamando. Alguns também exigem um cabeçalho próprio
para correlação de requisição, rejeitando ou limitando o tráfego sem ele. Declare-os
uma vez; eles são enviados em toda requisição de chat, qualquer que seja o provedor:

```bash
-e AI_MEMORY_LLM_HEADERS=x-opencode-session=prod-01,x-opencode-client=ai-memory
```

Entradas são `Name=Value` ou `Name: Value`, separadas por vírgula. Um *valor* de cabeçalho
não pode conter vírgula pela variável de ambiente; use `llm_headers = [...]` no
`config.toml` quando necessário. Cabeçalhos que o próprio ai-memory define (`authorization`,
`content-type`, `x-api-key`, `x-goog-api-key`, `anthropic-version`,
`anthropic-beta`, `openai-beta`, `host`, `content-length`) são recusados na
inicialização em vez de duplicados na requisição. Uma entrada para `user-agent`
substitui o padrão. Valores nunca são registrados em log.

O provedor `opencode` não precisa de configuração para isso: o OpenCode solicita
aos chamadores o `x-opencode-session`, e esse provedor já envia um id por
operação lógica. O id é estável entre tentativas e no fallback de saída estruturada,
de modo que uma passagem de consolidação é lida como uma operação nas métricas do
OpenCode. Forneça o cabeçalho via `AI_MEMORY_LLM_HEADERS` para substituir isso, por
exemplo para distinguir várias instâncias do ai-memory sob uma mesma conta.

#### Adequar o orçamento de consolidação à janela de contexto de um modelo local

A consolidação tem como padrão uma meta de entrada aproximada de 100k tokens mais um limite
de saída de 32k, dimensionados para um provedor de contexto de 200k. Um modelo local com uma janela
menor pode rejeitar toda a requisição (`exceed_context_size_error` do llama.cpp,
HTTP 400 da maioria dos gateways). Diminua ambos os limites para que sua soma caiba
na janela de contexto real, com margem adicional para variação do tokenizador:

```bash
# e.g. a model loaded with a 12k context window
-e AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS=10000
-e AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS=1000
```

Uma janela de 8k não pode conter o piso padrão (8.350 tokens de entrada na margem
de 0.8). Ela se ajusta com `AI_MEMORY_CONSOLIDATION__INPUT_TOKEN_SAFETY_MARGIN=1.0`
e cerca de 6.700 tokens de entrada mais 1.000 de saída, ao custo da margem que
é mantida para texto denso como português ou código (#884).

O sublinhado duplo separa a seção `[consolidation]` de cada chave.
A meta de entrada contabiliza as observações renderizadas, o corpo da página atual,
o prompt do sistema, as convenções da página, os snapshots de slot delimitados, o esquema
de saída estruturada e a reserva de envelope do provedor. Tokenizadores diferem, então esta é uma
estimativa conservadora em vez de uma contagem exata de tokens do provedor. Uma falha automática
do provedor de checkpoint degrada para uma página baseada em regras em vez de perder
o checkpoint, mas limites dimensionados corretamente são o que permite que a consolidação por LLM
tenha sucesso. A inicialização rejeita um orçamento que deixe pouco espaço para observações
após os prompts fixos e o esquema de saída estruturada — 8.350 tokens de entrada
no `input_token_safety_margin` padrão de 0.8 — e rejeita limites de saída
abaixo de 1.000, porque o esquema de lote e uma resposta útil não cabem de forma confiável
abaixo desses pisos. Reduzir a margem de segurança diminui o orçamento de caracteres, então
uma margem menor precisa de um `max_input_tokens` proporcionalmente maior.

Sessões longas podem omitir observações ou truncar um corpo individual antes que
a requisição chegue ao modelo. Para adicionar um conjunto delimitado de linhas de erro literais, caminhos,
URLs, códigos de status e versões desse texto faltante, habilite com:

```toml
[consolidation]
verbatim_residue = true
```

ou `AI_MEMORY_CONSOLIDATION__VERBATIM_RESIDUE=true`. O padrão é false.
Habilitá-lo amplia o subconjunto de texto capturado já higienizado enviado ao
provedor de LLM configurado; não anexa nada diretamente a uma página do wiki.
O bloco é limitado, marcado como dados de observação não confiáveis, e a resposta
do provedor ainda passa pelas fronteiras existentes de saída estruturada e
gravação no wiki.

---

## Subcomandos comuns

Esta é a lista operacional resumida. Execute `ai-memory --help` para a
árvore completa oficial de comandos.

Duas maneiras de invocar um subcomando contra o deploy do docker:

```bash
# A) Against the running container (stateful: status, search, backup,
#    checkpoints, restore-page, audit-contamination, forget-sweep, lint, embed).
docker exec ai-memory ai-memory status --json
docker exec ai-memory ai-memory search "karpathy"
docker exec ai-memory ai-memory backup --to /data/snapshot.tar.gz

# B) One-shot, no running container needed for pure-stdout helpers
#    (generate-auth-token, completions, install-mcp, install-hooks, setup-agent,
#    llm-test).
#    Auth login is stateful: use docker exec against the running container or
#    the wrapper so it writes into the same data volume as the server.
docker run --rm akitaonrails/ai-memory:latest generate-auth-token
docker run --rm akitaonrails/ai-memory:latest completions zsh
docker run --rm akitaonrails/ai-memory:latest install-mcp --client cursor
docker run --rm akitaonrails/ai-memory:latest --help     # full subcommand tree
```

| Subcomando | Padrão | O que faz |
|---|---|---|
| `serve` | `docker compose up -d` (já concluído) | Executa o servidor MCP HTTP |
| `setup [--yes] [--agent <name>]... [--no-service]` | binário nativo (após `install.sh`) | Inicializa (`init`) um diretório de dados limpo (com seu token bearer raiz), instala e inicia um serviço por usuário, conecta MCP, hooks e Agent Skills para cada agente indicado ou detectado, executando em seguida uma autoverificação somente leitura. Mostra o plano e solicita confirmação antes; uma segunda execução não altera nada. |
| `auth enable [--no-mcp]` | binário nativo ou wrapper do host | Concede a uma instalação existente sem autenticação um token bearer raiz em `auth-tokens/default` e rerenderiza as conexões de hooks e MCP para lê-lo; `--no-mcp` mantém os registros de MCP intactos. Idempotente. |
| `project init` | binário nativo, no checkout | Solicita `workspace` e `project` e grava-os em um novo marcador `.ai-memory.toml` na raiz da árvore de trabalho; os padrões mantêm o escopo para o qual o checkout já resolve, e um marcador existente nunca é sobrescrito. Requer um terminal. |
| `recover [--json]` | wrapper do host ou binário nativo | Após uma indisponibilidade do servidor, drena o spool de hooks local, reproduz transcrições de execuções degradadas pelo caminho de backfill e finaliza execuções gerenciadas cuja importação de transcrição falhou. Seguro para reexecutar; `--json` imprime o relatório como JSON. |
| `run [harness] [args...]` | wrapper do host ou binário nativo | Adere a um workstream gerenciado entre harnesses; omita o harness para retomar a sessão local utilizável mais recente, ou nomeie explicitamente Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP, Grok Build CLI, Antigravity CLI ou GitHub Copilot CLI; as flags exatas `--yolo` e `--fresh` são controladas pelo wrapper e outros argumentos nativos são repassados |
| `show [--json]` | wrapper do host ou binário nativo | Escolhe um checkout local do cliente e um harness gerenciado instalado, ou retorna dados estruturados de descoberta sem inicializar; servidores remotos nunca fornecem caminhos de checkout |
| `continue [--workspace NAME]` | wrapper do host ou binário nativo | A partir de qualquer diretório, revalida e retoma o checkout gerenciado local do cliente mais recente; aceita `--yolo` e `--fresh`, mas nenhum argumento nativo de harness |
| `resume [--all] [--workspace NAME] [--search TERM] [--limit N]` | wrapper do host ou binário nativo | Escolhe interativamente entre todos os workstreams no checkout atual (sem corte padrão), ou com `--all` de cada checkout vinculado válido local do cliente; digite para pesquisar nomes, Cima/Baixo seleciona, Esquerda/Direita alterna `auto` mais os harnesses instalados, Enter inicializa e Escape limpa a pesquisa ou cancela; um limite explícito restringe os resultados iniciais de pesquisa; aceita `--yolo` e `--fresh` |
| `workstreams [--workspace NAME] [--project NAME] [--limit N] [--json]` | wrapper do host ou binário nativo | Lista workstreams recentes selecionáveis a partir do checkout atual, incluindo a seleção atual e harnesses vinculados, sem expor caminhos ou ids de sessão nativos |
| `rename-workstream (--from NAME \| --workstream-id ID) --to NAME [--workspace NAME] [--project NAME] [--json]` | wrapper do host ou binário nativo | Renomeia um workstream selecionável a partir do checkout atual; apenas metadados, portanto o id estável, o ledger, a ordem de listagem e a seleção atual não são afetados |
| `workstream-search [query]` | filho gerenciado ou cliente HTTP leve | Pesquisa o ledger completo de workstreams gerenciados visíveis; o filho gerenciado recebe seu id de workstream automaticamente |
| `status` | `docker exec` | Contagens, caminhos, diagnósticos de índices derivados e integridade passiva dos provedores de LLM/embedding |
| `search "<query>"` | `docker exec` | Busca FTS5 no wiki + autoridade de fonte delimitada; use a ferramenta MCP `memory_query` para RRF de entidade/grafo/vetor |
| `write-page` | `docker exec` | Gravação manual de página (atômica + indexada) |
| `backup --to` / `restore --from` | `docker exec` | Cria snapshot ou restaura o diretório de dados |
| `checkpoints` / `restore-page` | `docker exec` | Lista checkpoints git do wiki ou restaura uma página markdown e a reindexa |
| `audit-contamination` | `docker exec` | Auditoria estrutural somente leitura para provável contaminação entre projetos |
| `forget-sweep` / `lint` / `embed` | `docker exec` | Manutenção manual; sweep + lint também rodam no agendamento do servidor por padrão |
| `commit -m "…"` | `docker exec` | Prepara + commita a árvore do wiki |
| `reset --confirm` | `docker exec` | Limpa dados (recusa enquanto processos irmãos estiverem ativos) |
| `generate-auth-token` | `docker run --rm` | Imprime um token bearer hexadecimal aleatório |
| `user add-human` / `list` / `reset-password` / `disable` / `enable` / `patch` | `docker exec` ou binário nativo | Identidades humanas e senhas temporárias; nunca emite uma chave de API |
| `api-key add` / `list` / `rotate` / `revoke` | `docker exec` ou binário nativo | Credenciais de máquina nativas `aim_`, mais as chaves `legacy-user-token` deixadas pelo `user add` do 1.x (removido no 3.0) |
| `auth login openai-oauth` | mesmo volume de dados que o servidor | Armazena um token de atualização OAuth do ChatGPT/Codex para o provedor LLM opcional `openai-oauth` |
| `auth login copilot` | mesmo volume de dados que o servidor | Armazena um token do GitHub para o provedor LLM opcional `copilot` |
| `auth login oidc-device` | mesmo diretório de dados de desenvolvedor que os hooks nativos e comandos CLI de cliente leve | Armazena um token de dispositivo OIDC por desenvolvedor para autenticação de hook nativo e autenticação de fallback de CLI HTTP |
| `install-mcp --client` | `docker run --rm` | Snippet de configuração de MCP por cliente |
| `install-hooks --agent [--scope project]` | `docker run --rm` | Snippet de configuração de hook para um diretório de hooks existente; `--scope project` tem como alvo o `.claude/settings.local.json` do checkout (apenas Claude Code) |
| `setup-agent --agent --to --host-prefix` | `docker run --rm -v` | Extrai scripts empacotados + imprime configuração (passo único) |
| `install-instructions [--target] [--print] [--no-skills]` | mesmo ambiente de host usado para os arquivos de prompt do agente | Instala ou atualiza o bloco de roteamento fino de CLAUDE.md / AGENTS.md e, por padrão, as Agent Skills gerenciadas do ai-memory |
| `install-skills [--scope] [--agent]` | mesmo ambiente de host usado para os diretórios de skills do agente | Instala ou atualiza apenas as Agent Skills gerenciadas do ai-memory |
| `uninstall --apply` | mesmo ambiente de host usado para instalação | Remove apenas hooks, entradas de MCP, blocos de instrução, arquivos de skills gerenciadas e plugins gerados de propriedade do ai-memory após validação de conteúdo/marcador, mais o serviço de usuário instalado pelo `setup` caso ninguém o tenha editado desde então. Remover hooks ou MCP também exclui os sentinelas de autoconexão do `ai-memory run` sob `<data_dir>/autowire-state/`, para que a próxima inicialização gerenciada faça a conexão novamente. Use `--mcp-url` para endpoints de MCP personalizados e `--mcp-name` apenas para restringir a remoção. |
| `llm-test --provider …` | `docker run --rm -e …` | Realiza teste de fumaça de um provedor LLM |
| `completions <shell>` | `docker run --rm` ou binário nativo | Imprime um script de completamento para bash/zsh/fish/PowerShell/elvish; consulte [`shell-completions.md`](shell-completions.md) |

### Snippets de roteamento gerenciados e Agent Skills

A instalação de roteamento do ai-memory é empacotamento de prompt voltado ao agente. Ela não adiciona um
roteador de skills em tempo de execução, e arquivos `SKILL.md` não são páginas de memória duráveis. A
wiki permanece a fonte durável da verdade.

`ai-memory install-instructions` agora grava dois artefatos de prompt gerenciados por
padrão:

1. Um bloco de instruções enxuto em `CLAUDE.md`, `AGENTS.md` ou no arquivo passado com
   `--target`. O bloco é delimitado pelos marcadores `<!-- ai-memory:start -->` e
   `<!-- ai-memory:end -->` que aparecem sozinhos em suas próprias linhas.
2. Agent Skills gerenciadas do ai-memory contendo a orientação detalhada de roteamento de ferramentas.

Reexecutar o comando é seguro. Se um projeto ainda tiver o bloco antigo longo do ai-memory
entre marcadores ancorados na linha, a atualização substitui esse bloco no local
pelo snippet enxuto, mantém intactas instruções não relacionadas antes e depois dele,
e grava um backup `.bak-*` com registro de data e hora antes de alterar um arquivo existente.
Arquivos de skills gerenciadas contêm um marcador de propriedade do ai-memory; skills de usuário de mesmo nome
sem esse marcador são preservadas, a menos que você force a substituição explicitamente.
Suas cargas úteis incorporadas usam quebras de linha LF em todas as plataformas de release, de modo que instalações
pela CLI e `memory_install_self_routing` retornam os mesmos bytes no Windows,
Linux e macOS. Isso não reescreve quebras de linha em arquivos de autoria do usuário.
`install-instructions --print` visualiza apenas o snippet de instruções; execute
`install-skills --print` quando quiser visualizar as cargas úteis das skills gerenciadas.

Flags de `install-instructions` para skills:

| Flag | Significado |
|---|---|
| `--no-skills` | Atualiza apenas o bloco de instruções marcado. |
| `--skills-scope <scope>` | Escolha raízes de skills locais do projeto ou globais do usuário. Valores: `project`, `global`. O padrão é `project`. |
| `--skills-agent <agent>` | Escolha `.claude/skills`, `.agents/skills`, `.devin/skills`, `.grok/skills`, `.hermes/skills`, `.github/skills` do GitHub Copilot CLI, raízes do Google Antigravity ou ambas as raízes Claude/Agents. Valores: `claude-code`, `agents`, `devin`, `grok`, `copilot-cli`, `hermes`, `antigravity`, `both`. Por padrão, alvos `CLAUDE.md` implicam `claude-code`, alvos `AGENTS.md` implicam `agents` e ambos os arquivos de instrução implicam `both`. |
| `--skills-target-dir <dir>` | Grava diretórios de skills gerenciadas abaixo de uma raiz explícita em vez de inferir pelo escopo e agente. |
| `--skills-force` | Substitui skills de mesmo nome não gerenciadas durante `install-instructions`; sem isso, elas são deixadas intocadas e o comando é encerrado com um erro acionável. |

Use `install-skills` quando o bloco de instruções já estiver correto e apenas os
arquivos de Agent Skill precisarem de uma atualização:

```bash
ai-memory install-skills
ai-memory install-skills --scope global --agent agents
ai-memory install-skills --scope global --agent devin
ai-memory install-skills --scope global --agent grok
ai-memory install-skills --scope global --agent copilot-cli
ai-memory install-skills --scope global --agent hermes
ai-memory install-skills --scope global --agent antigravity
ai-memory install-skills --agent both --print
ai-memory install-skills --target-dir .custom/skills --force
```

Flags de `install-skills`:

| Flag | Significado |
|---|---|
| `--scope <scope>` | Instala neste projeto ou nas raízes de skills globais do usuário atual. Valores: `project`, `global`. O padrão é `project`. |
| `--agent <agent>` | Instala na raiz de skills do Claude Code, na raiz de skills entre agentes, na raiz de skills do Devin, na raiz de skills do Grok, na raiz de skills do Hermes Agent, na raiz de skills do GitHub Copilot CLI, nas raízes de skills do Google Antigravity ou em ambas as raízes Claude/Agents. Valores: `claude-code`, `agents`, `devin`, `grok`, `copilot-cli`, `hermes`, `antigravity` (aliases `antigravity-cli`, `agy`), `both`. O padrão é `claude-code`. |
| `--target-dir <dir>` | Grava diretórios de skills gerenciadas abaixo de uma raiz explícita; `--scope` e `--agent` são ignorados. |
| `--print` | Imprime caminhos de destino e conteúdos de `SKILL.md` sem gravar arquivos. Um plano global do Grok também imprime `$GROK_HOME/rules/ai-memory-handoff.md`. |
| `--force` | Substitui skills de mesmo nome não gerenciadas (e, para o Grok global, uma regra de primeiro turno não gerenciada); sem isso, arquivos de mesmo nome de autoria do usuário são preservados. |

Raízes de destino de skills padrão:

| Scope | `--agent claude-code` | `--agent agents` | `--agent devin` | `--agent grok` | `--agent copilot-cli` | `--agent hermes` | `--agent antigravity` |
|---|---|---|---|---|---|---|---|
| `project` | `.claude/skills` | `.agents/skills` | `.devin/skills` | `.grok/skills` | `.github/skills` | `.hermes/skills` | `.agents/skills` |
| `global` | `~/.claude/skills` | `~/.agents/skills` | Windows: `%APPDATA%\devin\skills`; não-Windows: `~/.devin/skills` | `$GROK_HOME/skills` (padrão `~/.grok/skills`), mais a regra de primeiro turno `$GROK_HOME/rules/ai-memory-handoff.md` | `$COPILOT_HOME/skills` (padrão `~/.copilot/skills`) | `~/.hermes/skills` | ambos `~/.gemini/config/skills` (Antigravity App e IDE) e `~/.gemini/antigravity-cli/skills` (Antigravity CLI) |

Cada skill gerenciada é gravada como `<root>/<skill-name>/SKILL.md`.

`ai-memory uninstall --only skills --apply` remove arquivos de skills gerenciadas apenas
das raízes de projeto/globais padrão mostradas acima, após validar o
marcador de propriedade do ai-memory. Se você instalou com `--target-dir` ou
`--skills-target-dir`, limpe essa raiz personalizada manualmente.

O diretório de dados dentro do contêiner é `/data` (montado via o volume
do compose). Fora do docker, substitua com `AI_MEMORY_DATA_DIR=/path`.

A manutenção agendada é configurada em `[maintenance]` no `config.toml`.
Por padrão, o lint baseado em regras e a limpeza de esquecimento (forget sweep) rodam diariamente fora da
latência de hooks em cada espaço de trabalho/projeto existente. O backfill de embeddings é
compatível, mas vem desativado por padrão porque pode acionar um provedor pago; se você
ativar `embedding_backfill_interval_secs` após configurar um integrador de embeddings,
cada ciclo agendado executa o backfill em todo espaço de trabalho/projeto existente e pode
aumentar o consumo do provedor de acordo.

A limpeza de esquecimento e o lint baseado em regras persistem sua última conclusão bem-sucedida. Ao
reiniciar, uma tarefa que não venceu aguarda apenas o intervalo restante; uma tarefa nunca
executada ou atrasada roda uma vez após um atraso delimitado de inicialização. Execuções com falha
não são registradas como bem-sucedidas e tentam novamente após esse atraso delimitado. O backfill de embeddings
permanece opcional e mantém seu comportamento apenas por intervalo (sem recuperação na inicialização).

`reconcile_tombstones_deleted_pages` (padrão `false`, também sob
`[maintenance]`) é uma opção experimental e separada: permite que a própria
passagem de reconciliação de 30s do observador (não as tarefas agendadas acima) marque como tombstone uma
página de conteúdo importada pelo OKF (páginas de resumo de sessão são excluídas; consulte
`docs/okf.md`) cujo arquivo desapareceu do disco, depois de estar
ausente em duas passagens consecutivas e sobreviver a um disjuntor que
se recusa a agir quando mais de `max(3, 50%)` das páginas candidatas de um escopo
parecem ausentes de uma só vez, ou quando uma varredura não encontra nada. A tombstone é
suave (`is_latest = 0` + `superseded_at`, a mesma forma usada pela remoção por decaimento)
e é coletada pela mesma limpeza permanente de tombstones antigas; ela não está
isenta disso. O que realmente a protege: uma tombstone de reconciliação nunca
é destruída enquanto sua cadeia não tiver sucessor; se o arquivo retornar, a
nova versão se reconecta à cadeia com tombstone em vez de começar do zero, portanto
nada fica órfão para essa limpeza destruir. Ela nunca executa a barreira de
admissão bloqueante (nada pode recusá-la), mas dispara de forma assíncrona qualquer
webhook observador/espelho não bloqueante. Com ela desativada (o padrão), o comportamento
da reconciliação permanece inalterado: um arquivo excluído ainda requer `ai-memory delete-page`.
Consulte
`docs/okf.md` para o design completo.

---

## Bootstrap em projetos em andamento

Quando você adota o ai-memory em um projeto que já existe há
algum tempo, o wiki começa vazio. `ai-memory bootstrap` ingere o
histórico existente do projeto em páginas semente para que a primeira sessão
tenha contexto aquecido.

```bash
cd /path/to/project
ai-memory bootstrap
```

Se você instalou o wrapper do Docker a partir do início rápido e iniciou o
servidor em `127.0.0.1:49374`, o wrapper alcança automaticamente o servidor loopback
do host a partir do seu contêiner utilitário temporário. Defina
`AI_MEMORY_SERVER_URL=http://<server>:49374` apenas quando o servidor for
remoto ou usar um host/porta personalizado.

**O que é ingerido por padrão:**

| Origem | Prioridade (descartada primeiro ao estourar orçamento) |
|---|---|
| `CLAUDE.md` / `AGENTS.md` (regras do projeto) | nunca descartada |
| `README.md` na raiz do repositório | muito-tarde |
| `docs/**/*.md` | tarde |
| Commits substantivos do git (corpo >120 caracteres OU prefixo conventional-commit) | média |
| Comentários de documentação de nível de módulo `//!` em `**/*.rs` | primeira a descartar |

**Flags:**

```
--repo-path <PATH>         (default: git rev-parse --show-toplevel)
--workspace <NAME>         (default: the nearest `.ai-memory.toml` marker's
                            `workspace`, else "default")
--project <NAME>           (default: the marker's `project` when pinned,
                            else derived from cwd — main repo root's
                            basename via `git rev-parse --show-toplevel`,
                            or basename(cwd) when no repo is found.
                            "scratch" only as a defensive fallback for
                            hook events with no usable cwd.)
--max-input-tokens N       (default: 150000; total source budget after prune)
--chunk-input-tokens N     (default: 24000; per LLM call, each asking for up
                            to 16K output tokens; 0 = single call asking for
                            up to 64K, so input + 64K must fit the context)
--since "30 days ago"      (git log filter; supports "N days/months/years ago" + YYYY-MM-DD)
--exclude-git              (skip commit history)
--exclude-readme           (skip README)
--exclude-docs             (skip docs/**/*.md)
--exclude-code             (skip Rust module headers)
--dry-run                  (collect + estimate but don't call LLM or write)
--force                    (re-bootstrap, overwrites the prior manifest)
```

**Orçamentos e a estimativa.** O Bootstrap estima tokens como bytes ÷ 4, o que
subestima textos em outros idiomas e código, portanto preenche apenas 80% de
`--max-input-tokens` e de cada `--chunk-input-tokens` por essa estimativa. A
execução dry-run ainda relata tokens pela mesma estimativa; as fontes que ela mantém e os
chunks que planeja refletem essa margem.

**Custo.** Com Kimi 2.6 via OpenRouter ($0.73/$3.49 por milhão):
- Limite de 50k tokens de entrada → ~$0.04 de entrada no pior caso
- 1-2k tokens gerados → ~$0.007 de saída
- Total: bem abaixo de $0.20 por execução.

**Idempotência.** A primeira execução gera um manifesto `bootstrap.md` por projeto
(em `<wiki>/<workspace>/<project>/bootstrap.md`) listando cada
página gerada + uma justificativa de um parágrafo. Reexecutar sem `--force`
resulta em erro. Exclua o manifesto (e as páginas geradas) se quiser um
re-bootstrap limpo. Se a resolução canônica de escopo promover um projeto legado mas
a atualização/checkpoint do seu `_meta.md` falhar, o resultado inclui `manifest_warning`
e a CLI o exibe; a mudança de nome é commitada e o backfill de inicialização pode
reparar o manifesto de escopo.

**Faça um dry-run primeiro.** Sempre vale a pena antes da chamada real para ver
quais fontes seriam realmente enviadas + quantos tokens isso
representa. A saída é JSON para a stdout.

```bash
ai-memory bootstrap --dry-run
{
  "sources_collected": 117,
  "sources_sent": 22,
  "sources_dropped": 95,
  "estimated_input_tokens": 48760,
  "pages_written": [],
  "rationale": "(dry-run; LLM not invoked)",
  "dry_run": true,
  "llm_chunks": 1
}
```

Repositórios grandes (por exemplo, anos de histórico do git) são podados no lado do cliente antes do
POST, sendo então processados em chunks sequenciais de LLM para que os limites de contexto do provedor
não sejam excedidos. A CLI registra `llm_chunks` no dry-run e no resultado
final.

**Aviso: Detalhes fabricados pelo LLM.** Uma execução de bootstrap pode produzir
páginas plausíveis, porém incorretas (o LLM não conhece o seu projeto, ele está
inferindo a partir do histórico do git). A wiki é versionada pelo git precisamente para
que isso seja recuperável: revise o que foi gerado, execute `docker exec ai-memory git
-C /data/wiki diff HEAD~1` e reverta se estiver incorreto.

## Logs e sandboxes somente leitura

A CLI e o servidor gravam logs com rotação diária em `<data_dir>/logs/`
(`~/.local/share/ai-memory/logs/` por padrão). Quando esse local não é
gravável (sandboxes como [ai-jail](https://github.com/akitaonrails/ai-jail)
montam `$HOME` como somente leitura ou como tmpfs descartável), o ai-memory degrada em vez
de falhar: ele recorre ao diretório temporário do SO, e em seguida ao registro somente na stderr,
imprimindo o caminho exato que falhou em cada etapa. Os comandos continuam funcionando
de qualquer forma. Para manter logs duráveis em arquivo (e spool durável de hooks) dentro de uma
sandbox, mapeie o diretório de dados como leitura e escrita, por exemplo `ai-jail --rw-map
~/.local/share/ai-memory …`.

## Bootstrap e recuperação de senha humana

O login no console humano é por usuário/senha, não um token Bearer colado no
navegador. APIs de máquina continuam usando `Authorization: Bearer` (`AI_MEMORY_AUTH_TOKEN`
ou uma chave nativa `aim_`). Antes de a autenticação humana ser ativada, a compatibilidade legada
somente GET do navegador pode aceitar o bearer raiz via HTTP Basic (nenhum cookie
é definido; um cookie `ai_memory_auth` definido por uma versão anterior ainda é aceito);
a ativação desativa esse caminho imediatamente.
As classes ativas permanecem isoladas: uma senha emite apenas uma sessão; uma sessão
nunca autentica `/mcp`/hooks/workstreams; a recuperação nunca emite uma sessão;
uma chave de API nunca faz login em `/auth/login`.

### Primeiro acesso do root (greenfield)

Defina uma senha de uso único e, em seguida, inicie o `serve`. O mecanismo cria o
`root_username` configurado (padrão `root`) com `must_change_password=true` e marca
o bootstrap como concluído. Reinicializações posteriores ignoram o valor mesmo que permaneça no
ambiente, portanto remova-o para manter o texto puro fora do ambiente do processo.

```bash
# At least 12 characters. Must not equal the root bearer, actor-proxy bearer,
# or recovery token, and must not use an `ams_`/`aim_`/`amk_` prefix.
export AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD='choose-a-long-password'
docker compose up -d   # or: ai-memory serve
unset AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD
```

O primeiro login deve alterar essa senha (`POST /auth/password` com CSRF).
Se a senha colidir com o hash de uma credencial nativa de API existente, a inicialização
falha de forma segura e o bootstrap não é marcado como concluído.

### Recuperação de emergência (break-glass)

Se o último root humano for perdido, defina um segredo de recuperação de alta entropia (pelo menos
32 caracteres) e envie um `POST /auth/recovery` com uma nova senha. O sucesso retorna
204, expira o cookie legado `ai_memory_auth`, revoga as sessões daquele usuário
e **não** define `ai_memory_session`. Entre novamente com a nova senha.

```bash
export AI_MEMORY_AUTH__RECOVERY_TOKEN='at-least-32-characters-of-entropy-here'
# restart serve so the process picks up the new value
curl -sS -D - -o /dev/null -X POST http://127.0.0.1:49374/auth/recovery \
  -H 'content-type: application/json' \
  -d '{"recovery_token":"'"$AI_MEMORY_AUTH__RECOVERY_TOKEN"'","new_password":"brand-new-pass!!","new_password_confirmation":"brand-new-pass!!"}'
```

Rotacione alterando a variável de ambiente e reiniciando; o token anterior para
de funcionar imediatamente. Falhas públicas (token incorreto, recuperação não definida, política de
senha) compartilham um mesmo corpo 401. O token de recuperação não é um Bearer, cookie ou
senha de login.

Quando o modo humano está ativado (bootstrap concluído, qualquer hash de senha ou
segredos iniciais/de recuperação configurados), o `serve` se recusa a iniciar a menos que um
root recuperável exista (`role=root` com uma senha e não desativado) ou a
recuperação esteja configurada. Deploys remotos explícitos somente de máquina com
`AI_MEMORY_AUTH_TOKEN` e nenhum segredo humano permanecem válidos. O loopback sem
Bearer nem autenticação humana permanece anônimo.

CIDRs em `AI_MEMORY_AUTH__TRUSTED_PROXY_CIDRS` podem fornecer `X-Forwarded-For`
para limites de taxa de login; o XFF de pares não confiáveis é ignorado. Atrás de HTTPS, defina
`AI_MEMORY_AUTH__SECURE_COOKIE=true` conforme observado acima.

Um operador que aceite intencionalmente autenticação humana não criptografada em uma
LAN privada pode, em vez disso, passar `serve --allow-insecure-human-auth` (ou definir
`AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`, inclusive no Docker). A opção é
desativada por padrão e não torna o transporte em texto simples seguro: senhas, credenciais
Bearer e de API, cookies de sessão e dados de memória podem todos ser interceptados
ou modificados. Isso contorna apenas a verificação de postura de cookie humano fora do loopback; um
servidor sem autenticação ainda precisa da sobreposição separada
`--allow-insecure-no-auth`.

## Operando sem autenticação

Para deploys somente locais / em máquina única, você pode ignorar o token
bearer:

```bash
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    akitaonrails/ai-memory:latest
```

Observe o bind: `127.0.0.1:49374`, não `0.0.0.0:49374`. Este é o
pareamento crítico — **sem token bearer E apenas loopback** é a única
combinação segura. O servidor recusa um bind de LAN não autenticado antes de
aceitar requisições. `--allow-insecure-no-auth` pode sobrepor essa recusa apenas
para uma implantação perigosa e intencional em HTTP puro; prefira
`AI_MEMORY_AUTH_TOKEN` ou loopback em vez disso.

Em um *contêiner*, essa recusa se torna um aviso, porque o contêiner deve
fazer bind em `0.0.0.0` internamente para o `-p` funcionar e não pode ver qual endereço
do host você publicou. O `-p` acima está, portanto, fazendo o trabalho real: ele
é o que mantém este contêiner exclusivo para loopback. Se você alterá-lo para publicar em um
endereço de LAN, defina `AI_MEMORY_AUTH_TOKEN` também.

Um contêiner iniciado dessa forma não possui token, já que nunca executou `init`. Para
exigir um, execute `ai-memory auth enable` através do wrapper (ele grava o
token no volume), execute `docker restart ai-memory` e depois execute novamente
`ai-memory install-hooks --agent <agent> --apply` para que os hooks do host o obtenham.
O `install-hooks` copia o token do volume para um arquivo restrito ao proprietário sob
`~/.local/share/ai-memory/native-runner/`, e os comandos que o wrapper executa
nativamente (`install-hooks`, `mcp-bridge`, `run`) leem-no de lá, nunca a partir de
uma linha de comando. Quando o `install-hooks` não encontra mais nenhum token no volume (autenticação
desativada ou arquivo de token removido), ele exclui essa cópia; um mecanismo de contêiner
inacessível a deixa intacta. Um `ai-memory uninstall --apply` completo
pelo wrapper a exclui assim que a desinstalação for bem-sucedida; um dry run ou um
subconjunto `--only` a mantém.

Em seguida, configure a CLI do agente. Ambos os comandos têm como padrão
`http://127.0.0.1:49374` — nenhuma flag adicional é necessária para o caso local:

```bash
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

O wrapper Docker instalado executa comandos de CLI dentro de um contêiner auxiliar
de curta duração para operações de servidor/armazenamento, mas executa `install-hooks`,
`mcp-bridge` e comandos de ciclo de vida `hook` através do cliente nativo
estável e verificado por checksum sob
`~/.local/share/ai-memory/native-runner/`. Isso mantém caminhos de comando de hook válidos
no host e impõe exclusões de captura antes do spooling. Um comando de hook
que chama o wrapper (`ai-memory hook --event …`) portanto executa nativamente em
poucos milissegundos e faz POST para o `127.0.0.1:49374` do host; ele nunca inicia um
contêiner. Um hook apenas reutiliza o cliente instalado: `install-hooks` e
`mcp-bridge` o instalam e atualizam, enquanto o caminho de hook nunca verifica
atualizações nem faz downloads. Se nenhum cliente estiver instalado ainda, o wrapper avisa para executar
`ai-memory install-hooks` uma vez, descarta esse evento único e sai com código 0 para que o agente
não seja bloqueado. Para servidores
loopback locais, o wrapper conecta automaticamente
esse auxiliar de volta ao `127.0.0.1:49374` do host, de modo que `ai-memory status`,
`ai-memory search`, `ai-memory recent` e `ai-memory bootstrap` funcionem com a mesma URL
padrão da configuração gerada do agente.

#### Hosts com imposição de SELinux

Em sistemas Linux com imposição de SELinux como Fedora, RHEL e openSUSE, rótulos
normais de diretório de usuário podem impedir o contêiner auxiliar de alcançar a configuração
do agente mesmo quando seu UID e GID coincidirem com o usuário do host. O wrapper verifica tanto
o modo de imposição do host quanto as opções de segurança anunciadas pelo motor. Para os
comandos auxiliares de curta duração que tocam arquivos do host (`install-*`, `setup-agent`,
`uninstall`, `backup`, `restore` e `bootstrap`), ele adiciona `--security-opt
label=disable`; comandos de cliente leve permanecem confinados quando usam o volume
de dados nomeado e a configuração implícita. Um caminho explícito `--config` ou um
`AI_MEMORY_DATA_DIR` válido no host também ativa o tratamento de arquivos do host. Isso
relaxa o confinamento de rótulos do SELinux apenas para aquela invocação confiável do auxiliar. Isso
não modifica o servidor ai-memory de longa duração, que usa um volume
nomeado gerenciado pelo motor.

`bootstrap` está nessa lista mesmo que apenas *leia* arquivos do host: um
UID não mapeado e um rótulo confinado bloqueiam leituras da mesma forma, e a falha é
enganosa: ela degrada silenciosamente para `no .git found at /work; bootstrapping
from README/docs/rules only` antes de encerrar com `Permission denied (os error
13)`.

Os dois motores relatam esses fatos sob chaves diferentes. O Docker responde
`docker info --format '{{.SecurityOptions}}'`; o podman não possui esse campo e
falha nesse modelo, portanto o wrapper recorre a
`{{.Host.Security.Rootless}}` e `{{.Host.Security.SELinuxEnabled}}` do podman quando a
sondagem do Docker retorna vazia. Motores rootless necessitam adicionalmente de `-u 0:0`,
porque apenas o UID 0 do contêiner é mapeado de volta para o usuário invocador do host. No podman
rootless com imposição de SELinux, ambos os ajustes são necessários e nenhum deles sozinho
permite a gravação.

Não adicione `:z` ou `:Z` à montagem de todo o `$HOME` do wrapper. A
[documentação de montagem bind](https://docs.docker.com/engine/storage/bind-mounts/#configure-the-selinux-label)
do Docker adverte que rotular novamente diretórios de sistema como `/home` pode tornar o host
inoperável. O Docker documenta `label=disable` nas
[opções de segurança do `docker run`](https://docs.docker.com/reference/cli/docker/container/run/#security-opt).

`ai-memory run`, `ai-memory show`, `ai-memory continue`, `ai-memory resume`,
`ai-memory workstreams` e `ai-memory rename-workstream` também são exceções:
o wrapper atual os intercepta e inicia um cliente nativo verificado por checksum
em cache no host, onde checkouts locais, executáveis de harness e repositórios de
sessão existem. Ele preserva um `AI_MEMORY_SERVER_URL` remoto explícito. Se um
desses comandos registrar `data_dir=/data`, não encontrar um checkout ou não encontrar
o `codex`, `claude` ou outro executável do host, atualize o wrapper desatualizado com
`ai-memory upgrade` nessa máquina cliente.

### Alternativa com Docker compose

Se você preferir o compose, clone o repositório e execute:

```bash
docker compose -f docker/docker-compose.yml up -d
```

O arquivo compose empacotado já possui `restart: unless-stopped`, uma
verificação de integridade (healthcheck) e o volume nomeado configurados. A configuração do agente é a mesma
do caminho padrão do Docker.

---

## Mantendo o ai-memory atualizado

### Wrapper Docker

O wrapper verifica o Docker Hub no máximo uma vez a cada 24 horas e imprime um
aviso de uma linha quando uma imagem mais recente estiver disponível. Atualize com:

```bash
ai-memory upgrade
```

O comando baixa o wrapper e seu checksum SHA-256 do GitHub Release mais
recente, recusa uma atualização não verificada, baixa a imagem Docker mais
recente, atualiza o cliente nativo do host verificado por checksum, reescreve registros
de hooks preparados anteriormente para esse comando nativo quando compatível e
imprime como reiniciar o contêiner do servidor para que o novo binário seja usado.
Executar novamente `install-hooks --apply` permanece idempotente: o ai-memory
substitui apenas as entradas de hook que possui e não interfere em hooks não relacionados.
Quando um arquivo Compose é encontrado, o wrapper primeiro verifica se o projeto possui
o contêiner `ai-memory` em execução. Um contêiner independente nunca é repassado a um
projeto Compose não relacionado apenas porque seu arquivo ocupa um caminho convencional;
em vez disso, o wrapper grava o script de recriação independente inspecionado para
revisão, preservando a montagem `/data` existente e outras opções de tempo de execução.

O aplicativo da barra de menus do macOS não é coberto pelo `ai-memory upgrade`. Recompile com
`./companions/ai-memory-macos/build.sh` (ou substitua `/Applications/AI Memory.app`
por um pacote preparado mais recente). Wiki, SQLite, configuração e modelos permanecem em
`~/Library/Application Support/ai-memory`.

Defina `AI_MEMORY_NO_VERSION_CHECK=1` para silenciar a verificação diária. Para fixar atualizações
automáticas do wrapper a um fork ou release tagueada, defina `AI_MEMORY_WRAPPER_URL=<url>`;
o wrapper exige `<url>.sha256`, a menos que
`AI_MEMORY_WRAPPER_SHA256_URL=<checksum-url>` também esteja definida.

### Binário de release nativo (Linux / macOS / Windows x86_64)

Quando o `PATH` apontar para um binário `ai-memory` do GitHub-release sob um prefixo de
usuário gravável (por exemplo `~/.local/bin`, ou `%LOCALAPPDATA%\ai-memory` no
Windows), o mesmo comando atualiza o próprio binário:

```bash
ai-memory upgrade
# optional: pin a tag, or force a re-download of the current tag
ai-memory upgrade --version v2.3.2
ai-memory upgrade --force
```

O caminho nativo baixa o arquivo compactado de release correspondente
(`ai-memory-<os>-<arch>.tar.gz` no Unix, `ai-memory-windows-x86_64.zip` no
Windows) e seu arquivo auxiliar `.sha256` dos GitHub Releases, verifica o
checksum, substitui o binário em disco (e um diretório `hooks/` irmão quando
presente) e, em seguida, prepara novamente os hooks para os agentes que já estão sob a árvore
de hooks do diretório de dados. Os arquivos de serviço empacotados do arquivo compactado, a configuração padrão e a documentação
são verificados contra o layout exato do fluxo de trabalho de release, mas não são extraídos
durante a autoatualização. Os arquivos compactados são limitados a 4.096 entradas e 512 MiB expandidos,
além do limite de corpo de resposta de 128 MiB. O Windows usa renomeação paralela
(`.exe` → `.old`, depois promove `.new`) porque
uma imagem em execução não pode ser sobrescrita no local. Ele recusa instalações do Homebrew/AUR/Scoop/`/usr`
(use o gerenciador de pacotes), prefixos não graváveis (por exemplo Arquivos de
Programas; baixe o zip manualmente) e binários dentro de contêineres (atualize o
wrapper/imagem do host em vez disso). O Windows aarch64 ainda não possui um asset de release. Para
espelhos ou testes herméticos, defina `AI_MEMORY_RELEASE_BASE_URL` (ou
`release_base_url` em config.toml) para uma base compatível com Releases que sirva
`{base}/latest/tag` e `{base}/download/<tag>/<asset>` (+ `.sha256`). Essa
sobreposição é uma fronteira de confiança: o arquivo compactado e o checksum são buscados da mesma
base, portanto o `.sha256` apenas comprova que a base serviu um par consistente, não que
o binário seja genuíno. Aponte apenas para origens que você controla. Prefira uma
base `https://`; uma base puramente `http://` não possui proteção de trânsito, de modo que um
invasor on-path pode substituir tanto o arquivo compactado quanto seu checksum correspondente.
Cada corpo de resposta é limitado a 128 MiB.

### Contêiner do servidor executado por você mesmo, cliente nativo no host

Algumas instalações dividem as duas metades manualmente: o servidor roda em um contêiner que
você inicia com seu próprio `docker run` ou arquivo Compose (a partir da imagem oficial ou
de uma derivada dela), e o host possui o binário nativo do `install.sh` para
hooks, MCP e a CLI, apontado para `http://127.0.0.1:49374`. O
[wrapper Docker](#wrapper-docker) é esse mesmo layout gerenciado para você: seu
`upgrade` baixa a nova imagem, atualiza o cliente do host e imprime como
reiniciar o contêiner. Use-o a menos que precise de opções de tempo de execução que ele não
cubra.

Sem o wrapper, `ai-memory upgrade` no host segue o caminho nativo
acima: substitui o binário do host e prepara novamente seus hooks, não
tocando no contêiner. Como a URL do servidor é loopback, ele também não exibe nota
de servidor remoto, portanto a linha de sucesso não significa que o servidor foi
atualizado. Atualize o servidor você mesmo:

1. Faça um backup (`docker exec ai-memory ai-memory backup --to
   /data/pre-upgrade.tar.gz`); migrações são exclusivamente para a frente.
2. `docker pull akitaonrails/ai-memory:<tag>`, e recompile qualquer imagem derivada
   a partir dessa tag.
3. Recrie o contêiner com o mesmo volume `/data`, mapeamento de portas e
   ambiente.

Mantenha as duas versões alinhadas: `ai-memory status` exibe a versão do servidor
e `ai-memory --version` a do cliente do host.

Nesse layout, chaves de provedor e outras configurações de servidor passadas com `-e` existem
apenas dentro do contêiner. Um comando no host que construa um cliente de provedor diretamente
não pode visualizá-las, portanto `ai-memory llm-test` no host falha mesmo enquanto o
provedor do servidor funciona: com um erro de chave ausente ao construir o provedor,
ou, para `openai-compat`, com o provedor rejeitando uma requisição enviada sem
chave. Execute-o no contêiner em vez disso:

```bash
docker exec ai-memory ai-memory llm-test --provider <provider> --model <model> --prompt "Reply with OK"
```

A mesma fronteira se aplica a arquivos que o servidor lê. Um caminho de configuração, ou um
comando de [avaliador de pontuação (eval scorer)](auto-improve-eval-gates.md), deve nomear um caminho dentro do
contêiner, por exemplo sob o volume `/data`, não um caminho no host.

### Notas compartilhadas

Quando o servidor atualizado inicia, ele aplica migrações de esquema do SQLite e
migrações pendentes de estrutura do wiki automaticamente. Nenhuma reinicialização manual
do banco de dados ou reescrita do wiki é necessária para atualizações normais. As migrações
são apenas para a frente: depois que uma versão mais recente aplicou seu esquema, um binário mais antigo
se recusará a abrir esse diretório de dados (ele falha de forma segura em vez de arriscar
corrupção), portanto faça um `ai-memory backup` antes de atualizar se você puder precisar
reverter para a versão anterior.

Se a inicialização relatar que o **histórico de migrações possui uma lacuna**, uma
migração anterior está ausente mesmo que uma posterior esteja registrada. Isso pode
ocorrer com uma compilação de desenvolvimento cuja sequência de migrações diferiu da
sequência de lançamento. Mantenha um backup e relate a migração ausente e a
versão usada para criar o repositório para análise; a inicialização não repara
automaticamente o histórico nem aplica a migração ignorada.

### Atualizando para 3.0

A versão 3.0 remove os comandos 1.x `user add`, `user expire`, `user revive` e
`user rotate-token`; os tokens emitidos por eles continuam funcionando como chaves de API.
[`upgrading-to-3.0.md`](upgrading-to-3.0.md) aborda o que mudou, as etapas
de atualização e como fazer o rollback.

### Atualizando para 2.3.0

A 2.3.0 é uma atualização normal para a frente (a única nova migração, `V64`, apenas adiciona a
tabela de projetos cruzados `agent_messages`; nada existente é alterado ou removido).
Duas conveniências de captura/UX estão **ativadas por padrão**; ambas são aditivas e
não destrutivas, mas vale a pena conhecê-las para sua primeira sessão após a atualização:

- **O primeiro `ai-memory run <harness>` autoinstala os hooks + MCP daquele harness** se
  eles ainda não estiverem configurados (idempotente, uma única vez por harness; preserva
  sua configuração de hooks existente, incluindo a opção `--capture-assistant`). Desative
  com `ai-memory run --no-autowire` ou `AI_MEMORY_RUN_AUTOWIRE=false`.
- **A primeira sessão em um projeto novo (vazio) importa o histórico local existente do
  harness uma vez**, para que instalar o ai-memory em um projeto em andamento não seja
  amnésico. Ele é executado apenas em projetos vazios (nunca toca em projetos que
  já possuam memória capturada) e tem limite estrito. Desative com
  `AI_MEMORY_BACKFILL_ON_START=false`; execute manualmente com `ai-memory backfill`.

Se o servidor for executado em outro host, `ai-memory upgrade` atualiza apenas
o cliente local (wrapper/imagem ou binário nativo) e os scripts de hooks locais.
Implante novamente o servidor remoto separadamente com `bin/deploy` ou
`docker compose pull && docker compose up -d` naquele diretório de deploy.

Dentro do ai-jail ou de outra sandbox bwrap, o wrapper pode ser usado a partir da
sandbox, mas execute comandos `install-*` fora da sandbox porque eles
gravam em `~/.local/share/ai-memory/hooks/`.

---

## Consulte também

- [`docs/macos.md`](macos.md) - caminhos de instalação no macOS: app da barra de menus, tarball
  de release nativo, compilação a partir do fonte, wrapper Docker e launchd
- [`docs/deploy.md`](deploy.md) - passo a passo de deploy em homelab
  (`bin/deploy`, TLS com cloudflared, gerenciamento de arquivos env)
- [`docs/usage.md`](usage.md) - handoffs, consultas proativas, UI web, snippet
  de roteamento enxuto + Agent Skills gerenciadas, migração de outras ferramentas de memória e inspeção direta da wiki
- [`docs/mcp-install.md`](mcp-install.md) - referência de configuração de MCP por cliente para
  cada cliente na [Matriz de compatibilidade do README](../../README.md#support-matrix)
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) - o que realmente está
  em execução dentro do ai-memory
