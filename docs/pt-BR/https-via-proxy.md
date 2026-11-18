# HTTPS via um proxy reverso

> O ai-memory **não** faz terminação TLS por conta própria, por design. Esta página é
> o guia do operador para colocá-lo atrás de um terminador TLS maduro
> (Caddy, Cloudflare Tunnel, nginx) de modo que tokens e cookies do `/web`
> trafeguem criptografados entre clientes e o servidor. A instalação padrão
> permanece em HTTP puro no loopback, portanto nada muda para usuários existentes
> na atualização.

## Quando você não precisa disso

Ignore o TLS completamente se você estiver em um destes cenários; o orçamento
de segurança é melhor aproveitado em outro lugar:

- **Usuário único, transporte MCP por stdio.** `claude mcp add ai-memory -- ai-memory serve --transport stdio` nunca toca a rede. Nenhum TLS com que se preocupar.
- **Servidor HTTP exclusivo para loopback**, usuário único, sem acesso ao `/web` a partir de outra máquina. `127.0.0.1:49374` é inacessível de fora do host; o TLS não protege nada aqui que o limite de loopback do kernel já não proteja.
- **Desenvolvimento local / experimentos pontuais.** Introduza o TLS quando o formato de implantação exigir; não antes.

O caminho feliz de usuário único documentado no Início Rápido do README é
este caso. A maioria das instalações do ai-memory nunca precisa de um proxy.

## Quando você precisa disso

Adicione um proxy com terminação TLS à frente do ai-memory quando qualquer uma destas condições
se aplicar:

- **O modo multiusuário está ativado** (ao menos uma linha de usuário existe; `[auth].token_pepper`
  é o pré-requisito de credencial). Chaves nativas `aim_` trafegam entre clientes e
  o servidor e podem ser interceptadas sobre HTTP puro na LAN. Consulte
  [`docs/users.md`](users.md).
- **O servidor está vinculado além do loopback** (`AI_MEMORY_BIND=0.0.0.0:49374` ou um IP roteável na LAN). Qualquer pessoa no segmento de rede enxerga o tráfego de tokens em texto puro e os cookies do `/web`.
- **Você acessa o `/web` a partir de uma máquina diferente** daquela que executa o ai-memory. O cookie de sessão do navegador definido após o login por senha transita em texto puro sobre HTTP.
- **Você está expondo o ai-memory além da LAN.** O Cloudflare Tunnel ou um Caddy com domínio público e Let's Encrypt são os dois padrões mais adotados por operadores de homelab.

O ai-memory recusa HTTP não autenticado fora de loopback por padrão. Implantações apenas
com Bearer para máquinas ainda podem vincular HTTP puro autenticado e receber um aviso
enfático porque essas credenciais continuam sujeitas à interceptação. A autenticação humana é
mais rigorosa: um listener fora de loopback recusa a inicialização a menos que
`AI_MEMORY_AUTH__SECURE_COOKIE=true`, que é o sinal explícito do operador
de que um proxy HTTPS confiável é responsável pela borda voltada ao navegador. A única exceção é
a rota de escape explícita `serve --allow-insecure-human-auth` (variável de ambiente
`AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`) para um operador que aceita
conscientemente senhas, credenciais, cookies de sessão e dados de memória em texto puro em
uma LAN privada. A opção vem desativada por padrão, não contorna a proteção de exposição
sem autenticação e exibe um aviso incondicional.

## Escolha um caminho

| Caminho | Melhor para | O que é necessário externamente |
|---|---|---|
| **Caddy + domínio público + Let's Encrypt** | Operadores com um nome de domínio + porta 80/443 acessível a partir da internet (a maioria dos homelabs atrás de um roteador com encaminhamento). | Registro DNS A/AAAA apontando para o seu IP. |
| **Caddy + CA interna (somente LAN)** | Multiusuário somente na LAN, sem exposição pública. Cada máquina cliente precisa confiar no certificado raiz do Caddy uma única vez. | Instalação única do certificado raiz por cliente. |
| **Cloudflare Tunnel** | "Não quero abrir portas no meu roteador": túnel somente de saída, TLS terminado na borda da Cloudflare. | Uma conta Cloudflare (plano gratuito funciona) + um domínio na Cloudflare. |
| **Arquivos de certificado externos (Caddy ou nginx)** | Você já possui uma CA corporativa ou de homelab emitindo certificados para seus serviços. | Os arquivos de certificado/chave, independentemente de como seu ambiente os gera. |
| **nginx** | Você já executa o nginx para outros serviços e quer uma única linguagem de configuração. | O mesmo que o Caddy: um domínio ou arquivos. |

