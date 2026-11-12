# Conformidade com OKF (2.0)

## O que isso traz para você

Sua memória é portátil além do ai-memory. Entregue um pacote (bundle) de projeto para um
colega de equipe que execute uma ferramenta compatível com OKF *diferente*, ou nenhuma ferramenta,
e essa pessoa lerá suas decisões, gotchas e procedimentos como markdown
comum com metadados padrão:

```bash
ai-memory export-okf --project myproject -o myproject-bundle.tar.gz
```

O lado receptor descompacta um diretório de arquivos `.md` onde cada página
declara seu `type`, proveniência (`generated`, `sources`) e atualização
(`stale_after`) no vocabulário padronizado pelo Open Knowledge Format do Google.
Os arquivos são pesquisáveis via grep, abrem no Obsidian e são importados para
qualquer ferramenta compatível com OKF. Nada fica preso: a exportação é uma cópia validada dos
arquivos nos quais o ai-memory já armazena sua memória.

Rótulos de wikilinks locais contendo colchetes ou barras invertidas permanecem como links Markdown
comuns em um bundle exportado. A reindexação dos arquivos de conceitos extraídos
preserva seus destinos de link.

Downloads de backup e de exportação OKF substituem sua saída somente após a chegada de toda
a resposta. Um download interrompido mantém o arquivo anterior intacto.
Novos arquivos e symlinks de substituição são privados; arquivos regulares existentes mantêm
suas permissões e ainda exigem acesso de escrita. Arquivos somente leitura e
destinos de diretório são recusados. O destino de um symlink nunca é alterado.

O restante desta página trata do design: como a conformidade é aplicada e
como as bases de dados existentes migram.

---

A wiki do ai-memory é nativamente um bundle do **Open Knowledge Format** a
partir da versão 2.0: cada página que um consumidor lê do disco é um arquivo de conceito
conforme ao OKF, e o diretório de wiki de um projeto é um bundle conforme.
"Nativo" significa que os arquivos da wiki *são* os arquivos OKF, portanto nenhuma etapa de exportação
bifurca a verdade (um par `export --okf` / `import --okf` ainda existe
para mover bundles entre ferramentas).

## Alvo: OKF v0.2

Especificação: `GoogleCloudPlatform/knowledge-catalog`, `okf/SPEC.md` (verificado
em 2026-09-01). v0.2 substitui v0.1 com duas alterações incompatíveis
(`timestamp` → `generated: {by, at}`; a seção do corpo `# Citations` →
frontmatter `sources`) e famílias aditivas de confiança/ciclo de vida/proveniência.
Resumo do que a conformidade exige:

- todo arquivo `.md` não reservado: frontmatter YAML analisável com um
  **`type`** não vazio;
- `index.md` na raiz do bundle declara **`okf_version: "0.2"`** (o único
  frontmatter de index.md permitido) e lista o diretório;
- nomes reservados `index.md` / `log.md` seguem a estrutura da especificação quando
  presentes;
- consumidores DEVEM tolerar chaves desconhecidas, portanto todos os campos de extensão
  do ai-memory são seguros perante a especificação como estão.

## Mapeamento de campos

Links Markdown comuns usam codificação de caminho de URL: `notes/my%20note.md` aponta
para `notes/my note.md`. A importação e a interface web decodificam esse caminho uma vez antes
de resolvê-lo. Uma sequência literal de porcentagem em um nome de arquivo usa `%25` em uma
URL Markdown, enquanto `[[wikilinks]]` continuam nomeando caminhos literais de página.

