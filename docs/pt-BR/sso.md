# SSO / identidade corporativa

Esta página reúne o suporte existente a OIDC do ai-memory em um só lugar para
equipes cuja política de segurança de TI exige identidade centralizada (SSO) para
qualquer ferramenta na estação de trabalho antes de aprovar a instalação local. Não há
comportamento novo aqui: tudo já existe em
[`docs/install.md`](install.md) e [`SECURITY.md`](../../SECURITY.md). Esta
página responde a "oferece suporte ao nosso IdP?" sem precisar ler ambos na íntegra.

## Recursos aceitos hoje

`ai-memory auth login oidc-device` executa um fluxo de autorização de dispositivo OIDC
com qualquer emissor em conformidade com os padrões (Keycloak, Okta, Entra ID, etc.):

```bash
ai-memory auth login oidc-device \
  --issuer "https://issuer.example.com/realms/team" \
  --client-id "ai-memory-cli"
```

O access token resultante é armazenado por desenvolvedor e usado para duas coisas:

- **Autenticação nativa de hook de ciclo de vida**: o hook do lado do cliente que
  informa eventos da sessão do agente de código ao servidor.
- **Comandos HTTP de cliente leve** (`status`, `search`, `read-page`,
  `write-page`, `backup`, `embed` e similares), quando nenhum
  `AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` estático estiver configurado. A autenticação
  estática com bearer token tem precedência se ambos estiverem definidos.

Consulte [`docs/install.md`](install.md) (a seção `auth login oidc-device`)
para a referência completa do comando e notas de configuração de provedores.

## O que isto não é

- **Não** é um controle de acesso de login para o processo do servidor ai-memory em si. O
  modelo de autorização administrativa/root do próprio servidor é a hierarquia de token
  bearer root estático / token de usuário do banco de dados descrita na seção
  "Authentication and administrative authorization" de `SECURITY.md`. O OIDC identifica
  o *desenvolvedor humano* perante a CLI/hooks; ele não substitui a autorização
  administrativa no servidor.
- Se você colocar o servidor atrás de um gateway ou proxy reverso compatível com OIDC, esse
  gateway pode converter a identidade OIDC aceita na autenticação que o ai-memory
  entende (token bearer estático / token de usuário do banco de dados). O ai-memory em si não
  valida tokens OIDC junto ao seu IdP para acesso à API do servidor.
- As claims `sid` (sessão) de OIDC/Keycloak descrevem a sessão de login do provedor de
  identidade, não a sessão do agente de código que o ai-memory acompanha para isolamento
  por sessão (`[auto_scope]`). Não confunda as duas ao integrar um gateway.

## Modelo de implantação para ambientes que exigem SSO

1. Configure o fluxo de autorização de dispositivo do seu emissor OIDC (a maioria dos IdPs
   corporativos oferece suporte a isso nativamente para ferramentas de linha de comando).
2. Peça para cada desenvolvedor executar `ai-memory auth login oidc-device` uma vez, apontando
   para o seu emissor.
3. Se você também precisar da interface HTTP/administrativa do *servidor* atrás do seu IdP
   (não só a CLI/hooks), coloque um proxy reverso ou gateway compatível com OIDC na frente
   dele e converta identidades aceitas em tokens bearer estáticos / tokens de usuário do
   banco de dados do ai-memory. Veja as orientações de exposição de rede em `SECURITY.md` e
   [`docs/https-via-proxy.md`](https-via-proxy.md) para a parte de terminação
   TLS dessa configuração.

## Documentos relacionados

- [`docs/install.md`](install.md): referência completa de `auth login oidc-device`.
- [`SECURITY.md`](../../SECURITY.md): modelo de autenticação/autorização
  no lado do servidor.
- [`DATA_HANDLING.md`](../../DATA_HANDLING.md): quais dados transitam para onde.