Os modelos de compose em `docker/` estão prontos para copiar:

- [`docker/compose.tls.caddy.yml`](../../docker/compose.tls.caddy.yml): Caddy na frente, com variantes LE e CA interna documentadas em linha.
- [`docker/compose.tls.cloudflared.yml`](../../docker/compose.tls.cloudflared.yml): Sidecar Cloudflare Tunnel, zero portas abertas.

As seções abaixo detalham cada um.

---

## Caminho 1 — Caddy + domínio público + Let's Encrypt

O caminho mais limpo quando você possui um domínio e a porta 80/443 acessível. O Caddy
emite e renova automaticamente com o Let's Encrypt sem intervenção
do operador após a primeira inicialização.

### Modelo Compose

Copie `docker/compose.tls.caddy.yml` para seu diretório de implantação. O
bloco relevante é:

```yaml
services:
  ai-memory:
    image: akitaonrails/ai-memory:latest
    container_name: ai-memory
    restart: unless-stopped
    expose:
      - "49374"          # internal only — Caddy reaches it over the docker network
    volumes:
      - ai-memory-data:/data
    env_file:
      - .env.production  # AI_MEMORY_AUTH_TOKEN + AI_MEMORY_ALLOWED_HOSTS + your LLM provider creds

  caddy:
    image: caddy:2-alpine
    container_name: ai-memory-caddy
    restart: unless-stopped
    ports:
      - "80:80"           # for Let's Encrypt HTTP-01 challenges
      - "443:443"         # the only port your clients touch
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config

volumes:
  ai-memory-data:
    name: ai-memory-data
  caddy-data:           # cert + ACME account key live here. Back this up.
  caddy-config:
```

### Caddyfile

Um Caddyfile completo tem três linhas:

```caddyfile
memory.example.com {
    reverse_proxy ai-memory:49374
}
```

O Caddy irá:

1. Resolver o desafio ACME HTTP-01 na primeira requisição para aquele hostname.
2. Emitir um certificado Let's Encrypt.
3. Renovar automaticamente 30 dias antes da expiração.
4. Encaminhar cabeçalhos `Authorization: Bearer ...` (e seu fluxo de autenticação) intocados.
5. Definir `X-Forwarded-Proto: https` e `X-Forwarded-For: <client-ip>` automaticamente.

### Ajustes no `.env.production` do ai-memory

```bash
AI_MEMORY_AUTH_TOKEN=...long-random-token-from-generate-auth-token...
AI_MEMORY_AUTH__SECURE_COOKIE=true
AI_MEMORY_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
AI_MEMORY_BIND=0.0.0.0:49374
```

O `AI_MEMORY_ALLOWED_HOSTS` deve incluir o hostname público ou a proteção contra
DNS-rebinding do ai-memory recusará as requisições
encaminhadas pelo Caddy.

### Hospedando sob um subcaminho

Se o ai-memory compartilha um hostname com outros apps, mantenha o prefixo ao fazer proxy
e informe-o ao ai-memory:

```bash
AI_MEMORY_BASE_PATH=/wiki
```

Como alternativa, defina `base_path = "/wiki"` no `config.toml`; tanto o servidor
quanto os comandos de clientes leves leem essa opção. Para `serve`, `--base-path` substitui
`AI_MEMORY_BASE_PATH`, que por sua vez substitui o arquivo. Um valor explicitamente vazio
ou `/` seleciona a raiz do host mesmo quando o arquivo indica um prefixo.

```caddyfile
memory.example.com {
    handle /wiki/* {
        reverse_proxy ai-memory:49374
    }
}
```

**Não** use `handle_path /wiki/*` para esta implantação: ele remove `/wiki`
antes de encaminhar, enquanto o ai-memory atende intencionalmente todas as rotas sob o
prefixo configurado. Com o exemplo acima, clientes usam:

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/wiki/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com/wiki" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

A interface web fica então em `https://memory.example.com/wiki/web`; adicione
`AI_MEMORY_WEB_SLUG=/` se você desejar o navegador integrado ou a SPA personalizada
de `--web-ui-dir` em `https://memory.example.com/wiki` diretamente. O login
humano no console exige a SPA personalizada; a wiki integrada renderizada pelo servidor permanece
como dados protegidos em vez de uma página de autenticação.