| Chave OKF | Origem ai-memory |
|---|---|
| `type` (obrigatório) | derivado da família de caminhos + frontmatter existente: `sessions/` → `Session Summary`, `_rules/` → `Rule`, `gotchas/` → `Gotcha`, `decisions/` → `Decision`, `procedures/` → `Procedure`, `concepts/` → `Concept`, `notes/` → `Note`, `runbooks/` → `Runbook`, `_slots/` → `Invariant`/`State` (de `slot_kind`), `_lint/` → `Lint Report`, `_pending/` → `Pending Note`; frontmatter `kind:` (`fact`/`note`/`procedure`/`decision`) prevalece sobre o padrão de caminho quando presente |
| `title` | não é gravado por `conform_frontmatter` no caminho de escrita geral; apenas um valor explícito de frontmatter `title:` sobrevive lá. `export-okf` preenche um `title` ausente/vazio no momento da exportação a partir de `derive_title` (título H1, ou base do caminho); o arquivo da wiki no disco nunca é alterado |
| `description` | `conform_frontmatter` o preenche a partir de `summary` no momento da escrita, quando presente. `export-okf` adicionalmente recorre a `abstract` como fallback quando `summary` estiver ausente, mas apenas na cópia exportada, portanto uma página sem nenhum dos dois na escrita ainda não terá `description` até ser exportada |
| `tags` | já gravado |
| `generated.by` | convenção de ator: `process:ai-memory/<version>` para o consolidador sem LLM (zero-LLM) e gravadores do sistema; `<provider-model>` (ex: `openai-compat/qwen3:32b`) para páginas geradas por LLM; `human:<user>` para edições de wiki atribuídas pelo watcher |
| `generated.at` | o `updated_at` da versão da página |
| `sources` | proveniência da sessão: páginas já carimbadas com `session_id`/`agent` recebem `[{resource: "ai-memory://session/<uuid>", author: "process:<agent>"}]`, a forma de ator `process:<id>` (§5.1), já que uma versão semântica por harness não pode ser derivada de maneira confiável |
| `stale_after` | `expires_at` (TTL) existente, quando presente: um valor RFC 3339 literal, um simples `YYYY-MM-DD` como o final daquele dia em UTC (`2026-10-01T23:59:59.999999Z`), já que os timestamps do OKF trazem um deslocamento explícito |
| `status` | `deprecated` quando expirado por TTL mas retido; caso contrário omitido (padrão da especificação `stable`) |

Campos de extensão mantidos literalmente (chaves desconhecidas são conformes): `tier`,
`kind`, `slot_kind`, `entities`, `pinned`, `consolidated`,
`session_id`, `agent`, `summary`, `expires_at`, `source_files`.
`source_files` (`[{path, commit}]`, os arquivos do repositório descritos por uma página) é
deliberadamente uma chave separada: `sources` do OKF contém a proveniência
da sessão da página, e os dois nunca compartilham entradas.

## Limite do bundle

Um **diretório de escopo de projeto = um bundle**: a unidade portátil de
conhecimento é um projeto. Cada diretório de projeto recebe um `index.md` gerado
(frontmatter `okf_version: "0.2"`, corpo = listagem do diretório). O
manifesto de escopo `_meta.md` existente não é alterado: ele é o registro de
identidade do ai-memory, e `index.md` é a descrição voltada ao OKF. Nada na
árvore atual grava `index.md`, portanto esse nome reservado está livre.
`log.md` não é adotado: o git é o log.