**Regras de segurança em ambas as flags.** `AI_MEMORY_BASE_PATH` e
`AI_MEMORY_WEB_SLUG` passam pelo mesmo normalizador. Segmentos devem ser
caracteres não reservados da RFC 3986 (`[A-Za-z0-9-._~]`). Segmentos com ponto
(`.` / `..`) são rejeitados: eles significam "atual" e "pai" em uma
fronteira de segmento, portanto aceitá-los permitiria que um erro de digitação transformasse o prefixo
em travessia de diretório. Qualquer coisa fora do conjunto não reservado recorre a uma
montagem na raiz, e o log de inicialização informa o motivo. O redirecionamento de barra final
em `{base_path}{web_slug}/` preserva a query string em seu caminho para a
forma canônica.

### Configuração de cliente MCP (Claude Code demonstrado — outros seguem o mesmo formato)

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "https://memory.example.com" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

`https://` passa a valer, o token viaja em `Authorization: Bearer`, e
o certificado do Caddy é confiável em navegadores/curl/clientes MCP em qualquer lugar porque
o Let's Encrypt está em todo repositório de certificados raiz do sistema.

### O que pode dar errado

- **Porta 80 não acessível a partir da internet** → ACME falha. Sintoma: Caddy registra erros `Get "https://acme-v02.api.letsencrypt.org/...": ...`. Correção: encaminhe as portas 80 e 443 do roteador para o host do Caddy, OU mude para Cloudflare Tunnel (Caminho 3), que não precisa de portas abertas.
- **DNS ainda não propagado** → primeira emissão do certificado falha com `unauthorized: ...DNS name does not have any address`. Correção: aguarde ou verifique se o registro A aponta para o seu IP público.
- **Renovações de certificado falham silenciosamente meses depois** → Caddy registra a falha, mas você não lê os logs do Caddy. Correção: monitore com `journalctl -u docker-compose@... | grep -i 'renew\|error'` ou coloque healthchecks na frente do Caddy.

---

## Caminho 2 — Caddy com CA interna (somente LAN)

Você não tem um domínio público ou não quer expor nada
para a internet. A CA interna do Caddy gera um certificado raiz por servidor
que o operador instala **uma vez** no repositório de certificados
do sistema operacional de cada máquina cliente. Mesmo formato de rede do Caminho 1, sem dependência de internet, sem
encaminhamento de portas.

### Caddyfile

```caddyfile
{
    local_certs   # tells Caddy to use the internal CA instead of LE
}

homelab.local, 192.168.1.50 {
    reverse_proxy ai-memory:49374
}
```

Liste todo nome + IP que os clientes usarão (navegador, cliente MCP, curl)
no endereço do site. O Caddy inclui todos eles no SAN do certificado.

### A etapa de instalação de confiança (a etapa crítica)

O certificado raiz do Caddy fica em `<caddy-data>/caddy/pki/authorities/local/root.crt`
dentro do volume. Extraia-o uma vez:

```bash
docker compose exec caddy cat /data/caddy/pki/authorities/local/root.crt > caddy-root.crt
```

Em seguida, instale-o no repositório de certificados de cada sistema operacional cliente:

| Sistema Operacional | Comando |
|---|---|
| macOS | `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain caddy-root.crt` |
| Linux (Debian/Ubuntu) | `sudo cp caddy-root.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates` |
| Linux (Arch/openSUSE) | `sudo trust anchor --store caddy-root.crt` |
| Windows | `certutil -addstore -f "Root" caddy-root.crt` (PowerShell como Administrador) |
| iOS / Android | Envie o arquivo por e-mail para o dispositivo, abra-o, instale como perfil em Ajustes → Geral → VPN e Gerenciamento de Dispositivos. Depois **também** confie nele explicitamente em Ajustes → Geral → Sobre → Configurações de Confiabilidade de Certificados. |

**Aviso:** se você pular a etapa de instalação
de confiança em um cliente, esse cliente ou recusará conexões TLS
(clientes MCP, curl) ou treinará o operador a ignorar avisos
(navegadores). No último caso **você não tem nem a transparência do HTTP
nem a proteção do HTTPS**, apenas um certificado de fachada que
torna todos menos seguros. Instale o certificado raiz em toda máquina cliente
a partir da qual você se conectar, ou use o Caminho 1 / Caminho 3 em vez disso.

### Mesmo formato de `.env` + configuração de cliente que o Caminho 1

Substitua o domínio público por `https://homelab.local` (ou o SAN que você definiu).
Todo o restante é idêntico.

---

## Caminho 3 — Cloudflare Tunnel

O daemon `cloudflared` da Cloudflare estabelece um túnel somente de saída
para a borda da Cloudflare. **Nenhuma porta aberta no seu roteador**, nenhum IP
público necessário, TLS terminado na borda da Cloudflare com o certificado deles. Combina
particularmente bem com o caso de múltiplos usuários em homelab porque a premissa de
confiança é "a Cloudflare é a CA": universalmente confiável, sem etapa de instalação
por cliente.

### Configuração inicial na Cloudflare (feita uma vez)

1. Tenha um domínio na Cloudflare (o registrador pode ser outro; o DNS deve estar na Cloudflare).
2. No painel da Cloudflare, vá para **Zero Trust → Networks → Tunnels** → **Create a tunnel** → dê o nome `ai-memory-homelab` (ou outro) → salvar.
3. A Cloudflare fornece uma longa string de token. Guarde-a para o arquivo compose.
4. Adicione um hostname público ao túnel: `memory.example.com` → serviço `http://ai-memory:49374`. Salve.
5. (Opcional, mas recomendado) Proteja o hostname com uma aplicação **Cloudflare Access**. O SSO zero-trust da Cloudflare fica à frente do túnel e você ganha autenticação humana via Google/GitHub/etc. **além do** bearer token do ai-memory.

### Modelo Compose

Copie `docker/compose.tls.cloudflared.yml`. O bloco relevante:

```yaml
services:
  ai-memory:
    image: akitaonrails/ai-memory:latest
    container_name: ai-memory
    restart: unless-stopped
    expose:
      - "49374"          # tunnel reaches it over the docker network — no host port
    volumes:
      - ai-memory-data:/data
    env_file:
      - .env.production

  cloudflared:
    image: cloudflare/cloudflared:latest
    container_name: ai-memory-tunnel
    restart: unless-stopped
    command: tunnel --no-autoupdate run
    environment:
      - TUNNEL_TOKEN=${CLOUDFLARE_TUNNEL_TOKEN}

volumes:
  ai-memory-data:
    name: ai-memory-data
```

O `CLOUDFLARE_TUNNEL_TOKEN` vai no seu `.env` (ou no ambiente do compose). Nenhuma
porta exposta no host. Nenhuma configuração de DNS além da etapa no
painel acima (a Cloudflare gerencia o CNAME automaticamente).

### Ajustes no `.env.production` do ai-memory

```bash
AI_MEMORY_AUTH_TOKEN=...long-random-token...
AI_MEMORY_AUTH__SECURE_COOKIE=true
AI_MEMORY_ALLOWED_HOSTS=memory.example.com,localhost,127.0.0.1
AI_MEMORY_BIND=0.0.0.0:49374
CLOUDFLARE_TUNNEL_TOKEN=eyJ...long-base64-from-the-cf-dashboard...
```

### Configuração do cliente

Igual ao Caminho 1:

```bash
ai-memory install-mcp   --client claude-code --apply \
    --server-url "https://memory.example.com/mcp" --auth-token "$AI_MEMORY_AUTH_TOKEN"
```

### O que pode dar errado

- **Vazamento de token**. Qualquer um com o `CLOUDFLARE_TUNNEL_TOKEN` pode rodar um túnel para o seu hostname. Mantenha o arquivo de ambiente em `0600`, não o commite.
- **Túnel inativo + CF com DNS antigo em cache** → Cloudflare retorna 502 por alguns minutos após reiniciar. Geralmente se auto-corrige.
- **Políticas de Access confundidas com autenticação por bearer**. O Cloudflare Access (a camada opcional de SSO) é uma camada separada do bearer token do ai-memory. Ambas são executadas; ambas devem passar. Se o Access bloquear uma requisição, o ai-memory nunca chega a vê-la.

---

## Caminho 4 — Arquivos de certificado externos (Caddy ou nginx)

Você já possui uma CA emitindo certificados para seus serviços (PKI corporativa,
Vault no homelab, etc.). Você não quer que o Caddy emita os dele próprios.