Os hooks *realmente* gravam um registro bruto de eventos por mês na raiz do projeto
(`log-YYYY-MM.md`: linhas `## [ts] event | title`, sem frontmatter). Trata-se
de captura, não de um arquivo de conceito, portanto a exportação o descarta exatamente como
descarta `log.md`, e a barreira de conformidade nunca o vê (#748). A
exclusão é filtrada por conteúdo, da mesma forma que a verificação de migração
(#669): uma página comum que coincidentemente se chame `log-2026-09.md`
ainda é incluída no bundle e ainda deve declarar um `type`.

## Aplicação: um único ponto de controle

Toda escrita de página é afunilada através de `ops::upsert_page_in_tx`. Uma
normalização determinística `okf::conform_frontmatter(path, frontmatter, meta)`
executa ali para cada nova versão: preenche `type` /
`generated` / `sources` / `stale_after` a partir do mapeamento acima,
não altera nada que já esteja presente e não inventa nada não derivável.
O determinismo é crucial: a verificação de idempotência por conteúdo idêntico faz hash do
frontmatter, de modo que conformar a mesma entrada duas vezes deve gerar bytes
idênticos.

## Migração de bases existentes

A ordem é fixa; cada etapa condiciona a seguinte:

1. **Backup proativo, primeiro, sempre.** A migração compacta todo
   o diretório de dados (wiki, banco de dados SQLite, manifestos) para
   `~/ai-memory-backup-pre-2.0-<date>.tar.gz` (fora do diretório de dados),
   executa fsync nele, verifica se o arquivo é listável e possui tamanho plausível, e **aborta se o
   backup não puder ser gravado ou verificado**. O caminho do arquivo é registrado
   no manifesto meta da wiki.
2. **Reescrita de frontmatter in-place.** Mesmo id de página, mesma linha de versão,
   corpo intocado, `updated_at` intocado: sem explosão de versões, sem
   invalidação de embeddings, sem corrida de `updated_at`. Um único commit git
   ("okf-migration") na wiki, após um commit de checkpoint
   pré-migração. Reindexação posterior.
3. **Marcador de geração**: a migração é distribuída como uma `WikiMigration`
   (rastreada na tabela `wiki_migrations`), e o executor agora se recusa
   a abrir uma wiki cuja tabela registre uma migração que este binário não
   conhece (`NewerWikiFormat`). Esta é a proteção contra downgrade que espelha a regra
   de esquema à frente do banco de dados. Manifestos de escopo `_meta.md` recebem apenas seu `type`;
   são registros de identidade, não páginas de conceitos.
4. **Idempotente**: uma nova execução migra zero páginas.
5. **Aviso na página inicial** até que o arquivo compactado seja excluído: caminho, tamanho, data,
   além de "tudo parece correto → exclua o arquivo" e "algo
   está faltando → passos de restauração" (com link para `MIGRATION-2.0.md`).

Reversão (rollback): restaure o arquivo de backup (direto, sem necessidade de conhecimento de git), ou o
checkpoint git pré-migração + `reindex` (cirúrgico).

### Reparos em bases já migradas

Um bug de conformidade descoberto após a migração de uma base é reparado por uma
passagem idempotente na inicialização, e não por uma nova `WikiMigration`: um nome de
migração registrado faz qualquer binário anterior recusar a wiki
(`NewerWikiFormat`), um salto de geração de formato que uma correção de patch não deve
forçar. A passagem segue as
regras sem rotatividade da migração (linha no mesmo lugar, mesma versão, `updated_at` e
`generated.at` intocados, corpo intocado, um único commit git) e não faz nada
assim que a base estiver limpa. `conform_frontmatter` aplica o mesmo reparo,
de modo que qualquer reescrita posterior de uma página afetada (uma restauração, edição manual,
`reindex`) a corrige também.

- **`stale_after` apenas com data.** Compilações anteriores à correção copiavam uma data
  `expires_at` simples para `stale_after` literalmente. `serve` reescreve um
  `stale_after` que seja igual ao seu `expires_at` apenas com data para o instante indicado
  pelo TTL (`2026-10-01` → `2026-10-01T23:59:59.999999Z`); um
  `stale_after` que divirja de `expires_at` não foi derivado pelo
  ai-memory e é deixado intocado.

## Rede de segurança reconcile-delete (opcional, #929)

A passagem de reconciliação de 30s do watcher só reindexa eventos de criação/modificação por
padrão: uma página cujo arquivo desaparece do disco permanece indexada até que
`ai-memory delete-page` a remova explicitamente. `[maintenance]
reconcile_tombstones_deleted_pages` (padrão `false`) ativa uma rede de segurança
em segundo plano que fecha essa lacuna, projetada para ser segura embora adicione uma
mutação automática a um loop de segundo plano:

- O arquivo de uma página deve ser observado como ausente em **duas passagens** de reconciliação
  consecutivas (não apenas uma) antes que qualquer ação ocorra, e a checagem é reverificada
  no sistema de arquivos ativo imediatamente antes da ação.
- Apenas páginas que a varredura poderia ter retornado são elegíveis. `bootstrap.md`,
  `_meta.md` e sidecars `_pending/` nunca são candidatos, e um projeto
  cuja varredura encontrou `NotFound` (um subdiretório desaparecido, todo o diretório do
  projeto perdido no meio da passagem) não contribui com evidência de "ausente" para essa passagem.
  Páginas `sessions/*.md` também são excluídas, por uma razão não relacionada: um
  reposicionamento `move-session` no mesmo espaço de trabalho pode deixar uma linha correta no banco de dados sem nenhum
  arquivo em seu novo escopo (um bug separado e pré-existente na realocação de arquivos de `move-session`,
  rastreado para resolução futura e não corrigido aqui). Este mecanismo
  destina-se exclusivamente a páginas de conteúdo importadas por OKF.
- Um disjuntor (circuit breaker) se recusa a atuar em um escopo inteiro quando mais de `max(3,
  50%)` de suas páginas candidatas parecerem ausentes em uma única passagem, OU quando uma
  varredura não parcial não encontrar nada em um escopo que possui candidatas. Esse
  último caso intercepta a assinatura de falha de montagem do tipo "o diretório existe, mas retornou vazio"
  para escopos pequenos demais para acionar a matemática percentual. Qualquer um dos
  formatos é muito mais provável de ser um problema de varredura/montagem (um volume desmontado, um checkout
  de git no meio da varredura) do que uma exclusão em massa genuína, sendo registrado como `warn`.
- A ação em si é um tombstone suave (`is_latest = 0` + `superseded_at`),
  o mesmo formato de linha que o descarte por decaimento (decay eviction) já utiliza, sendo coletado pela
  mesma varredura de exclusão definitiva de tombstones antigos usada pelo descarte por decaimento
  (`hard_delete_after_days`, agnóstica a tier/fixação); ela NÃO é isenta dessa
  varredura. A garantia precisa: **um tombstone de reconciliação nunca é destruído
  enquanto sua cadeia não tiver sucessor; se o arquivo retornar, a nova
  versão se reconecta à cadeia com tombstone em vez de começar do zero, de forma que
  nada fica órfão para a varredura de 180 dias destruir.** Concretamente, uma nova
  escrita em um caminho com tombstone (`upsert_page_in_tx`) substitui a linha com
  tombstone via `supersedes` e limpa seu `superseded_at`, transformando-a em um membro comum e
  protegido da cadeia de substituição. Esse é o mesmo mecanismo que já mantém
  o histórico normal de edições fora dessa varredura. Ele também nunca dispara
  a barreira de admissão BLOQUEANTE (nada pode recusá-la, pois trata-se de uma
  rede de segurança em segundo plano reagindo a um arquivo já desaparecido, e não uma
  exclusão iniciada pelo usuário) e nunca toca no sistema de arquivos (não há nada
  para gravar; o arquivo já se foi); ele DISPARA em fire-and-forget qualquer webhook
  observador/espelho não bloqueante no sucesso, para que um espelho saiba sobre
  o tombstone em vez de divergir silenciosamente. `restore-page` e o histórico
  de versões da página permanecem intactos.
- Com a flag desativada (o padrão), o comportamento é idêntico em bytes ao de antes da
  existência desse recurso: nada de novo é registrado em log, e a reconciliação nunca altera
  a base de dados.

## Testes (cada um com um controle que deve falhar em uma compilação quebrada)

- Ida e volta: página → arquivo OKF em disco → lido de volta idêntico.
- Conformidade: cada arquivo em uma base migrada possui frontmatter
  analisável + `type` não vazio; a raiz do bundle carrega `okf_version`.
- Sem rotatividade: ids de página, linhas de versão e `updated_at` idênticos em bytes
  ao longo da migração (controle: uma migração que substitui páginas falha).
- Idempotência: a segunda execução migra zero páginas.
- Barreira de backup: etapa de arquivamento quebrada → migração se recusa a executar.
- Aviso na página inicial renderiza o caminho gravado do arquivo e limpa quando
  o arquivo sumir.
- Bundle OKF v0.2 externo é importado para um projeto; `export-okf`
  emite um bundle que um leitor estrito aceita (uma página não conforme falha
  a exportação). A importação não possui comando dedicado, por design: o formato é
  nativo, portanto descompactar os arquivos de conceitos de um bundle no diretório de
  wiki de um projeto e deixar o watcher (ou `reindex`) ingeri-los É o
  caminho de importação. Sobrescrever um arquivo de conceito já importado tem sua nova
  versão embutida da mesma forma que um arquivo novo, sem necessidade de
  `ai-memory embed` manual. Excluir um arquivo não o remove do índice por
  padrão: o watcher só reconcilia eventos de criação/modificação, de modo que um
  arquivo de conceito excluído precisa de um `ai-memory delete-page` explícito a menos que a rede de segurança
  opcional abaixo esteja ativada (#929).
- Regressão de recuperação: linha de base LongMemEval reexecutada; nenhuma queda substancial.