### Caddyfile

```caddyfile
memory.example.com {
    tls /etc/caddy/certs/memory.crt /etc/caddy/certs/memory.key
    reverse_proxy ai-memory:49374
}
```

Monte o certificado + chave:

```yaml
services:
  caddy:
    # ... rest as Path 1 ...
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - /your/cert/path:/etc/caddy/certs:ro   # the cert dir
      - caddy-data:/data
```

O Caddy recarrega o certificado automaticamente quando os arquivos mudam. Nenhum reload manual é necessário.

### Equivalente em nginx

```nginx
server {
    listen 443 ssl http2;
    server_name memory.example.com;

    ssl_certificate     /etc/nginx/certs/memory.crt;
    ssl_certificate_key /etc/nginx/certs/memory.key;

    location / {
        proxy_pass http://ai-memory:49374;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        # MCP Streamable HTTP transport is request-response; chunked
        # bodies and SSE both rely on the next two lines.
        proxy_http_version 1.1;
        proxy_set_header Connection "";
    }
}
```

As diretivas `proxy_http_version 1.1` + `Connection ""` vazia são obrigatórias para
que o transporte Streamable HTTP do MCP faça streaming corretamente.

Mantenha as linhas `Host` e `X-Forwarded-For` em todo bloco nginx, incluindo um
bloco em HTTP puro. Sem elas, o nginx envia o endereço upstream como `Host` e nenhum
cabeçalho de encaminhamento, de modo que um servidor executando sem um bearer token não consegue identificar que a
requisição veio por meio de um proxy e a trata como a de um cliente local (consulte as
observações sobre requisições de navegadores em [`security.md`](security.md)).

---

## Caddy nativo (sem Docker)

Para operadores que executam o ai-memory a partir do código-fonte / AUR / `cargo run`
sem Docker:

```caddyfile
memory.example.com {
    reverse_proxy 127.0.0.1:49374
}
```

Instale o Caddy nativamente (`brew install caddy` / `pacman -S caddy` /
`apt install caddy`), coloque o Caddyfile no caminho canônico do sistema operacional
(`/etc/caddy/Caddyfile` no Linux, `/opt/homebrew/etc/Caddyfile` no
macOS) e execute `systemctl enable --now caddy` / `brew services start
caddy`. Todo o restante (LE, CA interna, certificados externos) funciona da
mesma forma que nos caminhos com Docker acima; o Caddy não se importa com qual lado do
limite do contêiner ele se encontra.

Para Cloudflare Tunnel: `cloudflared service install ${CLOUDFLARE_TUNNEL_TOKEN}`
instala e inicia o túnel como um serviço systemd no Linux ou um
LaunchDaemon no macOS. Mesmo formato da variante Docker.

---

## Requisições de longa duração: `bootstrap` e timeouts de ociosidade do proxy

O `ai-memory bootstrap` em um repositório grande mantém um **único POST aberto durante
toda a execução em múltiplos blocos** (frequentemente mais de 20 minutos) enquanto o servidor realiza
chamadas a LLM, sem que bytes trafeguem na conexão nesse intervalo. Um proxy reverso
com um timeout padrão de ociosidade/leitura à frente do servidor cortará essa
conexão (`Connection reset by peer`), e a execução é perdida.

Se você executar o `bootstrap` através de um proxy, aumente ou desative o timeout
de leitura/escrita upstream para a rota do ai-memory.

**Caddy**: desative os timeouts de leitura/escrita do backend no `reverse_proxy`:

```caddyfile
memory.example.com {
    reverse_proxy ai-memory:49374 {
        transport http {
            read_timeout 0
            write_timeout 0
        }
    }
}
```

**nginx**: aumente `proxy_read_timeout` / `proxy_send_timeout` (padrão 60s)
bem acima da sua execução mais longa:

```nginx
location / {
    proxy_pass http://127.0.0.1:49374;
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    # plus the http/1.1 + Connection "" lines from the nginx template above
}
```

Isso só importa para o POST de longa duração do `bootstrap`; requisições comuns de MCP e
de `/api/v1` são curtas e não são afetadas. (Um bloco que genuinamente falhe ainda
perde a execução hoje. A questão do progresso durável/`--resume` é rastreada
separadamente; veja #614.)

---

## Chamadas de saída para LLMs falham atrás de um firewall com inspeção TLS

Os caminhos acima tratam de clientes confiando no certificado do *ai-memory*. Esta seção aborda o caso inverso: o ai-memory confiando em um interceptador upstream para que
suas próprias chamadas de saída para LLM/embedding tenham sucesso.

**Sintoma.** Consolidação, lint ou embeddings falham e os logs mostram
`invalid peer certificate: UnknownIssuer` em chamadas para seu provedor de
LLM/embedding. Sua rede executa um proxy com inspeção HTTPS (um appliance MITM
corporativo, um antivírus com inspeção, um gateway estilo Zscaler/Netskope) que
reassina o TLS com uma raiz de interceptação privada em que o contêiner não confia.

**Por que isso acontece.** O cliente HTTP do ai-memory (reqwest + rustls, compilado com
`rustls-tls-native-roots`) confia no repositório de certificados do **sistema operacional**,
não em uma lista de raízes embutida. Ele lê o `/etc/ssl/certs` do contêiner e respeita
`SSL_CERT_FILE` / `SSL_CERT_DIR`. A imagem padrão distribui apenas as raízes
públicas do Debian, de modo que a raiz de interceptação é desconhecida. Um `curl` do seu host pode ter sucesso
porque seu host já confia nessa raiz; o contêiner não.

**Correção.** Forneça ao contêiner um pacote de CA que inclua a raiz de interceptação.

1. Exporte a raiz de interceptação que seu gateway apresenta (a partir de um host confiável),
   ou, melhor ainda, obtenha-a com sua equipe de TI, que é a fonte definitiva:

   ```bash
   openssl s_client -showcerts -connect api.openai.com:443 </dev/null 2>/dev/null \
     | openssl x509 -outform PEM > corp-root.crt
   ```

2. Monte um pacote combinado = raízes públicas do contêiner **mais** sua raiz.
   `SSL_CERT_FILE` *substitui* o arquivo padrão, portanto deve conter ambos:

   ```bash
   docker run --rm --entrypoint cat ai-memory:latest \
     /etc/ssl/certs/ca-certificates.crt > ca-bundle.crt
   cat corp-root.crt >> ca-bundle.crt
   ```

3. Monte-o e aponte o contêiner para ele. Qualquer uma das opções funciona:

   ```yaml
   # docker-compose.yml — option A: SSL_CERT_FILE
   services:
     ai-memory:
       volumes:
         - ./ca-bundle.crt:/etc/ai-memory/ca-bundle.crt:ro
       environment:
         SSL_CERT_FILE: /etc/ai-memory/ca-bundle.crt
   ```
   ```yaml
   # option B: overwrite the default path, no env var needed
       volumes:
         - ./ca-bundle.crt:/etc/ssl/certs/ca-certificates.crt:ro
   ```

4. Recrie o contêiner (`docker compose up -d`). O repositório de certificados é lido no
   início do processo, portanto um contêiner em execução deve ser reiniciado para reconhecer a
   alteração.

**Observações.**
- Aponte `SSL_CERT_FILE` para o pacote *combinado*, nunca apenas para a raiz de
  interceptação; isso descartaria todas as raízes públicas e quebraria todo o restante do TLS.
- Instalações nativas (sem Docker): instale a raiz no repositório do sistema operacional (os
  comandos na tabela de instalação de confiança do Caminho 2 funcionam para qualquer raiz) ou defina
  `SSL_CERT_FILE` / `SSL_CERT_DIR`. O cliente respeita ambos em qualquer sistema operacional,
  incluindo Windows e macOS, e eles também substituem o repositório do sistema operacional lá, de modo que a
  regra do pacote combinado acima continua valendo.
- `ai-memory mcp-bridge` é a exceção: no Windows e macOS ele ignora
  `SSL_CERT_FILE` / `SSL_CERT_DIR`, de modo que uma raiz na qual a ponte deve confiar precisa estar
  no repositório do sistema operacional. Consulte a seção sobre a ponte abaixo.

## O que o ai-memory faz para oferecer suporte a execução atrás de proxy

Nada de especial. O servidor intencionalmente não gera URLs absolutas
em respostas, de modo que não importa se `https://` ou `http://`
está à frente. O middleware de bearer token lê `Authorization`
diretamente da requisição, que os proxies encaminham literalmente. O
ETag de `/api/v1` é calculado a partir de campos independentes da requisição.

Para acesso de navegador ao `/web` via HTTPS, defina
`AI_MEMORY_AUTH__SECURE_COOKIE=true` (ou `[auth] secure_cookie = true`). Isso
marca o cookie `ai_memory_session` como `Secure`; ele é sempre `HttpOnly`,
`SameSite=Strict` e `Path=/`. O ai-memory intencionalmente **não** deduz
HTTPS de `X-Forwarded-Proto` ou de qualquer outro cabeçalho de proxy. Feche o acesso
HTTP direto ao hostname público, ou redirecione-o para HTTPS. A autenticação humana em um
listener fora de loopback não iniciará sem essa configuração. Ela permanece como false por
padrão apenas para que testes rápidos/desenvolvimento direto em loopback possam usar HTTP puro.

A única coisa com que se preocupar: **`AI_MEMORY_ALLOWED_HOSTS` deve incluir o
hostname público**, e não apenas `localhost`. O middleware da lista de permissões de hosts
executa antes de qualquer reescrita de cabeçalhos, portanto ele vê o
`Host: memory.example.com` encaminhado pelo proxy e o rejeitaria caso contrário.

### Clientes conectam-se diretamente, nunca através de um proxy de encaminhamento (forward proxy)

Os hooks (nativos, shell, PowerShell 7 e as integrações TypeScript) e
`ai-memory mcp-bridge` ignoram `HTTP_PROXY`, `HTTPS_PROXY` e `ALL_PROXY`, de modo que uma
sandbox que exporte um proxy de filtragem não pode interceptar suas requisições. Um
servidor que uma máquina cliente só consiga acessar através de um forward proxy de saída
fica, portanto, fora do alcance deles. Forneça ao servidor um endereço que o cliente possa
alcançar diretamente, como um proxy reverso na borda da rede ou um túnel como no
Caminho 3.

## Não tape o buraco de segurança superficialmente

Três coisas a evitar ativamente:

1. **Não desative a proteção de hosts permitidos.** Ela é a defesa contra DNS-rebinding; suprimi-la porque o proxy "deveria estar" filtrando é exatamente o tipo de suposição "a outra camada cuida disso" que introduz falhas. Adicione o hostname público; não amplie para `*`.
2. **Não pule a etapa de instalação de confiança no Caminho 2.** A tentação é adicionar `-k` (curl) ou `--insecure` (clientes MCP compatíveis) "só para fazer funcionar". Se fizer isso, você terá um certificado de segurança ilusória: TLS sem autenticação, o que é pior do que HTTP com bearer porque parece seguro e não é.
3. **Não execute o cloudflared com `--no-tls-verify`.** O daemon de túnel da Cloudflare valida o certificado do ai-memory por padrão, o que é seguro porque o ai-memory está em HTTP puro dentro da rede docker. Não substitua essa flag; recorrer a ela indicaria que outra coisa está mal configurada.

Se você não puder adotar um desses caminhos de forma limpa, mantenha o ai-memory apenas em loopback ou
coloque-o atrás do proxy em que você já confia. Uma configuração que transmita aos operadores
o modelo mental errado, parecendo segura sem realmente ser, é pior do que
qualquer uma das opções.

## A ponte MCP com reconhecimento de sessão e o HTTPS

`ai-memory mcp-bridge` alcança URLs de servidor `https://`. Versões anteriores não conseguiam:
seu transporte utilizava um segundo `reqwest` sem backend TLS compilado, de modo que qualquer
esquema diferente de `http` era recusado antes de tentar a conexão. Se você colocar um proxy
com terminação TLS à frente do ai-memory conforme descrito acima, aponte a ponte diretamente para a
URL `https://` do proxy; um segundo proxy local em cada máquina cliente não é
necessário.

A ponte usa o verificador de certificados da plataforma, portanto confia nas mesmas raízes que o
sistema operacional. Um certificado no qual o sistema operacional não confia (um autoassinado, ou uma
CA privada que não foi instalada no repositório de confiança do sistema) é rejeitado.
No Windows e macOS a ponte consulta o sistema operacional diretamente e ignora `SSL_CERT_FILE`
/ `SSL_CERT_DIR`; no Linux ela os respeita, como o cliente principal faz em toda plataforma. Uma
raiz fornecida apenas via `SSL_CERT_FILE` atende, portanto, a chamadas de provedores e
à CLI do `ai-memory` no Windows e macOS, mas não à ponte.
