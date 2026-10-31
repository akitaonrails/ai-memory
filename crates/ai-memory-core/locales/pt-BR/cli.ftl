## Human-facing command output. Keep flags, paths and identifiers verbatim.

cli-audit-contamination-no-structural-contamination-found-no-session-mis-bucketed-by-cwd-no-project = Nenhuma contaminação estrutural encontrada (nenhuma sessão atribuída ao projeto errado pelo cwd, nenhuma colisão de nomes de projetos).
cli-audit-contamination-contamination-audit-session-s-mis-bucketed-project-s-with-a-colliding-name = Auditoria de contaminação: { $value0 } sessão(ões) atribuída(s) incorretamente, { $value1 } projeto(s) com colisão de nomes.
cli-audit-contamination-session-landed-in-but-its-cwd-resolves-to-project = { "  " }[{ $value0 }] a sessão { $value1 } foi atribuída a { $value2 }/{ $value3 }, mas seu cwd ({ $value4 }) resolve para o projeto { $value5 }
cli-audit-contamination-project-page-s-session-s-last-activity-collides-by-with-add-a-ai-memory-toml = { "  " }[{ $value0 }] projeto { $value1 } ({ $value2 }/{ $value3 }): { $value4 } página(s), { $value5 } sessão(ões), última atividade { $value6 }; colisão por { $value7 } com { $value8 }. Adicione um marcador `.ai-memory.toml` ou execute `merge-project`.
cli-audit-contamination-in = { "  " }[{ $value0 }] { $value1 } { $value2 } em { $value3 }/{ $value4 }
cli-auth-github-token-client-id-issuer-do-not-apply-to-auth-login-openai-oauth = --github-token / --client-id / --issuer não se aplicam a `auth login openai-oauth`
cli-auth-issuer-applies-only-to-auth-login-oidc-device = --issuer se aplica somente a `auth login oidc-device`
cli-auth-github-token-applies-only-to-auth-login-copilot = --github-token se aplica somente a `auth login copilot`
cli-auth-open-this-url-complete = Abra esta URL: { $value0 }
cli-auth-enter-code = Digite o código: { $value0 }
cli-auth-waiting-for-authorization = Aguardando autorização...
cli-auth-oidc-device-logged-in = oidc-device: acesso autenticado
cli-auth-issuer-issuer = emissor: { $value0 }
cli-auth-token-file = arquivo do token: { $value0 }
cli-auth-oidc-device-logged-out = oidc-device: acesso encerrado
cli-auth-openai-oauth-token-response-did-not-include-refresh-token = a resposta do token openai-oauth não incluiu refresh_token
cli-auth-openai-oauth-logged-in = openai-oauth: acesso autenticado
cli-auth-account-account-id = conta: { $value0 }
cli-auth-openai-oauth-logged-out = openai-oauth: acesso encerrado
cli-auth-copilot-logged-in = copilot: acesso autenticado
cli-auth-copilot-logged-out = copilot: acesso encerrado
cli-auth-expires-in = expira em: { $value0 }
cli-auth-openai-oauth-not-logged-in = openai-oauth: sem autenticação
cli-auth-copilot-cached-token-valid-no-github-token-stored-for-refresh = copilot: token em cache válido (nenhum token GitHub armazenado para renovação)
cli-auth-copilot-not-logged-in-cached-token-expired-and-no-refreshable-github-token-s = copilot: sem autenticação (token em cache expirado e nenhum token GitHub renovável armazenado)
cli-auth-github-token-expires-in = o token GitHub expira em: { $value0 }
cli-auth-cached-copilot-token-expires-in = o token Copilot em cache expira em: { $value0 }
cli-auth-api-base-api-base-url = base da API: { $value0 }
cli-auth-copilot-not-logged-in = copilot: sem autenticação
cli-auth-oidc-device-not-logged-in = oidc-device: sem autenticação
cli-auto-improve-report-machine-readable = { "\u000A" }--- formato legível por máquina ---
cli-backfill-server-profile-name-was-refused = o perfil de servidor `{ $value0 }` foi recusado ({ $value1 })
cli-backfill-ai-memory-backfill-of-session-failed-error = ai-memory: falha na importação do histórico de { $value0 }, sessão { $value1 }: { $value2 }
cli-backfill-backfill-failed-to-import-of-selected-session-s = a importação do histórico falhou em { $value0 } de { $value1 } sessão(ões) selecionada(s)
cli-backup-wrote-backup-to = ✓ backup gravado em { $value0 } ({ $value1 })
cli-bootstrap-note-no-git-found-at-bootstrapping-from-readme-docs-rules-only-git-commit-hi = nota: nenhum .git encontrado em { $value0 }; inicializando apenas com README/documentação/regras (histórico de commits ignorado). Informe --repo-path ou execute `git init` para incluir commits.
cli-bootstrap-machine-readable-report = { "\u000A" }--- formato legível por máquina ---{ "\u000A" }{ $value0 }
cli-checkpoints-no-wiki-checkpoints-found = nenhum checkpoint da wiki encontrado
cli-commit-committed = commit criado: { $value0 }
cli-commit-nothing-to-commit = nada para registrar em commit{ $value0 }
cli-compact-compact-rewrites-the-whole-database-under-an-exclusive-lock-every-write-bloc = compact reescreve todo o banco de dados com bloqueio exclusivo: todas as gravações ficam bloqueadas até o término, e é necessário espaço livre em disco aproximadamente igual ao tamanho do banco.{ "\u000A" }{ "\u000A" }Verifique primeiro o espaço recuperável em `ai-memory status`, depois execute:{ "\u000A" }{ "\u000A" }  ai-memory compact --confirm
cli-compact-compacted-reclaimed = Compactado: { $value0 } → { $value1 } ({ $value2 } recuperados).
cli-compact-nothing-to-reclaim-the-database-had-no-meaningful-free-page-backlog-this-is = Nada para recuperar — o banco não tinha um volume significativo de páginas livres. Este é o resultado normal quando não houve uma exclusão grande recentemente.
cli-continue-session-no-managed-checkout-is-linked-for-this-server-yet-launch-one-with-ai-memory = nenhum checkout gerenciado está vinculado a este servidor ainda; inicie um com `ai-memory show` ou `ai-memory run <harness>` primeiro
cli-continue-session-skipping-client-registry-has-an-invalid-linked-at-timestamp = ignorando { $value0 }: o registro de clientes contém um horário linked_at inválido
cli-continue-session-skipping-label = ignorando { $value0 }: { $value1 }
cli-continue-session-continuing-in = continuando { $value0 } em { $value1 }
cli-continue-session-no-linked-checkout-is-still-usable-skipped-pick-one-explicitly-with-ai-memor = nenhum checkout vinculado continua utilizável ({ $value0 } ignorados); escolha um explicitamente com `ai-memory show`
cli-continue-session-checkout-now-resolves-to = o checkout agora resolve para { $value0 }/{ $value1 }
cli-curator-choose-either-dry-run-or-stage-not-both = escolha --dry-run ou --stage, não ambos
cli-delete-page-path-was-passed-empty-specify-the-wiki-page-path-to-delete = --path foi informado vazio; especifique o caminho da página da wiki a excluir
cli-delete-page-under = { $value0 } { $value1 } em { $value2 }/{ $value3 }
cli-doctor-resolving-the-current-working-directory = resolvendo o diretório de trabalho atual
cli-doctor-invalid-home-route-map-doctor-did-not-query-a-fallback-server-scope = mapa de rotas do diretório pessoal inválido; doctor não consultou um escopo alternativo do servidor
cli-embed-dry-run-would-embed-would-page-s-skipped-already-up-to-date = simulação: geraria embeddings de { $value0 } página(s), { $value1 } já atualizada(s)
cli-export-okf-exported-okf-bundle-to-size-bytes = pacote OKF exportado para { $value0 } ({ $value1 } bytes)
cli-finalize-session-ai-memory-no-home-directory-session-is-finalized-without-its-prompts = ai-memory: diretório pessoal indisponível; a sessão { $value0 } será finalizada sem seus prompts
cli-finalize-session-ai-memory-could-not-replay-the-prompts-of-session-error-finalizing-without-t = ai-memory: não foi possível reproduzir os prompts da sessão { $value0 } ({ $value1 }); finalizando sem eles
cli-handoffs-no-open-handoffs-for-workspace-project = Nenhuma transferência aberta para { $value0 }/{ $value1 }.
cli-init-generated-a-root-bearer-token-in-owner-only-the-server-requires-it-on-every = Token bearer root gerado em { $value0 } (acesso exclusivo do proprietário). O servidor o exige em todas as rotas, exceto /healthz; os hooks, a CLI e `ai-memory mcp-bridge` o leem desse arquivo.
cli-init-generating-auth-token-pepper-e = gerando token_pepper de autenticação: { $value0 }
cli-llm-test-model = --- modelo: { $value0 } ---
cli-llm-test-usage-in-out = --- uso: entrada={ $value0 } saída={ $value1 } ---
cli-llm-test-server-provider-provider-model-latency-ms = --- provedor do servidor: { $value0 } / { $value1 } ({ $value2 } ms) ---
cli-mcp-bridge-stored-bridge-credential-is-blank-repair-or-remove-it-instead-of-falling-bac = a credencial armazenada da ponte { $value0 } está vazia; repare ou remova em vez de recorrer ao acesso root
cli-merge-project-exactly-one-of-from-or-from-project-id-is-required = é necessário informar exatamente um de --from ou --from-project-id
cli-merge-project-exactly-one-of-into-or-into-project-id-is-required = é necessário informar exatamente um de --into ou --into-project-id
cli-merge-project-verb-page-versions-sessions-observations-handoffs-workstreams-page-conflicts = { $value0 } { $value1 } -> { $value2 }: { $value3 } versões de páginas, { $value4 } sessões, { $value5 } observações, { $value6 } transferências, { $value7 } workstreams, { $value8 } conflitos de páginas.
cli-merge-project-re-run-the-same-command-with-confirm-to-apply-this-merge = Execute novamente o mesmo comando com --confirm para aplicar esta mesclagem.
cli-merge-project-warning-merge-committed-but-scope-manifest-refresh-failed-warning = aviso: mesclagem concluída, mas houve falha ao atualizar o manifesto de escopo: { $value0 }
cli-message-reading-message-body-from-stdin-e = lendo o conteúdo da mensagem da entrada padrão: { $value0 }
cli-message-sent-message-from-from-workspace-from-project-to = Mensagem { $value0 } enviada de { $value1 }/{ $value2 } para { $value3 }/{ $value4 }.
cli-message-no-pending-mailbox-messages-for-workspace-project = Nenhuma mensagem { $value0 } pendente para { $value1 }/{ $value2 }.
cli-message-pending-mailbox-messages-for-workspace-project-oldest-first = Mensagens { $value0 } pendentes para { $value1 }/{ $value2 } (mais antigas primeiro):
cli-message-id = { "    " }id: { $value0 }
cli-message-no-pending-messages-in-workspace-project-s-inbox = Nenhuma mensagem pendente na caixa de entrada de { $value0 }/{ $value1 }.
cli-message-message-cancel-requires-exactly-one-of-id-message-id-or-all = message cancel exige exatamente um de --id <MESSAGE_ID> ou --all
cli-message-cancelled-message-in-workspace-project = { $value0 } { $value1 ->
        [one] mensagem cancelada
       *[other] mensagens canceladas
    } em { $value2 }/{ $value3 }.
cli-mod-ai-memory-scope-workspace-project-from = ai-memory: escopo { $value0 }/{ $value1 } ({ $value2 } de { $value3 })
cli-mod-project-candidate-does-not-exist = o projeto candidato não existe: { $value0 }
cli-mod-could-not-derive-project-name-from = não foi possível derivar o nome do projeto de { $value0 }
cli-mod-ai-memory-workspace-declared-workspace-from = ai-memory: espaço de trabalho { $value0 } (obtido de { $value1 })
cli-mod-the-ai-memory-wrapper-at-local-bin-ai-memory-looks-stale-it-didn-t-pass-ai-m = o wrapper `ai-memory` em ~/.local/bin/ai-memory parece desatualizado (não passou AI_MEMORY_HOST_CWD ao contêiner). Sem isso, todos os projetos seriam atribuídos a `default/work`, independentemente do diretório do host usado. Reinstale o wrapper com checksum verificado da versão mais recente do GitHub, conforme documentado em README.md,{ "\u000A" }  (ou execute `ai-memory upgrade` se o wrapper existente for recente o suficiente para reconhecer esse comando)
cli-mod-could-not-derive-project-name-from-cwd-pass-project-explicitly = não foi possível derivar o nome do projeto do CWD ({ $value0 }); informe --project explicitamente
cli-move-project-one-of-project-or-project-id-is-required = é necessário informar --project ou --project-id
cli-move-project-move-project-moves-to-workspace-if-the-destination-has-no-same-named-project = move-project move { $value0 }/{ $value1 } para o espaço de trabalho { $value2 }. Se o destino não contiver um projeto de mesmo nome, será uma movimentação sem perda (reatribuição no local — sessões, observações e histórico preservados). Se já existir, as páginas serão copiadas e mescladas, e a origem será ELIMINADA. Ambas as operações são irreversíveis.{ "\u000A" }Execute novamente com --confirm para prosseguir:{ "\u000A" }{ "\u000A" }  { $value3 }
cli-move-project-ai-memory-warning-warning = ai-memory: aviso: { $value0 }
cli-move-project-ai-memory-project-moved-but-the-client-local-checkout-link-could-not-be-upda = ai-memory: projeto movido, mas não foi possível atualizar o vínculo local do checkout ({ $value0 })
cli-move-project-moved-pages-pages-re-stamped-true-move-sessions-observations-and-history-pre = Movido { $value0 }/{ $value1 } → { $value2 }/{ $value3 }: { $value4 } páginas reatribuídas (movimentação — sessões, observações e histórico preservados).
cli-move-project-moved-pages-pages-copied-merged-into-existing-project-tail = Movido { $value0 }/{ $value1 } → { $value2 }/{ $value3 }: { $value4 } páginas copiadas (mescladas ao projeto existente){ $value5 }.
cli-move-project-warning-skipped-count-page-s-could-not-be-read-from-the-source-and-were-skip = Aviso: { $value0 } página(s) da origem não puderam ser lidas e foram ignoradas; a origem NÃO foi eliminada. Corrija e execute novamente.
cli-move-project-path-conflict-s-source-page-kept-under-a-de-duplicated-path-both-versions-pr = { $value0 } conflito(s) de caminho — página de origem mantida em caminho sem duplicação (ambas as versões preservadas):
cli-move-session-dry-run-nothing-was-written-re-run-with-confirm-to-apply = { "\u000A" }(simulação; nada foi gravado){ "\u000A" }Execute novamente com --confirm para aplicar:{ "\u000A" }{ "\u000A" }  { $value0 }
cli-pending-writes-no-pending-writes = (nenhuma gravação pendente)
cli-pending-writes-body = { "\u000A" }{ $value0 }{ "\u000A" }{ "\u000A" }--- conteúdo ---{ "\u000A" }{ $value1 }
cli-pending-writes-approved = ✓ aprovado { $value0 }
cli-pending-writes-rejected = ✓ rejeitado { $value0 }
cli-profile-sets-profile-consume-false-so-this-project-does-not-take-the-profile = { $value0 } define `[profile] consume = false`, então este projeto não recebe o perfil
cli-profile-workspace-project-sets-profile-consume-false-so-it-does-not-take-the-profile = { $value0 }/{ $value1 } define `[profile] consume = false`, então não recebe o perfil
cli-profile-no-profile-entries-apply-to-workspace-project-nothing-written = Nenhuma entrada de perfil se aplica a { $value0 }/{ $value1 }; nada gravado.
cli-profile-would-write-to-block = Gravaria em { $value0 }:{ "\u000A" }{ "\u000A" }{ $value1 }
cli-profile-entr = { $value0 }: { $value1 } ({ $value2 } { $value3 ->
        [one] entrada
       *[other] entradas
    }{ $value4 })
cli-project-init-ai-memory-project-init-needs-an-interactive-terminal = `ai-memory project init` precisa de um terminal interativo
cli-project-registry-ai-memory-could-not-read-the-checkout-binding-error-using-guarded-project-na = ai-memory: não foi possível ler o vínculo do checkout ({ $value0 }); usando resolução protegida do nome do projeto
cli-project-registry-checkout-identity-changed-while-selecting-the-project-refusing-to-remember-a = a identidade do checkout mudou durante a seleção do projeto; recusando salvar uma seleção desatualizada
cli-project-registry-checkout-is-not-a-directory = o checkout não é um diretório: { $value0 }
cli-project-registry-client-project-registry-exceeds-max-links-links = o registro de projetos do cliente excede { $value0 } vínculos
cli-project-registry-client-project-registry-exceeds-the-byte-limit-keeping-the-previous-registry = o registro de projetos do cliente excede o limite de bytes; mantendo o registro anterior
cli-purge-project-purge-project-is-destructive-and-irreversible-re-run-with-confirm-to-proceed = purge-project é destrutivo e irreversível.{ "\u000A" }Execute novamente com --confirm para prosseguir:{ "\u000A" }{ "\u000A" }  { $value0 }
cli-purge-project-the-following-workstream-segment-directories-are-now-orphaned-under-data-dir = Os seguintes diretórios de segmentos de workstreams ficaram órfãos em <data_dir>/raw/workstreams/ e podem ser removidos:
cli-purge-project-warning-wiki-file-s-could-not-be-removed-from-disk-db-rows-are-gone = Aviso: { $value0 } arquivo(s) da wiki não puderam ser removidos do disco (os registros do banco foram removidos).
cli-purge-project-database-compacted-freed-bytes-reclaimed = Banco de dados compactado: bytes liberados recuperados.
cli-purge-project-note-this-was-a-logical-delete-the-project-is-unreachable-through-the-api-an = Nota: esta foi uma exclusão lógica. O projeto está inacessível pela API e pela busca, mas seus bytes permanecem no arquivo do banco até sua reescrita. Execute novamente com --compact para recuperá-los (operação lenta) e veja docs/lifecycle-ops.md para entender suas garantias e limitações.
cli-purge-session-session-id-session-id-is-not-a-uuid-pass-the-full-sessions-id-value-a-prefix = `--session-id { $value0 }` não é um UUID. Informe o valor completo de `sessions.id`; um prefixo ou ID de workstream não corresponderá.
cli-purge-session-purge-session-is-destructive-and-irreversible-re-run-with-confirm-to-proceed = purge-session é destrutivo e irreversível.{ "\u000A" }Execute novamente com --confirm para prosseguir:{ "\u000A" }{ "\u000A" }  ai-memory purge-session --workspace { $value0 } --project { $value1 } --session-id { $value2 } --confirm
cli-purge-session-wiki-pages-removed = Páginas da wiki removidas:
cli-purge-session-warning-wiki-page-file-s-could-not-be-removed-from-disk-db-rows-are-gone = Aviso: { $value0 } arquivo(s) de páginas da wiki não puderam ser removidos do disco (os registros do banco foram removidos).
cli-purge-session-note-this-was-a-logical-delete-the-session-is-unreachable-through-the-api-an = Nota: esta foi uma exclusão lógica. A sessão está inacessível pela API e pela busca, mas seus bytes permanecem no arquivo do banco até sua reescrita. Execute novamente com --compact para recuperá-los (operação lenta) e veja `docs/` para entender suas garantias e limitações.
cli-read-page-path-was-passed-empty-specify-the-wiki-page-path-to-read = --path foi informado vazio; especifique o caminho da página da wiki a ler
cli-read-page-search-query-was-passed-empty-specify-a-query-or-path-wiki-path = a consulta foi informada vazia; especifique uma consulta ou --path <wiki-path>
cli-read-page-provide-a-query-positional-or-path-wiki-path = informe uma consulta (posicional) ou --path <wiki-path>
cli-reclaim-ledger-versions-no-superseded-ledger-versions-to-reclaim-nothing-written-by-the-pre-2-1-1-in = Nenhuma versão substituída do registro para recuperar. Nada gravado pelo indexador anterior à versão 2.1.1 permanece neste armazenamento.
cli-reclaim-ledger-versions-would-delete-rows-superseded-version-s-of-the-hook-event-ledger-across-paths = Excluiria { $value0 } versão(ões) substituída(s) do registro de eventos dos hooks em { $value1 } caminho(s), contendo { $value2 } de conteúdo de páginas.
cli-reclaim-ledger-versions-drop-latest-each-ledger-s-live-row-would-go-too = { "  " }--drop-latest: o registro ativo de cada registro de eventos também seria removido.
cli-reclaim-ledger-versions-nothing-was-changed-to-apply = Nada foi alterado. Para aplicar:
cli-reclaim-ledger-versions-deleted-rows-superseded-ledger-version-s-across-paths-ledger-path-s-carrying = { $value0 } versão(ões) substituída(s) do registro excluída(s) em { $value1 } caminho(s), contendo { $value2 } de conteúdo de páginas.
cli-reclaim-ledger-versions-each-ledger-s-live-row-was-dropped-as-well-drop-latest = { "  " }O registro ativo de cada registro de eventos também foi removido (--drop-latest).
cli-reclaim-ledger-versions-reclaimed = Recuperados { $value0 } ({ $value1 } → { $value2 }).
cli-reclaim-ledger-versions-the-bytes-are-free-pages-now-run-ai-memory-compact-confirm-to-return-them-to = Os bytes agora são páginas livres. Execute `ai-memory compact --confirm` para devolvê-los ao sistema de arquivos.
cli-recovery-ai-memory-recovery-count-hook-event-s-were-dropped-recovery-remains-incomple = ai-memory: recuperação: { $value0 } evento(s) de hooks foram descartados; a recuperação continua incompleta até serem substituídos por uma transcrição com correlação exata
cli-reindex-refusing-to-reindex-into-a-non-empty-sqlite-store-counts-ai-memory-reindex-r = recusando reindexar em um armazenamento SQLite não vazio ({ $value0 }). `ai-memory reindex` reconstrói um banco derivado limpo a partir da wiki; não repara um índice sujo no local. Pare o servidor, faça um backup, mova ou remova { $value1 } e execute reindex novamente. Dados exclusivos do banco (sessões, observações, transferências, usuários, auditoria, embeddings e contadores de decaimento) não são reconstruídos do Markdown.
cli-reindex-reindexed-pages-across-project-s-in-workspace-s-from = { $value0 } páginas reindexadas em { $value1 } projeto(s) de { $value2 } espaço(s) de trabalho de { $value3 }
cli-reindex-skipped-page-s-whose-path-differs-from-an-indexed-page-only-by-case-or-unico = { $value0 } página(s) ignoradas porque seu caminho difere de uma página indexada apenas por maiúsculas/minúsculas ou normalização Unicode (não podem coexistir no macOS/Windows); o log identifica cada par
cli-rename-project-ai-memory-project-renamed-but-the-client-local-checkout-link-could-not-be-up = ai-memory: projeto renomeado, mas não foi possível atualizar o vínculo local do checkout ({ $value0 })
cli-rename-project-renamed-pages-now-under-the-new-name = Renomeado { $value0 }/{ $value1 } → { $value2 }/{ $value3 } ({ $value4 } páginas com o novo nome).
cli-rename-workstream-to-was-passed-empty-specify-the-new-workstream-name = --to foi informado vazio; especifique o novo nome do workstream
cli-rename-workstream-pass-from-name-or-workstream-id-id-to-choose-the-workstream-to-rename = informe --from NAME ou --workstream-id ID para escolher o workstream a renomear
cli-reorg-all-sessions-are-already-in-the-correct-per-cwd-project-nothing-to-do = Todas as sessões já estão no projeto correto por cwd; nada a fazer.
cli-reorg-session-s-would-be-moved = { $value0 } sessão(ões) seriam movidas:{ "\u000A" }
cli-reorg-table-header = { $value0 } { $value1 } cwd
cli-reorg-dry-run-omit-dry-run-to-apply = { "\u000A" }(simulação; omita --dry-run para aplicar)
cli-reorg-reorg-complete = Reorganização concluída:
cli-reorg-sessions-moved = { "  " }sessões movidas:             { $value0 }
cli-reorg-observations-updated = { "  " }observações atualizadas:     { $value0 }
cli-reorg-pages-graveyarded = { "  " }páginas enviadas ao arquivo: { $value0 }
cli-reorg-distinct-new-projects = { "  " }novos projetos distintos:    { $value0 }
cli-repair-backfill-timestamps-ai-memory-repair-backfill-timestamps-for-workspace-project-no-local-transcri = ai-memory: repair-backfill-timestamps para { $value0 }/{ $value1 }: nenhuma transcrição local continha um horário legível ({ $value2 } sessão(ões) verificadas localmente, nenhuma capturada); nada para enviar.
cli-repair-backfill-timestamps-ai-memory-repair-backfill-timestamps-for-workspace-project-total-repaired-se = ai-memory: repair-backfill-timestamps para { $value0 }/{ $value1 }: { $value2 } sessão(ões) { $value3 }, { $value4 } ignoradas, { $value5 } transcrição(ões) locais sem horário legível.
cli-repair-backfill-timestamps-sent-session-s-across-request-s-of-up-to-max-sessions-per-request-each-one-t = { "  " }{ $value0 } sessão(ões) enviadas em { $value1 } solicitação(ões) de até { $value2 } cada (uma transação por solicitação).
cli-repair-backfill-timestamps-skipped-reason-count = { "  " }ignoradas ({ $value0 }): { $value1 }
cli-repair-backfill-timestamps-dry-run-nothing-was-written-re-run-with-confirm-to-apply-recorded-in-audit-l = { "  " }simulação: nada foi gravado. Execute novamente com --confirm para aplicar (a aplicação é registrada em audit_log, que fornece a base para reversibilidade).
cli-reset-would-remove = removeria { $value0 }
cli-reset-dry-run-pass-confirm-to-wipe = (simulação; informe --confirm para apagar)
cli-restore-source-tarball-not-found = arquivo de origem { $value0 } não encontrado
cli-restore-restored = restaurado { $value0 } -> { $value1 }
cli-restore-restored-the-backup-s-config-toml = config.toml do backup restaurado
cli-restore-kept-the-live-config-toml-the-backup-s-copy-is-at-a-config-can-run-commands = config.toml atual mantido; a cópia do backup está em { $value0 }. Uma configuração pode executar comandos e redirecionar dados: revise antes de adotá-la ou execute novamente com --with-config para restaurá-la no local.
cli-restore-page-path-was-passed-empty-specify-the-wiki-page-path-to-restore = --path foi informado vazio; especifique o caminho da página da wiki a restaurar
cli-restore-page-restored-under-from-page-id = { $value0 } restaurada em { $value1 }/{ $value2 } a partir de { $value3 } (page_id={ $value4 })
cli-restore-page-pre-restore-checkpoint-pre = checkpoint anterior à restauração: { $value0 }
cli-restore-page-restore-checkpoint-cp = checkpoint da restauração: { $value0 }
cli-resume-ai-memory-resume-needs-a-terminal-use-ai-memory-workstreams-to-list-a-checko = `ai-memory resume` precisa de um terminal; use `ai-memory workstreams` para listar um checkout em scripts
cli-resume-resuming-workstream-with-harness-in-at = retomando o workstream '{ $value0 }' com o cliente '{ $value1 }' em { $value2 } às { $value3 }
cli-resume-current-checkout-belongs-to-not-workspace = o checkout atual pertence a { $value0 }, não ao espaço de trabalho '{ $value1 }'
cli-resume-no-managed-workstreams-for-the-current-checkout-launch-one-here-with-ai-memo = nenhum workstream gerenciado para o checkout atual ({ $value0 }); inicie um aqui com `ai-memory run <harness>` primeiro
cli-resume-skipping-current-checkout = ignorando o checkout atual: { $value0 }
cli-resume-skipping-could-not-list-managed-workstreams = ignorando { $value0 }: não foi possível listar workstreams gerenciados ({ $value1 })
cli-resume-no-managed-workstreams-are-available-detail-launch-one-with-ai-memory-run-ha = nenhum workstream gerenciado disponível ({ $value0 }); inicie um com `ai-memory run <harness>` primeiro
cli-service-host-service-host-is-the-windows-logon-entry-s-launcher-on-this-platform-ai-memor = `service-host` inicia a entrada de logon do Windows; nesta plataforma, `ai-memory setup` instala uma unidade de usuário systemd ou um LaunchAgent
cli-setup-init = { "\u000A" }== inicialização
cli-setup-config = configuração: { $value0 } { $value1 }
cli-setup-service = { "\u000A" }== serviço
cli-setup-service-reason = serviço: { $value0 }
cli-setup-service-2 = serviço: { $value0 } { $value1 }
cli-setup-self-check = { "\u000A" }== autoverificação
cli-setup-auth-none-loopback-run-ai-memory-auth-enable-to-require-a-bearer-token = autenticação: nenhuma (loopback); execute `ai-memory auth enable` para exigir um token bearer
cli-setup-auth-bearer-token-owner-only-file-never-printed = autenticação: token bearer (arquivo exclusivo do proprietário; nunca exibido)
cli-setup-problem-problem = problema: { $value0 }
cli-setup-setup-finished-with-problem-s-fix-them-and-re-run-ai-memory-setup = configuração concluída com { $value0 } problema(s); corrija e execute `ai-memory setup` novamente
cli-setup-plugin-is-enabled-leaving-the-settings-file-hooks-mcp-entry-and-skills-alone = plugin: { $value0 } está habilitado; mantendo os hooks do arquivo de configurações, a entrada MCP e as skills (execute `ai-memory install-hooks --agent { $value1 } --apply` para adicioná-los mesmo assim)
cli-setup-mcp-keeping-the-existing-session-aware-entry = mcp: mantendo a entrada existente com suporte a sessões
cli-setup-healthz-ok-healthz = healthz: ok ({ $value0 })
cli-setup-memory-status-ok = memory_status: ok
cli-setup-memory-status-reported-an-error = memory_status informou um erro
cli-setup-agent-setup-agent-agent-opencode-requires-opencode-dialect-v1-v2-because-a-contain = setup-agent --agent opencode exige --opencode-dialect v1|v2 porque um contêiner não pode inspecionar o executável do host
cli-setup-agent-opencode-dialect-cannot-be-combined-with-the-force-v2-opencode2-compatibilit = --opencode-dialect não pode ser combinado com o alias de compatibilidade opencode2, que força V2
cli-setup-agent-opencode-dialect-is-valid-only-with-agent-opencode-not = --opencode-dialect é válido somente com --agent opencode, não { $value0 }
cli-setup-agent-cline-discovers-generated-hook-files-in-the-host-user-s-hook-directory-run-a = O Cline descobre arquivos de hooks gerados no diretório de hooks do usuário do host; execute `ai-memory install-hooks --agent cline --apply` nesse host
cli-setup-agent-internal-generated-integration-should-have-returned-before-staging-hooks = interno: a integração gerada deveria ter retornado antes de preparar os hooks
cli-setup-agent-ai-memory-setup-agent-emits-shell-powershell-hook-bundles-for-agent-sub-thes = [ai-memory] setup-agent emite pacotes de hooks shell/PowerShell para { $value0 }; esses pacotes remotos/de compatibilidade não impõem a capacidade de política de captura v1. Instale hooks nativos locais quando for necessário impor a política.
cli-setup-agent-extracted-copied-hook-script-s-from-to = ✓ { $value0 } script(s) de hooks extraído(s) de { $value1 } para { $value2 }
cli-setup-agent-internal-generated-integration-should-have-returned-before-emitting-staged-h = interno: a integração gerada deveria ter retornado antes de emitir os hooks preparados
cli-setup-agent-label-uses-a-typescript-extension-plugin-not-extracted-shell-scripts = # { $value0 } usa uma extensão/plugin TypeScript, não scripts shell extraídos.
cli-setup-agent-capture-policy-capability-v1-requires-a-freshly-generated-integration-re-run = # A capacidade de política de captura v1 exige uma integração recém-gerada; execute --apply novamente após atualizações.
cli-setup-agent-legacy-shell-powershell-and-remote-only-paths-are-unsupported-compatibility = # Os caminhos shell/PowerShell legados e exclusivamente remotos são modos de compatibilidade sem suporte.
cli-setup-agent-install-it-directly-instead = # Instale diretamente:
cli-setup-agent-server-url = { "  " }--server-url { $value0 } \
cli-setup-agent-auth-token-token = { "  " }--auth-token <token>
cli-setup-agent-server-url-2 = { "  " }--server-url { $value0 }
cli-setup-agent-add-auth-token-token-if-the-server-requires-bearer-auth = { "  " }# adicione --auth-token <token> se o servidor exigir autenticação bearer
cli-setup-agent-mcp-tools-come-through-the-same-generated-pi-bridge-extension-no-native-mcp = As ferramentas MCP passam pela mesma extensão de ponte Pi gerada; nenhum mcp.json nativo é gravado.
cli-setup-agent-also-run-ai-memory-install-mcp-client-mcp-client-to-wire-mcp-separately = Execute também `ai-memory install-mcp --client { $value0 }` para configurar MCP separadamente.
cli-show-json-only-lists-launch-options-do-not-combine-it-with-launch-arguments = --json apenas lista opções de inicialização; não combine com argumentos de execução
cli-show-ai-memory-show-needs-a-terminal-use-ai-memory-show-json-to-list-choices-in-s = `ai-memory show` precisa de um terminal; use `ai-memory show --json` para listar opções em scripts
cli-show-no-supported-managed-harness-was-found-in-path-looked-for = nenhum cliente gerenciado compatível encontrado no PATH (procurados: { $value0 })
cli-show-ai-memory-server-project-listing-unavailable-using-client-local-links-and-sc = ai-memory: listagem de projetos do servidor indisponível ({ $value0 }); usando vínculos locais do cliente e resultados da varredura
cli-show-ai-memory-client-project-registry-unavailable-using-scan-results = ai-memory: registro de projetos do cliente indisponível ({ $value0 }); usando resultados da varredura
cli-show-ai-memory-ignored-stale-client-project-link = ai-memory: vínculo de projeto desatualizado ignorado { $value0 }/{ $value1 } ({ $value2 })
cli-show-ai-memory-ignored-additional-stale-client-project-links = ai-memory: { $value0 } vínculos adicionais de projetos desatualizados ignorados
cli-show-checkout-scope-changed-after-it-was-listed-expected-found-rerun-ai-memory-sh = o escopo do checkout mudou desde a listagem: esperado { $value0 }/{ $value1 }, encontrado { $value2 }/{ $value3 }; execute `ai-memory show` novamente
cli-uninstall-would-run-program = executaria `{ $value0 } { $value1 }`
cli-uninstall-note-run-ai-memory-uninstall-only-hooks-apply-inside-every-other-checkout-th = nota: execute `ai-memory uninstall --only hooks --apply` em cada outro checkout que usou `install-hooks --scope project`; seu .claude/settings.local.json continua chamando este binário e diretório de dados.
cli-uninstall-would-purge = eliminaria { $value0 }
cli-uninstall-dry-run-pass-apply-to-remove = (simulação; informe --apply para remover)
cli-uninstall-proceed-with-removal-y-n = Prosseguir com a remoção? [s/N]{ " " }
cli-uninstall-aborted = cancelado.
cli-uninstall-purged = ✓ eliminado { $value0 }
cli-upgrade-upgraded-to-tag = ✓ atualizado para { $value0 }
cli-upgrade-no-staged-hook-scripts-found-at = → nenhum script de hook preparado encontrado em { $value0 }
cli-upgrade-nothing-to-refresh-install-hooks-hasn-t-been-run-with-apply-yet = { "  " }(nada para atualizar — install-hooks ainda não foi executado com --apply)
cli-upgrade-no-staged-agent-dirs-found-under = → nenhum diretório de agente preparado encontrado em { $value0 }
cli-upgrade-skipping-generated-name-integration-err = { "  " }(ignorando a integração gerada de { $value0 } — { $value1 })
cli-upgrade-refreshing-generated-name-integration = → atualizando a integração gerada de { $value0 }
cli-upgrade-skipped-err-re-run-ai-memory-install-hooks-agent-name-apply = { "      " }(ignorado — { $value0 }; execute `ai-memory install-hooks --agent { $value1 } --apply` novamente)
cli-upgrade-updating-the-ai-memory-claude-code-plugin = → atualizando o plugin ai-memory do Claude Code
cli-upgrade-the-ai-memory-codex-plugin-is-installed-run-codex-plugin-marketplace-upgrade = → o plugin ai-memory do Codex está instalado: execute `codex plugin marketplace upgrade ai-memory` e `codex plugin add { $value0 }` para atualizá-lo
cli-upgrade-refreshing-staged-hook-scripts-for = → atualizando scripts de hooks preparados para: { $value0 }
cli-upgrade-no-ai-memory-hooks-in-skipping-the-user-level-re-apply-the-staged-scripts-ar = { "    " }(nenhum hook ai-memory em { $value0 } — ignorando a reaplicação no nível do usuário; os scripts preparados são atualizados, mas arquivos `--scope project` não são reescritos: execute `ai-memory install-hooks --agent { $value1 } --scope project --apply` em cada checkout para receber as alterações nos comandos dos hooks)
cli-upgrade-restaged = { "    " }preparado novamente { $value0 }
cli-upgrade-skipped-err-re-run-ai-memory-install-hooks-agent-name-scope-project-apply-in = { "      " }(ignorado — { $value0 }; execute `ai-memory install-hooks --agent { $value1 } --scope project --apply` novamente em cada checkout)
cli-upgrade-skipped-err-re-run-with-the-same-server-url-auth-token-used-originally = { "      " }(ignorado — { $value0 }; execute novamente com os mesmos --server-url / --auth-token usados originalmente)
cli-upgrade-note-ai-memory-server-url-looks-remote-this-upgrade-refreshed-only-the-local = nota: AI_MEMORY_SERVER_URL={ $value0 } parece remoto — esta atualização modificou somente o binário e os hooks locais; implante o servidor remoto separadamente
cli-workstream-search-no-matching-managed-workstream-events = Nenhum evento de workstream gerenciado correspondente.
cli-workstream-search-event = ## Evento { $value0 } | { $value1 } | { $value2 }
cli-workstream-search-source-record = Registro de origem: { $value0 }
cli-workstream-search-provenance = Proveniência: { $value0 }
cli-workstreams-warning-the-ai-memory-server-ignored-workstream-pagination-showing-its-first = aviso: o servidor ai-memory ignorou a paginação dos workstreams; exibindo os primeiros { $value0 }. Atualize o servidor para listar todos.
cli-write-page-path-was-passed-empty-specify-the-wiki-page-path-to-write = --path foi informado vazio; especifique o caminho da página da wiki a gravar
cli-write-page-wrote-page-id-under = ✓ { $value0 } gravada (page_id={ $value1 }) em { $value2 }/{ $value3 }
cli-install-hooks-scope-project-is-only-supported-for-agent-claude-code-ai-memory-writes-no-pr = `--scope project` só tem suporte com `--agent claude-code`; ai-memory não grava arquivos locais de hooks do projeto para { $value0 }
cli-install-hooks-ai-memory-warning-could-not-persist-the-hook-auth-token-under-e-the-token-co = [ai-memory] aviso: não foi possível persistir o token de autenticação dos hooks em { $value0 } ({ $value1 }). O token vem de [auth].bearer_token_file e nunca é embutido na configuração de um agente; estes hooks não serão autenticados até que o diretório de dados permita gravação e `install-hooks --apply` seja executado novamente.
cli-install-hooks-ai-memory-warning-could-not-persist-the-hook-auth-token-under-e-embedding-it = [ai-memory] aviso: não foi possível persistir o token de autenticação dos hooks em { $value0 } ({ $value1 }); embutindo na configuração de hooks renderizada para manter a captura funcionando. O token fica legível nesse arquivo / nos argumentos do processo. Para evitar isso (recomendado), instale com um diretório de dados do host que permita gravação — por exemplo, `AI_MEMORY_DATA_DIR=$HOME/.local/share/ai-memory` para o wrapper Docker — ou um binário nativo `ai-memory`.
cli-install-hooks-ai-memory-hooks-installing-for-user-user = [ai-memory] instalando hooks para o usuário: { $value0 }
cli-install-hooks-ai-memory-capture-policy-capability-v1-enforced-by-this-selected-integration = [ai-memory] capacidade de política de captura v1 imposta pela integração selecionada; execute --apply novamente para atualizar instalações existentes.
cli-install-hooks-ai-memory-selected-shell-powershell-compatibility-path-does-not-enforce-capt = [ai-memory] o caminho de compatibilidade shell/PowerShell selecionado não impõe a política de captura v1; use uma seleção de plataforma nativa ou integração gerada.
cli-install-hooks-capture-assistant-requires-agent-claude-code-codex-or-opencode2-on-a-native = --capture-assistant exige --agent claude-code, codex ou opencode2 em uma plataforma de hooks nativa (PosixNative/WindowsNative). A seleção atual usa scripts alternativos ou outro agente, onde a opção não pode ter efeito. Remova --capture-assistant ou mude para uma instalação nativa compatível.
cli-install-hooks-no-capture-prompts-and-capture-prompts-require-agent-claude-code-other-agent = --no-capture-prompts e --capture-prompts exigem --agent claude-code. Outros agentes podem usar o hook de prompt para entregar contexto de transferência; removê-lo pode interromper a continuidade entre agentes.
cli-install-hooks-capture-mode-capture-mode = modo de captura: { $value0 }
cli-install-hooks-repositories-without-a-ai-memory-toml-marker-emit-no-lifecycle-events = { "  " }repositórios sem marcador .ai-memory.toml não emitem eventos de ciclo de vida
cli-install-hooks-warning-this-install-uses-script-hooks-which-post-directly-and-cannot-enforc = { "  " }AVISO: esta instalação usa hooks em scripts, que enviam POST diretamente e não podem impor o modo. A lista de permissões está armazenada, mas NÃO está em vigor aqui.
cli-api-key-creating-api-credential = criando credencial de API
cli-api-key-listing-api-credentials = listando credenciais de API
cli-api-key-no-api-credentials = (nenhuma credencial de API)
cli-api-key-rotating-api-credential = renovando credencial de API
cli-api-key-revoking-api-credential = revogando credencial de API
cli-api-key-revoked-api-key = ✓ chave de API '{ $value0 }' revogada
cli-api-key-verb-api-key-store-this-token-now-it-will-not-be-shown-again = ✓ chave de API '{ $value1 }' ({ $value2 }): { $value0 }{ "\u000A" }{ "\u000A" }Salve este token agora — ele NÃO será exibido novamente.
cli-auth-issuer-is-required-for-auth-login-oidc-device = --issuer é obrigatório para `auth login oidc-device`
cli-auth-client-id-is-required-for-auth-login-oidc-device = --client-id é obrigatório para `auth login oidc-device`
cli-auth-enable-auth-already-required-by-ai-memory-auth-token-or-auth-bearer-token-nothing-c = autenticação: já exigida por AI_MEMORY_AUTH_TOKEN ou [auth].bearer_token; nada alterado.
cli-auth-enable-auth-bearer-token-at-owner-only = autenticação: token bearer { $value0 } em { $value1 } (acesso exclusivo do proprietário)
cli-auth-enable-hooks-given-the-token-through-the-hook-credential-files = hooks: token fornecido pelos arquivos de credenciais dos hooks
cli-auth-enable-hooks-already-use-the-token = hooks: já usam o token
cli-auth-enable-hooks-another-credential-is-stored-for-them-and-was-kept-re-run-ai-memory-in = hooks: outra credencial está armazenada e foi mantida; execute `ai-memory install-hooks --agent <agent> --apply` novamente para usar este token
cli-auth-enable-mcp-client-re-registered-in-so-it-reads-the-token-without-holding-it = mcp { $value0 }: registrado novamente em { $value1 } para ler o token sem armazená-lo
cli-auth-enable-mcp-client-targets-url-another-server-left-unchanged = mcp { $value0 }: aponta para { $value1 }, outro servidor; mantido sem alterações
cli-auth-enable-mcp-client-needs-a-manual-change-reason = mcp { $value0 }: precisa de alteração manual: { $value1 }
cli-auth-enable-restart-the-ai-memory-server-for-it-to-require-the-token = Reinicie o servidor ai-memory para que ele passe a exigir o token.
cli-auto-improve-staged-auto-improve-run-for-manual-approval = Execução de melhoria automática { $value0 } preparada para aprovação manual
cli-auto-improve-auto-approved-auto-improve-run = Execução de melhoria automática { $value0 } aprovada automaticamente
cli-auto-improve-approval-policy = Política de aprovação: { $value0 }
cli-auto-improve-page = { "  " }- { $value0 } [{ $value1 }] página { $value2 } ({ $value3 })
cli-auto-improve-report-auto-improve-telemetry-for-project = { "\u000A" }Telemetria de melhoria automática de { $value0 }{ "\u000A" }
cli-auto-improve-report-summary = Resumo: { $value0 }
cli-auto-improve-report-terminal-denominator = Denominador de estados finais: { $value0 }
cli-auto-improve-report-rates-approved-1-rejected-1-conflict-1-failed-1 = Taxas: aprovadas { $value0 }%, rejeitadas { $value1 }%, conflitos { $value2 }%, falhas { $value3 }%
cli-auto-improve-report-runs-with-learning-proposals = Execuções: { $value0 } ({ $value1 } com propostas de aprendizado)
cli-auto-improve-report-top-targets = Principais destinos:
cli-auto-improve-report-findings = Constatações: { $value0 }
cli-auto-improve-report-more = { "  " }... mais { $value0 }
cli-backfill-locating-the-local-harness-session-stores = localizando os armazenamentos locais de sessões dos clientes
cli-backfill-ai-memory-session-skipped-skipped-oversized-transcript-record-s-the-rest-was = ai-memory: { $value0 }, sessão { $value1 }: { $value2 } registro(s) de transcrição acima do tamanho limite ignorado(s); o restante foi importado
cli-backfill-ai-memory-capture-mode-is-allowlist-and-this-checkout-has-no-ai-memory-toml = ai-memory: o modo de captura é allowlist e este checkout não tem um marcador .ai-memory.toml que habilite a captura (ou o marcador está ilegível) — nada para importar em { $value0 }/{ $value1 }.
cli-backfill-ai-memory-already-has-captured-sessions-nothing-to-backfill-use-force-to-imp = ai-memory: { $value0 }/{ $value1 } já contém sessões capturadas — nada para importar (use --force para importar mesmo assim).
cli-backfill-ai-memory-would-import-local-session-s-into-dry-run = ai-memory: importaria { $value0 } sessão(ões) local(is) para { $value1 }/{ $value2 } (simulação).
cli-backfill-ai-memory-no-local-session-history-found-to-backfill-for = ai-memory: nenhum histórico local de sessões encontrado para importar em { $value0 }/{ $value1 }.
cli-backup-requesting-backup-from-server = solicitando backup ao servidor
cli-backup-agents-locating-user-home-directory = localizando o diretório pessoal do usuário
cli-backup-agents-locating-current-working-directory = localizando o diretório de trabalho atual
cli-backup-agents-no-ai-agent-assets-detected-for-backup = Nenhum recurso de agente de IA detectado para backup.
cli-backup-agents-reopening-temporary-agent-archive = reabrindo o arquivo temporário de agentes
cli-backup-agents-syncing-temporary-agent-archive = sincronizando o arquivo temporário de agentes
cli-backup-agents-backed-up-agent-assets-to = ✓ Backup de { $value0 } recursos de agentes gravado em { $value1 } ({ $value2 })
cli-backup-agents-warning-secrets-were-included-without-sanitization-archive-mode-is-0600-on-u = ⚠️  Aviso: segredos foram incluídos sem sanitização. O modo do arquivo é 0600 no Unix; proteja o arquivo nas demais plataformas.
cli-backup-agents-found-assets-across-ai-agents-dry-run = { $value0 } recursos encontrados entre os agentes de IA (simulação):
cli-bootstrap-getting-cwd-for-bootstrap-no-git-repo-falling-back-to = obtendo CWD para inicialização (sem repositório git, usando . como alternativa)
cli-bootstrap-kind-complete-for-workspace-project = { "\u000A" }{ $value0 } concluído para { $value1 }/{ $value2 }{ "\u000A" }
cli-bootstrap-sources-loaded-into-the-llm = Fontes carregadas no LLM:
cli-bootstrap-git-commit-summar = { "  " }- { $value0 } { $value1 ->
        [one] resumo de commit git
       *[other] resumos de commits git
    }
cli-bootstrap-readme = { "  " }- README
cli-bootstrap-doc-file-under-docs = { "  " }- { $value0 } { $value1 ->
        [one] arquivo de documentação
       *[other] arquivos de documentação
    } (em docs/)
cli-bootstrap-rust-module-header = { "  " }- { $value0 } { $value1 ->
        [one] cabeçalho de módulo Rust
       *[other] cabeçalhos de módulos Rust
    }
cli-bootstrap-project-rules-file-claude-md-agents-md = { "  " }- { $value0 } { $value1 ->
        [one] arquivo de regras do projeto
       *[other] arquivos de regras do projeto
    } (CLAUDE.md / AGENTS.md / ...)
cli-bootstrap-input-tokens-estimated = { "  " }-> ~{ $value0 } tokens de entrada estimados{ $value1 }
cli-bootstrap-sequential-llm-chunk = { "  " }-> { $value0 } { $value1 ->
        [one] bloco sequencial do LLM
       *[other] blocos sequenciais do LLM
    }
cli-bootstrap-dry-run-no-llm-call-no-pages-written = { "\u000A" }(simulação -- nenhuma chamada ao LLM, nenhuma página gravada)
cli-bootstrap-generated-wiki-page = { "\u000A" }{ $value0 } { $value1 ->
        [one] página da wiki gerada
       *[other] páginas da wiki geradas
    }:
cli-bootstrap-rationale = { "\u000A" }Justificativa: { $value0 }
cli-bootstrap-what-ai-memory-knows-now-only-the-sources-listed-above-not-every-file-in-you = { "\u000A" }O que o ai-memory sabe agora{ "\u000A" }  Somente as fontes listadas acima. NÃO todos os arquivos do projeto,{ "\u000A" }  NÃO todos os commits desde o início, NÃO o comportamento em execução{ "\u000A" }  nem os logs de testes. Ao usar Claude Code (ou outro agente MCP), os{ "\u000A" }  hooks de ciclo de vida capturam automaticamente seu workstream{ "\u000A" }  real, e a consolidação aprimora a wiki ao longo do tempo.
cli-curator-curator-dry-run-for-project = { "\u000A" }Simulação do curador para { $value0 }{ "\u000A" }
cli-delete-page-deleting-page-via-server = excluindo página pelo servidor
cli-doctor-machine-identity-is-unavailable-showing-capture-coverage-only = Identidade da máquina indisponível; exibindo somente a cobertura de captura.
cli-doctor-muse-hooks-mcp-entry = { "  " }muse: { $value0 } (hooks { $value1 }, entrada MCP { $value2 }){ "\u000A" }
cli-doctor-has-both-mcpservers-and-mcp-servers-so-muse-ignores-every-mcp-server-move-th = ⚠ { $value0 } contém `mcpServers` e `mcp_servers`, então o Muse ignora todos os servidores MCP. Mova os servidores para um dos dois mapas.{ "\u000A" }
cli-doctor-has-no-schema-version-so-every-muse-command-fails-at-startup-add-schema-vers = ⚠ { $value0 } não contém "schema_version", então todos os comandos muse falham na inicialização. Adicione "schema_version": 1.{ "\u000A" }
cli-doctor-plugin-plugin-id-for-agent = { "  " }plugin: { $value0 } { $value1 } para { $value2 } ({ $value3 }){ "\u000A" }
cli-doctor-the-agent-plugin-is-version-but-this-binary-is-run-ai-memory-upgrade-it-upda = ⚠ O plugin { $value0 } está na versão { $value1 }, mas este binário é { $value2 }. Execute `ai-memory upgrade` (também atualiza o plugin do Claude Code) ou atualize o plugin em { $value3 }.{ "\u000A" }
cli-doctor-agent-also-has-ai-memory-settings-file-hooks-in-consequence-keep-one-remove = ⚠ { $value0 } também tem hooks ai-memory no arquivo de configurações { $value1 }. { $value2 }. Mantenha apenas uma instalação: remova as entradas ai-memory desse arquivo ou execute `{ $value3 }`.{ "\u000A" }
cli-doctor-the-ai-memory-routing-skills-are-installed-in-as-well-as-in-the-plugin-so-ag = ⚠ As skills de roteamento do ai-memory estão instaladas em { $value0 } e no plugin, então { $value1 } carrega ambas as cópias. Mantenha apenas uma: `ai-memory uninstall --only skills --apply` remove as instaladas.{ "\u000A" }
cli-doctor-an-ai-memory-mcp-server-is-registered-in-as-well-as-by-the-plugin-so-agent-l = ⚠ Um servidor MCP ai-memory está registrado em { $value0 } e pelo plugin, então { $value1 } lista as ferramentas de memória duas vezes. Mantenha apenas um.{ "\u000A" }
cli-doctor-setting-is-set-in-your-claude-code-settings-the-plugin-s-hooks-do-not-run-an = ⚠ { $value0 } está definido nas configurações do Claude Code: os hooks do plugin não são executados e as sessões não são capturadas.{ "\u000A" }
cli-doctor-codex-skips-of-the-plugin-s-hooks-untrusted-or-modified-open-codex-run-hooks = ⚠ O Codex ignora { $value0 } hooks do plugin (não confiáveis ou modificados). Abra o Codex, execute /hooks e marque-os como confiáveis; até lá, as sessões do Codex não serão capturadas.{ "\u000A" }
cli-doctor-the-server-at-did-not-answer-hook-events-are-being-spooled-locally = ⚠ O servidor em { $value0 } não respondeu. Os eventos dos hooks estão sendo armazenados localmente no spool.{ "\u000A" }
cli-doctor-the-hook-spool-is-full-events-oldest-newly-captured-events-are-dropped-until = ⚠ O spool de hooks está cheio ({ $value0 } eventos, mais antigo { $value1 }): novos eventos capturados são descartados até que haja espaço.{ "\u000A" }
cli-doctor-hook-spool-event-s-waiting-for-the-server-oldest = { "  " }spool de hooks: { $value0 } evento(s) aguardando o servidor (mais antigo { $value1 }).{ "\u000A" }
cli-doctor-codex-skips-ai-memory-hook-s-codex-runs-a-hook-only-after-you-trust-it-and-a = ⚠ O Codex ignora { $value0 } hook(s) ai-memory: { $value1 }. O Codex executa hooks apenas após a confirmação de confiança, exigida novamente quando o comando muda. Abra o Codex, execute /hooks e marque-os como confiáveis; até lá, as sessões do Codex não serão capturadas.{ "\u000A" }
cli-doctor-project-coordinate-requested-current = { "  " }coordenada do projeto: { $value0 } (solicitada { $value1 }, atual { $value2 })
cli-doctor-collision-reason = { "    " }colisão: { $value0 }
cli-doctor-project-coordinate-unavailable = { "  " }coordenada do projeto: indisponível ({ $value0 })
cli-doctor-local-scope-marker = { "    " }local: escopo { $value0 }, marcador { $value1 }
cli-doctor-repository-source-source-style = { "    " }repositório: origem { $value0 }, estilo { $value1 }
cli-doctor-canonical-candidate-canonical = { "    " }candidato canônico: { $value0 }
cli-doctor-legacy-candidate-legacy = { "    " }candidato legado: { $value0 }
cli-doctor-marker-diagnostic-message = { "    " }diagnóstico do marcador: { $value0 }
cli-export-okf-requesting-okf-bundle-from-server = solicitando pacote OKF ao servidor
cli-finalize-session-no-open-sessions-matched = Nenhuma sessão aberta de { $value0 } corresponde a { $value1 }/{ $value2 }
cli-finalize-session-finalized-session-s-for = { $value0 } sessão(ões) de { $value1 } finalizada(s) em { $value2 }/{ $value3 }
cli-grant-no-grants = (nenhuma permissão concedida)
cli-grant-open-projects-admit-every-user-regardless-a-restricted-project-with-no-grant = Projetos abertos admitem todos os usuários; um projeto restrito sem permissões concedidas admite apenas root.
cli-grant-listing-grants = listando permissões concedidas
cli-grant-who-already-holds-on-project-nothing-changed = { $value0 } já tem { $value1 } em { $value2 }; nada alterado.
cli-grant-who-previous-on-project = { $value0 }: { $value1 } -> { $value2 } em { $value3 }.
cli-grant-who-now-holds-on-project = { $value0 } agora tem { $value1 } em { $value2 }.
cli-grant-no-longer-holds-anything-on-project = { $value0 } não tem mais permissões em { $value1 }.
cli-grant-held-nothing-on-project-nothing-changed = { $value0 } não tinha permissões em { $value1 }; nada alterado.
cli-install-instructions-would-write-into = # Gravaria em: { $value0 }{ "\u000A" }
cli-install-instructions-getting-cwd-for-install-instructions-target = obtendo CWD para o destino de install-instructions
cli-install-instructions-note-neither-claude-md-nor-agents-md-exists-in-creating-claude-md-if-you-use = nota: nem CLAUDE.md nem AGENTS.md existem em { $value0 }; criando CLAUDE.md. Se você usa Codex / OpenCode / Cursor / Gemini CLI / Antigravity CLI / Kimi Code / Kiro CLI, execute novamente com `--target AGENTS.md`.
cli-install-mcp-ai-memory-this-install-s-hooks-sign-in-with-oidc-keeping-the-auth-token-key = [ai-memory] os hooks desta instalação usam OIDC; mantendo a chave --auth-token na entrada MCP para que continuem autenticando
cli-install-mcp-ai-memory-stored-the-bridge-s-key-in-the-mcp-entry-carries-no-token = [ai-memory] chave da ponte armazenada em { $value0 }; a entrada MCP não contém token
cli-install-mcp-ai-memory-warning-already-holds-a-different-key-for-this-install-s-hooks-kee = [ai-memory] aviso: { $value0 } já contém outra chave para os hooks desta instalação; mantendo a chave --auth-token na entrada MCP
cli-install-mcp-ai-memory-warning-could-not-store-the-key-under-e-keeping-it-in-the-mcp-entr = [ai-memory] aviso: não foi possível armazenar a chave em { $value0 } ({ $value1 }); mantendo-a na entrada MCP
cli-install-mcp-prime-agent-reads-the-token-from-prime-agent-token-env-export-it-where-you-l = { "  " }O Prime Agent lê o token de ${ $value0 }; exporte a variável onde você inicia prime-agent.
cli-install-mcp-codex-reads-the-token-from-prime-agent-token-env-export-it-where-you-launch = { "  " }O Codex lê o token de ${ $value0 }; exporte a variável onde você inicia codex.
cli-install-mcp-refusing-to-replace-user-owned-opencode-mcp-entry-in-the-selected-major-vers = recusando substituir a entrada MCP OpenCode '{ $value0 }', pertencente ao usuário, no local da versão principal selecionada
cli-install-mcp-refusing-to-replace-a-user-owned-opencode-mcp-entry-in-the-selected-major-ve = recusando substituir a entrada MCP OpenCode '{ $value0 }', pertencente ao usuário, no local da versão principal selecionada ao migrar a entrada alternativa gerada
cli-install-mcp-ai-memory-warning-preserving-ambiguous-opencode-mcp-entry-in-the-alternate-m = [ai-memory] aviso: preservando a entrada MCP OpenCode ambígua '{ $value0 }' no local da versão principal alternativa
cli-install-mcp-mcp-is-present-but-not-an-object = `mcp` está presente, mas não é um objeto
cli-install-mcp-a-user-owned-opencode-mcp-entry-occupies-the-selected-major-version-location = uma entrada MCP OpenCode pertencente ao usuário ocupa o local da versão principal selecionada e uma entrada gerada permanece no local alternativo; ambas foram preservadas. Informe --server-url e --auth-token explicitamente após resolver o conflito
cli-install-mcp-conflicting-generated-opencode-mcp-entries-exist-in-the-v1-and-v2-locations = existem entradas MCP OpenCode geradas conflitantes nos locais V1 e V2; informe --server-url e --auth-token explicitamente antes de instalar
cli-install-mcp-ai-memory-warning-preserving-user-owned-opencode-mcp-entry-in-the-selected-m = [ai-memory] aviso: preservando a entrada MCP OpenCode pertencente ao usuário no local da versão principal selecionada; informe --server-url e --auth-token explicitamente em vez de inferir a partir dela
cli-install-mcp-internal-unsupported-json-mcp-client = interno: cliente MCP JSON sem suporte
cli-install-mcp-mcpservers-is-present-but-not-an-object = `mcpServers` está presente, mas não é um objeto
cli-install-mcp-mcp-servers-is-present-but-not-an-object = `mcp.servers` está presente, mas não é um objeto
cli-install-mcp-servers-is-present-but-not-an-object = `servers` está presente, mas não é um objeto
cli-install-mcp-context-servers-is-present-but-not-an-object = `context_servers` está presente, mas não é um objeto
cli-install-skills-ai-memory-note-skipped-the-grok-first-turn-rule-is-not-grok-home-hooks-ai-me = [ai-memory] nota: regra de primeiro turno do Grok ignorada: { $value0 } não é <grok home>/hooks/ai-memory.json, então não identifica um diretório pessoal do Grok
cli-install-skills-ai-memory-warning-skipped-grok-first-turn-handoff-rule-err = [ai-memory] aviso: regra de transferência de primeiro turno do Grok ignorada: { $value0 }
cli-list-projects-table-header = { $value0 }  { $value1 }  { $value2 }  { $value3 }  ÚLTIMA ATUALIZAÇÃO{ "\u000A" }
cli-llm-test-provider-is-required-unless-server-is-passed = --provider é obrigatório, exceto quando --server é informado
cli-llm-test-model-is-required-unless-server-is-passed = --model é obrigatório, exceto quando --server é informado
cli-llm-test-parsing-ai-memory-llm-headers-llm-headers = analisando AI_MEMORY_LLM_HEADERS / llm_headers
cli-llm-test-building-llm-provider = construindo provedor LLM
cli-llm-test-prompt-is-required-unless-server-is-passed = --prompt é obrigatório, exceto quando --server é informado
cli-llm-test-calling-provider-for-structured-output = chamando provedor para saída estruturada
cli-llm-test-calling-provider = chamando provedor
cli-message-popped-message-for-workspace-project = Mensagem { $value0 } coletada para { $value1 }/{ $value2 }.
cli-message-from-agent = { "  " }de: { $value0 }/{ $value1 } (agente: { $value2 })
cli-message-sent-by = { "  " }enviada por: { $value0 }
cli-message-subject = { "  " }assunto: { $value0 }
cli-message-nature = { "  " }natureza: { $value0 }
cli-message-created = { "  " }criação: { $value0 }
cli-message-evidence = evidência: { $value0 }
cli-mod-getting-cwd-for-project-auto-detect = obtendo CWD para detecção automática do projeto
cli-move-session-verb-session-from-to = { $value0 } sessão { $value1 } de { $value2 } para { $value3 }:
cli-move-session-session-row = { "  " }registro da sessão:          { $value0 }
cli-move-session-warning-warning = Aviso: { $value0 }
cli-move-session-would-create-project-absent-created-on-confirm = { "  " }criaria o projeto { $value0 } (inexistente; criado com --confirm)
cli-move-session-pre-move-checkpoint-oid = Checkpoint anterior à movimentação: { $value0 }
cli-move-session-checkpoint-oid = Checkpoint: { $value0 }
cli-move-session-verb-moved-of-total-session-s-from-to = { $value0 } { $value1 } de { $value2 } sessão(ões) de { $value3 } para { $value4 }:
cli-move-session-table-header = { $value0 } { $value1 } { $value2 } { $value3 } { $value4 } { $value5 } cwd
cli-move-session-note-rehomed-session-s-marked-re-home-already-had-their-session-row-in-the-d = Nota: { $value0 } sessão(ões) marcada(s) para reatribuição já tinham seu registro no destino; somente seus registros dispersos (contadores acima) foram reunidos.
cli-move-session-warning-warned-session-s-have-a-cwd-whose-basename-is-not-the-destination-pr = Aviso: { $value0 } sessão(ões) têm um cwd cujo nome de diretório não é o projeto de destino; novas sessões desses diretórios continuam resolvendo pelo nome do diretório, exceto quando um marcador .ai-memory.toml fixa o projeto.
cli-move-session-gathering-observations-out-of-scopes-into-destination = { "  " }reunindo observações de { $value0 } escopos em { $value1 }:
cli-move-session-observation-s = { "    " }{ $value0 }/{ $value1 }: { $value2 } observação(ões)
cli-move-session-note-this-empties-every-scope-listed-above-not-only-the-one-named-as-the-sou = { "  " }Nota: isso esvazia todos os escopos listados acima, não apenas o indicado como origem. Mover a sessão de volta posteriormente retorna todos os registros a um único projeto — a divisão exibida aqui não é restaurada.
cli-move-session-observations = { "  " }observações:                 { $value0 }
cli-move-session-handoffs = { "  " }transferências:              { $value0 }
cli-move-session-consolidation-jobs = { "  " }tarefas de consolidação:     { $value0 }
cli-move-session-auto-improve-runs = { "  " }execuções de melhoria:       { $value0 }
cli-move-session-auto-improve-claims = { "  " }reservas de melhoria:        { $value0 }
cli-move-session-page-page-version-s-moved-retired = { "  " }página:                      { $value0 } ({ $value1 } versão(ões) movida(s), { $value2 } retirada(s))
cli-profile-profile-on-share-deployment-server = Perfil: habilitado ({ $value0 }; servidor { $value1 })
cli-profile-profile-off-deployment-server = Perfil: desabilitado (servidor { $value0 })
cli-profile-enabled-share = { "  " }enabled = { $value0 }, share = { $value1 }
cli-profile-scope-entr-digest-of-bytes = { "  " }escopo: { $value0 }/{ $value1 } — { $value2 } { $value3 ->
        [one] entrada
       *[other] entradas
    }, resumo de { $value4 } de { $value5 } bytes
cli-profile-scope-one-private-profile-per-user-pass-user-name-to-inspect-one = { "  " }escopo: um perfil privado por usuário; informe --user <name> para examinar um
cli-profile-scope-no-profile-entries-yet = { "  " }escopo: nenhuma entrada de perfil ainda
cli-profile-digest-at-session-start-baseline-for-new-projects-bytes = { "  " }resumo no início da sessão: { $value0 } (base para novos projetos: { $value1 } bytes)
cli-profile-min-projects-apply-max-lines-llm = { "  " }min_projects = { $value0 }, apply_max_lines = { $value1 }, llm = { $value2 }
cli-profile-every-project-contributes = { "  " }todos os projetos contribuem
cli-profile-projects-that-opted-out-profile-contribute-false = { "  " }projetos que não contribuem ([profile] contribute = false):
cli-profile-listing-the-profile = listando o perfil
cli-profile-no-profile-entries-yet = Nenhuma entrada de perfil ainda.
cli-profile-forgot-the-git-history-keeps-it = { $value0 } esquecida (o histórico git a preserva).
cli-profile-was-not-in-the-profile-nothing-changed = { $value0 } não estava no perfil; nada alterado.
cli-profile-reviewing-the-profile = revisando o perfil
cli-profile-the-profile-is-off-on-this-server = O perfil está desabilitado neste servidor.
cli-profile-profile-share-entr = Perfil ({ $value0 }): { $value1 } { $value2 ->
        [one] entrada
       *[other] entradas
    }
cli-profile-next-pass-will-update = A próxima passagem atualizará: { $value0 }
cli-profile-not-in-the-profile-yet-needs-more-projects-or-say-it-as-a-general-rule = Ainda fora do perfil (precisa de mais projetos ou de uma declaração como regra geral):
cli-profile-project-more-needed-people = { "  " }{ $value0 } ({ $value1 } { $value2 ->
        [one] projeto
       *[other] projetos
    }, faltam mais { $value3 }{ $value4 })
cli-profile-edited-by-hand-left-alone = Editada manualmente, mantida sem alterações: { $value0 }
cli-profile-forgotten-stays-out-until-you-say-it-again = Esquecida, permanece fora até ser declarada novamente: { $value0 }
cli-profile-fix-a-wrong-entry-ai-memory-profile-forget-path-reword-it-by-editing-the-pag = Corrija uma entrada errada: `ai-memory profile forget <path>`; reformule editando a página (assim ela permanece sob seu controle).
cli-profile-rebuilding-the-profile = reconstruindo o perfil
cli-profile-rebuilt-the-profile-read-project-new-candidate-entr-written-unchanged = Perfil { $value0 } reconstruído: { $value1 } { $value2 ->
        [one] projeto lido
       *[other] projetos lidos
    }, { $value3 } { $value4 ->
        [one] novo candidato
       *[other] novos candidatos
    }, { $value5 } { $value6 ->
        [one] entrada gravada
       *[other] entradas gravadas
    }, { $value7 } sem alterações.
cli-profile-llm-calls-fell-back-to-the-zero-llm-path = { "  " }Chamadas ao LLM: { $value0 } ({ $value1 } usaram o caminho sem LLM como alternativa)
cli-profile-left-alone-edited-by-hand = { "  " }mantidas sem alterações (editadas manualmente): { $value0 }
cli-profile-error-error = { "  " }erro: { $value0 }
cli-profile-reading-the-current-directory = lendo o diretório atual
cli-profile-reading-the-profile-entries-for-this-project = lendo as entradas de perfil deste projeto
cli-profile-does-not-exist-nothing-to-remove = { $value0 } não existe; nada para remover.
cli-profile-has-no-profile-block-nothing-to-remove = { $value0 } não contém bloco de perfil; nada para remover.
cli-profile-removed-it-held-only-the-profile-block = { $value0 }: removido (continha apenas o bloco de perfil)
cli-profile-profile-block-removed = { $value0 }: bloco de perfil removido ({ $value1 })
cli-project-setting-project-access = definindo acesso ao projeto
cli-project-repo-is-now-was = { $value0 } agora é { $value1 } (antes era { $value2 }).
cli-project-repo-was-already-nothing-changed = { $value0 } já era { $value1 }; nada alterado.
cli-project-these-users-have-written-to-repo-and-hold-no-grant-so-they-are-now-refused = Estes usuários gravaram em { $value0 } e não têm permissão concedida, portanto agora terão o acesso recusado:
cli-project-grant-the-ones-who-should-keep-access-ai-memory-user-grant-user-name-workspa = Conceda permissão a quem deve manter o acesso: ai-memory user grant --user <name> --workspace { $value0 } --project { $value1 } --level write
cli-project-registry-canonicalizing-project-binding-checkout = normalizando o checkout do vínculo do projeto
cli-project-registry-locking-client-project-registry = bloqueando o registro de projetos do cliente
cli-project-registry-serializing-client-project-registry = serializando o registro de projetos do cliente
cli-read-page-path = caminho: { $value0 }{ "\u000A" }
cli-recent-page-s = { $value0 } página(s):{ "\u000A" }
cli-recovery-acquiring-the-recovery-spool-lock = adquirindo o bloqueio do spool de recuperação
cli-recovery-restoring-previously-quarantined-hook-events = restaurando eventos de hooks anteriormente em quarentena
cli-recovery-quarantining-correlated-hook-events = colocando eventos de hooks correlacionados em quarentena
cli-recovery-restoring-quarantined-hook-events = restaurando eventos de hooks em quarentena
cli-recovery-ai-memory-server-at-is-unreachable-nothing-to-recover-yet = ai-memory: o servidor em { $value0 } está inacessível — nada para recuperar ainda.
cli-recovery-journal-unreadable-error = { "  " }registro: ilegível ({ $value0 })
cli-recovery-journal-entr-await-recovery-degraded-launch-es-failed-transcript-import-s = { "  " }registro: { $value0 } { $value1 ->
        [one] entrada
       *[other] entradas
    }{ $value2 ->
        [one] { " aguardando" }
       *[other] { " aguardando" }
    } recuperação ({ $value3 } inicialização(ões) degradada(s), { $value4 } falha(s) de importação de transcrição)
cli-recovery-spool-hook-event-s-queued-locally = { "  " }spool:   { $value0 } evento(s) de hooks na fila local
cli-recovery-start-the-server-or-fix-ai-memory-server-url-then-run-ai-memory-recover-agai = Inicie o servidor (ou corrija AI_MEMORY_SERVER_URL) e execute `ai-memory recover` novamente.
cli-recovery-ai-memory-recovering-against = ai-memory: recuperando em { $value0 }…
cli-recovery-spool-another-drainer-held-the-lock-transcript-recovery-was-deferred = { "  " }spool: outro processo de envio mantinha o bloqueio; a recuperação de transcrições foi adiada
cli-recovery-spool-hook-event-s-delivered-remain-queued-dropped = { "  " }spool: { $value0 } evento(s) de hooks entregues, { $value1 } permanecem na fila, { $value2 } descartados
cli-recovery-recovery-is-incomplete-because-hook-events-remain-queued-or-were-dropped-exa = { "  " }a recuperação está incompleta porque há eventos de hooks na fila ou descartados; transcrições com correlação exata ainda podem substituir eventos em quarentena
cli-recovery-in = { "  " }[{ $value0 }] { $value1 } { $value2 } em { $value3 }/{ $value4 } — { $value5 }
cli-recovery-events = { "  " }({ $value0 } eventos)
cli-recovery-recovered-event-s-across-journal-entr-failed-remain = { $value0 } evento(s) recuperado(s) em { $value1 } { $value2 ->
        [one] entrada
       *[other] entradas
    } do registro; { $value3 } com falha; { $value4 } restantes
cli-recovery-recovery-journal-error-error = erro no registro de recuperação: { $value0 }
cli-recovery-failed-entries-stay-journaled-with-their-reason-fix-the-cause-and-re-run-ai = as entradas com falha permanecem no registro com o motivo; corrija a causa e execute `ai-memory recover` novamente (é seguro repetir — importações são deduplicadas)
cli-reindex-opening-store-for-reindex = abrindo armazenamento para reindexação
cli-reindex-checking-whether-the-sqlite-store-is-clean-for-reindex = verificando se o armazenamento SQLite está limpo para reindexação
cli-reindex-opening-wiki = abrindo wiki
cli-reindex-rebuilding-index-from-wiki = reconstruindo índice a partir de wiki/
cli-rename-project-one-of-from-or-project-id-is-required = é necessário informar --from ou --project-id
cli-rename-workstream-getting-managed-workstream-checkout = obtendo checkout do workstream gerenciado
cli-rename-workstream-workstream-already-has-that-name = O workstream '{ $value0 }' já tem esse nome.{ "\u000A" }
cli-rename-workstream-renamed-workstream-to-id = Workstream '{ $value0 }' renomeado para '{ $value1 }'.{ "\u000A" }    id: { $value2 }{ "\u000A" }
cli-repair-backfill-timestamps-note-returned-the-maximum-scanned-sessions-for-this-checkout-older-sessions = { "  " }nota: { $value0 } retornou o máximo de sessões verificadas para este checkout; sessões mais antigas podem existir e não foram consideradas.
cli-restore-agents-backup-archive-not-found-at = arquivo de backup não encontrado em { $value0 }
cli-restore-agents-creating-restore-staging-directory = criando diretório temporário de restauração
cli-restore-agents-parsing-agent-backup-manifest = analisando manifesto do backup de agentes
cli-restore-agents-archive-manifest-schema-version-is-not-supported-supported-1 = a versão de esquema { $value0 } do manifesto não tem suporte (compatíveis: 1..={ $value1 })
cli-restore-agents-archive-manifest-contains-duplicate-asset-path = o manifesto do arquivo contém o caminho de recurso duplicado { $value0 }
cli-restore-agents-archive-manifest-references-missing-asset = o manifesto do arquivo referencia o recurso ausente { $value0 }
cli-restore-agents-archive-asset-is-missing-its-required-sha-256-checksum = o recurso { $value0 } do arquivo não contém o checksum SHA-256 obrigatório
cli-restore-agents-sha-256-integrity-mismatch-for-asset-expected-got = integridade SHA-256 divergente para o recurso { $value0 }: esperado { $value1 }, obtido { $value2 }
cli-restore-agents-archive-asset-has-unsafe-permission-bits-o = o recurso { $value0 } do arquivo tem bits de permissão inseguros { $value1 }
cli-restore-agents-archive-manifest-maps-more-than-one-asset-to = o manifesto do arquivo mapeia mais de um recurso para { $value0 }
cli-restore-agents-refusing-to-replace-non-file-destination = recusando substituir o destino { $value0 }, que não é um arquivo
cli-restore-agents-archive-contains-unmanifested-asset = o arquivo contém o recurso { $value0 }, ausente do manifesto
cli-restore-agents-all-identical-count-selected-assets-are-already-identical-on-host-nothing-to = Todos os { $value0 } recursos selecionados já são idênticos no host. Nada para restaurar.
cli-restore-agents-no-archive-assets-matched-the-selected-agent-and-scope-filters = Nenhum recurso do arquivo corresponde aos filtros de agente e escopo selecionados.
cli-restore-agents-restoration-complete-created-updated-identical-skipped-existing = { "\u000A" }Restauração concluída: { $value0 } criados, { $value1 } atualizados, { $value2 } idênticos, { $value3 } ignorados (existentes).
cli-restore-agents-note-this-archive-contains-sanitized-configurations-restored-mcp-configs-may = Nota: este arquivo contém configurações sanitizadas. As configurações MCP restauradas podem exigir a substituição de valores '[REDACTED:...]' por credenciais válidas.
cli-restore-agents-skip-already-exists-pass-force-to-overwrite = { "  " }[IGNORADO] { $value0 } (já existe, informe --force para sobrescrever)
cli-restore-agents-updated = { "  " }atualizado { $value0 }
cli-restore-agents-created = { "  " }criado { $value0 }
cli-restore-agents-restore-preview-dry-run-pass-apply-to-execute = Prévia de restauração (simulação, informe --apply para executar):{ "\u000A" }
cli-restore-agents-asset-row = { "  " }{ $value0 } [{ $value1 }] { $value2 } ({ $value3 } bytes) -> { $value4 }
cli-restore-agents-skipped-identical-count-identical-files = { "\u000A" }  ({ $value0 } arquivos idênticos ignorados)
cli-restore-agents-note-archive-contains-sanitized-configurations-with-redacted-credentials = { "\u000A" }  Nota: o arquivo contém configurações sanitizadas com credenciais ocultadas.
cli-restore-agents-review-the-archive-source-and-listed-destinations-skills-plugins-and-instruc = { "\u000A" }Revise a origem do arquivo e os destinos listados: skills, plugins e instruções são conteúdo ativo. Execute novamente com `--apply` para restaurar.
cli-restore-page-restoring-page-via-server = restaurando página pelo servidor
cli-resume-reading-the-current-checkout = lendo o checkout atual
cli-search-no-results-for-query = nenhum resultado para { $value0 }{ "\u000A" }
cli-search-result-s-for-query = { $value0 } resultado(s) para { $value1 }:{ "\u000A" }
cli-search-scoped-hit = { "  " }{ $value0 }/{ $value1 }  { $value2 }  classificação={ $value3 }
cli-search-hit = { "  " }{ $value0 }  classificação={ $value1 }
cli-service-host-locating-the-running-ai-memory-binary = localizando o binário ai-memory em execução
cli-service-host-the-service-host-supervisor-stopped-unexpectedly = o supervisor service-host parou inesperadamente
cli-setup-locating-your-home-directory = localizando seu diretório pessoal
cli-setup-create-the-data-dir-and-write-with-a-new-root-bearer-token-in-auth-tokens-de = { "  " }- criar o diretório de dados { $value0 } e gravar { $value1 } (com um novo token bearer root em auth-tokens/default quando o diretório de dados for novo)
cli-setup-keep-the-existing-install-at = { "  " }- manter a instalação existente em { $value0 } ({ $value1 })
cli-setup-install-and-start-a-at = { "  " }- instalar e iniciar um { $value0 } em { $value1 }{ $value2 }
cli-setup-service-reason-2 = { "  " }- serviço: { $value0 }
cli-setup-wire-no-agent-none-name-one-with-agent-name = { "  " }- não configurar nenhum agente: nenhum { $value0 }; indique um com --agent <name>
cli-setup-wire-these-agents = { "  " }- configurar estes agentes:
cli-setup-number-name-leave-it-to-its-enabled-ai-memory-plugin-which-carries-the-hooks = { "      " }{ $value0 }. { $value1 }: manter com seu plugin ai-memory habilitado ({ $value2 }), que fornece os hooks, a entrada MCP e as skills
cli-setup-number-name-mcp-skills-why = { "      " }{ $value0 }. { $value1 }: { $value2 } + skills ({ $value3 })
cli-setup-check-healthz-memory-status-and-doctor-read-only = { "  " }- verificar /healthz, memory_status e doctor (somente leitura)
cli-setup-also-supported-not-in-this-plan-add-with-agent-name = Também compatíveis, fora deste plano (adicione com --agent <name>): { $value0 }
cli-setup-nothing-was-changed-there-is-no-terminal-to-confirm-this-plan-on-re-run-ai-m = nada foi alterado: não há terminal para confirmar este plano. Execute `ai-memory setup --yes` novamente para aplicar.
cli-setup-proceed-y-n-or-type-the-numbers-of-the-agents-to-wire-e-g-1-3 = Prosseguir? [s/N], ou digite os números dos agentes a configurar (ex.: 1 3):{ " " }
cli-setup-proceed-y-n = Prosseguir? [s/N]{ " " }
cli-setup-reading-the-answer = lendo a resposta
cli-setup-calling-memory-status = chamando memory_status
cli-setup-web-binding-the-setup-page-to-127-0-0-1 = vinculando a página de configuração a 127.0.0.1
cli-setup-web-open-the-setup-page-to-choose-your-agents-and-apply-url = Abra a página de configuração para escolher seus agentes e aplicar:{ "\u000A" }{ "\u000A" }  { $value0 }{ "\u000A" }
cli-setup-web-it-answers-only-on-this-machine-over-ssh-forward-the-port-first-ssh-l-port-1 = Ela responde somente nesta máquina. Por SSH, encaminhe a porta primeiro (`ssh -L { $value0 }:127.0.0.1:{ $value1 } <host>`) e abra o mesmo endereço. Pressione Ctrl-C aqui para parar sem alterar nada.
cli-setup-web-the-setup-page-was-idle-for-15-minutes-stopping-it = A página de configuração ficou inativa por 15 minutos; encerrando.
cli-setup-web-serving-the-setup-page = servindo a página de configuração
cli-setup-web-the-setup-page-closed-nothing-was-changed = A página de configuração foi fechada; nada foi alterado.
cli-setup-web-setup-finished-with-problems-see-the-output-on-the-page-or-run-ai-memory-set = a configuração terminou com problemas; veja a saída na página ou execute `ai-memory setup` aqui
cli-setup-web-the-setup-page-stopped-while-setup-was-still-running = a página de configuração parou enquanto a configuração ainda estava em execução
cli-show-canonicalizing-the-current-directory = normalizando o diretório atual
cli-show-invalid-project-name-name-use-lowercase-ascii-letters-digits-dot-dash-or-und = nome de projeto inválido { $value0 }: use letras ASCII minúsculas, dígitos, ponto, hífen ou sublinhado, começando com letra ou dígito
cli-show-project-directory-already-exists = o diretório do projeto já existe: { $value0 }
cli-show-writing-the-staged-project-marker = gravando o marcador de projeto preparado
cli-show-project-directory-appeared-while-it-was-being-prepared = o diretório do projeto surgiu enquanto estava sendo preparado: { $value0 }
cli-show-created = criado { $value0 }
cli-status-local-spool-server-unreachable-rendered = spool local (servidor inacessível): { $value0 }
cli-status-local-spool-server-unreachable = spool local (servidor inacessível):
cli-status-pending = { "  " }pendentes:  { $value0 }
cli-status-oldest = { "  " }mais antigo: { $value0 }
cli-status-retries = { "  " }tentativas: { $value0 }
cli-status-the-spool-is-full-events-newly-captured-events-are-dropped-until-space-is-av = { "  " }O spool está cheio ({ $value0 } eventos): novos eventos capturados são descartados até que haja espaço. Restabeleça o servidor para interromper a perda.
cli-status-event-s-are-queued-locally-and-will-be-delivered-once-the-server-is-reachabl = { "  " }{ $value0 } evento(s) estão na fila local e serão entregues quando o servidor estiver acessível novamente.
cli-status-windows-logon-server-unreachable-rendered = logon do Windows (servidor inacessível): { $value0 }
cli-uninstall-warning-skipping-the-active-omp-profile-error = aviso: ignorando o perfil OMP ativo: { $value0 }
cli-uninstall-getting-cwd-for-hook-removal = obtendo CWD para remoção de hooks
cli-uninstall-getting-cwd-for-instruction-removal = obtendo CWD para remoção de instruções
cli-uninstall-getting-cwd-for-skill-removal = obtendo CWD para remoção de skills
cli-uninstall-nothing-to-remove-ai-memory-wiring-not-found = Nada para remover. Configuração do ai-memory não encontrada.
cli-uninstall-would-remove-from = removeria { $value0 } de { $value1 }
cli-uninstall-would-delete = excluiria { $value0 } ({ $value1 })
cli-uninstall-skipped-because-it-no-longer-looks-like-an-ai-memory-generated = { $value0 } ignorado porque não parece mais um { $value1 } gerado pelo ai-memory
cli-uninstall-deleted = ✓ excluído { $value0 }
cli-uninstall-removed-empty-directory = ✓ diretório vazio removido { $value0 }
cli-uninstall-stopped-and-removed-the-user-service = ✓ serviço de usuário { $value0 } parado e removido
cli-uninstall-would-stop-and-remove-the-user-service = pararia e removeria o serviço de usuário { $value0 }
cli-uninstall-leaving-ai-memory-setup-did-not-write-it-or-it-was-edited-since-remove-it-yo = mantendo { $value0 }: `ai-memory setup` não o gravou ou ele foi editado desde então; remova manualmente se ele executar ai-memory
cli-uninstall-wiring-removed-ai-memory-s-server-data-live-in-its-container-volume = Configuração removida. O servidor e os dados do ai-memory ficam no contêiner/volume —
cli-uninstall-tear-those-down-manually = remova-os manualmente:
cli-uninstall-docker-compose-f-docker-docker-compose-yml-down-v = { "  " }docker compose -f docker/docker-compose.yml down -v
cli-uninstall-docker-volume-rm-ai-memory-data-if-you-used-the-default-volume = { "  " }docker volume rm ai-memory-data   # se você usou o volume padrão
cli-uninstall-rm-f-bin-ai-memory-the-wrapper-script-if-installed = { "  " }rm -f bin/ai-memory               # o script wrapper, se instalado
cli-uninstall-local-data-dir-was-left-intact-to-wipe-it-ai-memory-reset-confirm-or-re-run = O diretório de dados local foi mantido. Para apagá-lo: `ai-memory reset --confirm` (ou execute novamente com --purge-data).
cli-upgrade-already-up-to-date-current = já está atualizado ({ $value0 })
cli-upgrade-no-github-release-asset-for-this-os-arch = nenhum recurso de versão do GitHub para este sistema/arquitetura
cli-upgrade-creating-extract-temp-dir = criando diretório temporário de extração
cli-upgrade-extracting-release-archive = extraindo arquivo da versão
cli-upgrade-downloading-asset-tag = → baixando { $value0 } ({ $value1 })
cli-upgrade-release-archive-checksum-mismatch-expected-expected-got-actual = checksum do arquivo da versão divergente (esperado { $value0 }, obtido { $value1 })
cli-upgrade-checksum-ok = { "  " }✓ checksum correto
cli-upgrade-executable-has-no-parent-directory = o executável não tem diretório pai
cli-upgrade-replacing = → substituindo { $value0 }
cli-upgrade-refreshing = → atualizando { $value0 }
cli-upgrade-skipping-unknown-staged-agent-dir-name = { "  " }(ignorando diretório preparado de agente desconhecido `{ $value0 }`)
cli-user-creating-human-user = criando usuário humano
cli-user-listing-users = listando usuários
cli-user-no-registered-users = (nenhum usuário registrado)
cli-user-resetting-password = redefinindo senha
cli-user-disabling-user = desabilitando usuário
cli-user-disabled-user = ✓ usuário '{ $value0 }' desabilitado
cli-user-enabling-user = habilitando usuário
cli-user-enabled-user = ✓ usuário '{ $value0 }' habilitado
cli-user-patching-user = alterando usuário
cli-user-updated-user = ✓ usuário '{ $value0 }' atualizado ({ $value1 })
cli-user-verb-user = ✓ usuário '{ $value1 }': { $value0 }
cli-user-name-name = { "  " }nome:  { $value0 }
cli-user-email-email = { "  " }email: { $value0 }
cli-user-role-store-this-temporary-password-now-it-will-not-be-shown-again-the-user-m = { "  " }função: { $value0 }{ "\u000A" }{ "\u000A" }Salve esta senha temporária agora — ela NÃO será exibida novamente. O usuário precisa alterá-la no próximo acesso.
cli-workstreams-inspecting-managed-workstream-checkout = examinando checkout do workstream gerenciado
cli-workstreams-workstream-pagination-offset-overflow = estouro do deslocamento de paginação de workstreams
cli-workstreams-managed-workstreams-for-workspace-project = Workstreams gerenciados para { $value0 }/{ $value1 }:{ "\u000A" }
cli-write-page-reading-body-from-stdin = lendo conteúdo da entrada padrão
cli-write-page-writing-page-via-server = gravando página pelo servidor
cli-install-hooks-internal-the-muse-settings-path-is-resolved-before-apply = interno: o caminho das configurações do Muse é resolvido antes da aplicação
cli-install-hooks-could-not-resolve-current-dir-for-grok-project-configuration = não foi possível resolver o diretório atual para a configuração de projeto do Grok
cli-install-hooks-grok-project-configuration-may-override-grok-home-pass-explicit-server-url-a = a configuração de projeto do Grok { $value0 } pode sobrepor GROK_HOME; informe --server-url e --auth-token explicitamente em vez de inferir as configurações MCP
cli-presentation-doctor-capture-coverage = DIAGNÓSTICO / cobertura de captura
cli-presentation-connection = Conexão
cli-presentation-capture-coverage-for-server = Cobertura de captura de { $value0 }/{ $value1 } (servidor { $value2 })
cli-presentation-identity-server = { "  " }identidade: { $value0 } ({ $value1 }, servidor { $value2 })
cli-presentation-identity-unavailable = { "  " }identidade: indisponível
cli-presentation-auth = { "  " }autenticação: { $value0 }
cli-presentation-ai-memory-capture-owner-is-active-native-capture-is-suppressed-in-this-proce = { "  " }AI_MEMORY_CAPTURE_OWNER está ativo: a captura nativa está suprimida neste processo.{ "\u000A" }  Confirme se o produtor externo está entregando eventos.
cli-presentation-capture-coverage-unavailable = { "  " }cobertura de captura: indisponível ({ $value0 })
cli-presentation-capture-health = Saúde da captura
cli-presentation-recent-window-all-on-disk-sessions = { "  " }período recente: todas as sessões em disco{ "\u000A" }
cli-presentation-recent-window-last-days = { "  " }período recente: últimos { $value0 } dias{ "\u000A" }
cli-presentation-the-settings-marker-has-an-invalid-capture-section-it-fails-closed-file-and = ⚠ { $value0 }: o marcador de configurações contém uma seção `[capture]` inválida.{ "\u000A" }  Ele bloqueia o conteúdo por segurança — conteúdo de ferramentas de arquivos e shell é reduzido a metadados, sem vazamentos — mas as exclusões de `ignore_paths` NÃO se aplicam enquanto estiver inválido. Corrija o TOML e execute `ai-memory doctor` novamente para confirmar.{ "\u000A" }
cli-presentation-note-a-shell-powershell-hook-install-does-not-enforce-capture-policy-v1-excl = Nota: uma instalação de hooks shell/PowerShell não impõe a política de captura v1 — "excluído" abaixo só vale para hooks nativos ou uma integração gerada. Verifique o comando de hook configurado; uma simulação de `ai-memory install-hooks` informa o caminho selecionado para instalação, não o ativo.{ "\u000A" }
cli-presentation-no-local-harness-sessions-found-for-this-project-server-capture-counts-are-u = { "  " }Nenhuma sessão local de cliente encontrada para este projeto; os contadores de captura do servidor estão indisponíveis.
cli-presentation-no-local-harness-sessions-found-for-this-project-and-nothing-captured-yet = { "  " }Nenhuma sessão local de cliente encontrada para este projeto e nada capturado ainda.
cli-presentation-agent-coverage = Cobertura dos agentes
cli-presentation-agent-row = { "  " }{ $value0 } { $value1 } { $value2 } locais ({ $value3 } recentes) → { $value4 } capturadas
cli-presentation-no-captured-session-check-the-external-producer-and-repository-policy = { "      " }Nenhuma sessão capturada: verifique o produtor externo e a política do repositório.
cli-presentation-ran-here-but-nothing-was-captured-install-its-hook-ai-memory-install-hooks-a = { "      " }└ executado aqui, mas nada foi capturado — instale seu hook:{ "\u000A" }        ai-memory install-hooks --agent { $value0 } --apply
cli-presentation-count-session-s-contain-events-from-multiple-capture-sources-backfill-can-al = { "      " }{ $value0 } sessão(ões) contêm eventos de várias fontes de captura.{ "\u000A" }      A importação do histórico também pode misturar metadados de origem; isso não comprova captura duplicada.
cli-presentation-native-memory-file = { "      " }memória nativa: { $value0 } ({ $value1 } { $value2 ->
        [one] arquivo
       *[other] arquivos
    })
cli-presentation-capture = { "      " }captura: { $value0 }
cli-presentation-capture-verdict-unavailable-server-capture-counts-could-not-be-resolved = { "\u000A" }Veredito de captura indisponível: não foi possível resolver os contadores de captura do servidor.
cli-presentation-every-harness-that-ran-in-this-project-recently-is-captured-on-the-server = { "\u000A" }✓ Todos os clientes executados recentemente neste projeto têm captura no servidor.
cli-presentation-harness-es-ran-here-recently-with-no-captured-sessions-remedy = { "\u000A" }⚠ { $value0 } cliente(s) executado(s) aqui recentemente sem sessões capturadas: { $value1 }.{ "\u000A" }  { $value2 }
cli-presentation-status-memory-capture = ESTADO / memória e captura
cli-presentation-server = { "  " }servidor:     { $value0 }
cli-presentation-data-dir = { "  " }data-dir:     { $value0 }
cli-presentation-db = { "  " }banco:        { $value0 }
cli-presentation-bind = { "  " }bind:         { $value0 }
cli-presentation-auth-2 = { "  " }autenticação: { $value0 }
cli-presentation-memory = Memória
cli-presentation-pages-all-versions = { "  " }páginas:      { $value0 } (todas as versões: { $value1 })
cli-presentation-sessions = { "  " }sessões:      { $value0 }
cli-presentation-observations = { "  " }observações:  { $value0 }
cli-presentation-search-index = Índice de busca
cli-presentation-fts-pages-observations = { "  " }fts:          páginas { $value0 }/{ $value1 }; observações { $value2 }/{ $value3 }
cli-presentation-embeddings-rows-latest-pages-missing = { "  " }embeddings:   { $value0 } registros; { $value1 } páginas mais recentes sem embedding
cli-presentation-unembeddable-empty-body-no-embedder-can-cover-these = { "    " }sem embedding possível: { $value0 } (conteúdo vazio; nenhum gerador pode processar)
cli-presentation-dim-rows = { "    " }{ $value0 }/{ $value1 } dimensão { $value2 }: { $value3 } registros
cli-presentation-embed-failures-unresolved-recovered-since = { "    " }falhas de embedding: { $value0 } não resolvidas ({ $value1 } recuperadas desde então)
cli-presentation-storage-links = Armazenamento e links
cli-presentation-storage-on-disk-reclaimable-pct-1 = { "  " }armazenamento: { $value0 } em disco, { $value1 } recuperáveis ({ $value2 }%)
cli-presentation-filesystem-free = { "    " }espaço livre no sistema de arquivos: { $value0 }
cli-presentation-would-return-it-blocks-writes-while-it-runs = { "    " }`{ $value0 }` devolveria esse espaço (bloqueia gravações durante a execução)
cli-presentation-links-latest-page-links-unresolved-stale = { "  " }links:        { $value0 } links de páginas mais recentes (não resolvidos: { $value1 }, desatualizados: { $value2 })
cli-presentation-typed-edges = { "    " }conexões tipadas: { $value0 }
cli-presentation-wiki-format-migrated-pre-migration-backup-still-on-disk-path = { "  " }formato da wiki: { $value0 }; backup anterior à migração ainda em disco: { $value1 } ({ $value2 })
cli-presentation-wiki-format-migrated = { "  " }formato da wiki: { $value0 }
cli-presentation-capture-delivery = Captura e entrega
cli-presentation-write-queue-queued-capacity = { "  " }fila de gravação: { $value0 }/{ $value1 }
cli-presentation-spool = { "  " }spool:
cli-presentation-pending = { "    " }pendentes:  { $value0 }
cli-presentation-oldest = { "    " }mais antigo: { $value0 }
cli-presentation-retries = { "    " }tentativas: { $value0 }
cli-presentation-recovery = { "  " }recuperação:
cli-presentation-journal-journal-pending-pending-journal-degraded-degraded-journal-finish-fai = { "    " }registro:   { $value0 } pendentes ({ $value1 } degradadas, { $value2 } com falha no encerramento)
cli-presentation-error-error = { "    " }erro:       { $value0 }
cli-presentation-action-run = { "    " }ação:       execute `{ $value0 }`
cli-presentation-ingest-server-this-process = { "  " }ingestão (servidor, neste processo):
cli-presentation-accepted = { "    " }aceitos:    { $value0 }
cli-presentation-dropped-capture-policy = { "    " }descartados: { $value0 } (política de captura)
cli-presentation-outcomes-stored-replayed-resumed-ignored-end-failed = { "    " }resultados: { $value0 } armazenados, { $value1 } reproduzidos, { $value2 } retomados, { $value3 } encerramentos ignorados, { $value4 } falhas
cli-presentation-drops-unauthorized-collision-invalid = { "    " }descartes:  { $value0 } não autorizados, { $value1 } colisões, { $value2 } inválidos
cli-presentation-shed-saturated-rate-limited = { "    " }rejeitados: { $value0 } por saturação, { $value1 } por limite de taxa
cli-presentation-last-write = { "    " }última gravação: { $value0 }
cli-presentation-capture-mode-allowlist-repositories-without-a-ai-memory-toml-marker-send-not = { "  " }modo de captura: allowlist — repositórios sem marcador .ai-memory.toml não enviam nada
cli-presentation-optional-models = Modelos opcionais
cli-presentation-providers = { "  " }provedores:
cli-presentation-llm = { "    " }llm:       { $value0 }
cli-presentation-embedding = { "    " }embedding: { $value0 }
cli-presentation-retry-hint = { "    " }tentativa: { $value0 }
cli-presentation-fallback-chain = { "    " }cadeia de alternativas:
cli-doctor-an-authorized-write-can-promote-this-project-name-in-place = { "    " }uma gravação autorizada pode promover este nome de projeto no local{ "\u000A" }
cli-list-projects-no-projects-found = Nenhum projeto encontrado.{ "\u000A" }
cli-read-page-expired-true = expirada: true{ "\u000A" }
cli-recent-no-recent-pages = nenhuma página recente{ "\u000A" }
cli-recovery-no-session-id = (sem ID de sessão)
cli-setup-named-with-agent = indicado com --agent
cli-workstreams-no-managed-workstreams-for-this-checkout = Nenhum workstream gerenciado para este checkout.{ "\u000A" }
cli-workstreams-no-linked-harnesses = nenhum cliente vinculado
cli-presentation-unavailable = indisponível

cli-help-option-help = Exibir ajuda
cli-help-option-version = Exibir versão
cli-help-commands = Comandos
cli-parse-unknown-argument = Argumento desconhecido
cli-parse-unknown-command = Comando desconhecido
cli-parse-missing-command = É necessário informar um comando
cli-parse-required-argument = Falta um argumento obrigatório
cli-parse-conflicting-arguments = Os argumentos não podem ser combinados
cli-parse-value-count = Número incorreto de valores
cli-parse-invalid-value = Valor de argumento inválido
cli-parse-help-hint = Execute 'ai-memory --help' para mais informações.
cli-help-options = Opções
cli-help-arguments = Argumentos
cli-help-template = { "{before-help}{name} {version}\u000A{about-with-newline}\u000AUso: {usage}\u000A\u000A{all-args}{after-help}" }

cli-http-server-response = { $method } { $path }: o servidor retornou { $status }: { $detail }
cli-http-configured-server-hint = { "\u000A" }AI_MEMORY_SERVER_URL está definido como { $value0 }, mas não houve resposta (conexão recusada ou tempo esgotado). Verifique se o servidor está em execução, se a porta está acessível deste host e, se remoto, se as configurações de firewall e token bearer correspondem.
cli-http-default-server-hint = { "\u000A" }AI_MEMORY_SERVER_URL NÃO está definido; a CLI usou o padrão { $value0 } e não houve resposta (conexão recusada ou tempo esgotado). Se o servidor estiver em outra máquina (ex.: homelab), execute `export AI_MEMORY_SERVER_URL=http://<server>:49374` e, se houver autenticação, `export AI_MEMORY_AUTH_TOKEN=<token>` antes de tentar novamente.
cli-http-unreachable = não foi possível acessar { $value0 }.{ $value1 }
cli-http-request-failed = a solicitação HTTP para { $value0 } falhou

cli-status-client-version = ai-memory { $value0 } (cliente)
cli-status-major-version-mismatch = ⚠ O cliente é { $value0 } e o servidor é { $value1 }: versões principais diferentes podem não compreender as solicitações entre si. Atualize o lado mais antigo.
cli-status-provider-disabled = desabilitado
cli-status-provider-unknown = { $value0 } desconhecido (nenhuma chamada ainda)
cli-status-time-unknown = horário desconhecido
cli-status-provider-ok = { $value0 } ok (última chamada: { $value1 })
cli-status-provider-error = { $value0 } erro ({ $value1 })
cli-status-last-error = último erro: { $value0 }
cli-auth-posture-token = token bearer
cli-auth-posture-loopback = nenhuma (loopback) — correção: `{ $value0 }`
cli-auth-posture-none = nenhuma — defina AI_MEMORY_AUTH_TOKEN no servidor

cli-table-workspace = ESPAÇO DE TRABALHO
cli-table-project = PROJETO
cli-table-project-id = ID DO PROJETO
cli-table-pages = PÁGINAS

## Interactive selections and setup completion.

cli-picker-project = Projeto
cli-picker-new-project = + Novo projeto
cli-picker-new-project-detail = criar um diretório e instalar o contexto do agente
cli-picker-agent-title = Agente | { $value0 }
cli-picker-resume-title = Retomar workstream
cli-picker-horizontal-hint = esquerda/direita: cliente
cli-picker-empty = nenhuma opção disponível
cli-picker-no-match = Nenhum fluxo correspondente — altere a busca ou pressione Esc para limpar
cli-picker-search-title = { $title } [{ $matching }/{ $total }] | busca: { $query }
cli-picker-project-name = Nome do novo projeto
cli-picker-name-required = digite um nome
cli-picker-name-policy = use letras minúsculas, dígitos, ponto, hífen ou sublinhado
cli-picker-name-exists = { $value0 } já existe aqui
cli-picker-create-hint = { "  enter criar | esc cancelar" }
cli-picker-search-hint = { "  digite para buscar | ↑↓ mover | ←→ cliente | enter selecionar | esc limpar/cancelar" }
cli-picker-paged-hint = { "  " }cima/baixo mover{ $horizontal } | pgup/pgdn página | enter selecionar | esc cancelar [{ $selected }/{ $total }]
cli-picker-basic-hint = { "  " }cima/baixo mover{ $value0 } | enter selecionar | esc cancelar
cli-setup-ready-no-agent = O ai-memory está configurado, mas nenhum agente foi integrado; execute `ai-memory setup --agent <name>`.
cli-setup-ready-one = O ai-memory está configurado. Reinicie { $value0 } para carregar os novos hooks e a entrada MCP e pergunte: "O que você lembra sobre este projeto?" Ele responde pelas ferramentas do ai-memory; uma instalação nova ainda não tem memórias, e todas as sessões a partir de agora serão capturadas.
cli-setup-ready-many = O ai-memory está configurado. Reinicie { $value0 } para carregar os novos hooks e a entrada MCP e pergunte a um deles: "O que você lembra sobre este projeto?" Ele responde pelas ferramentas do ai-memory; uma instalação nova ainda não tem memórias, e todas as sessões a partir de agora serão capturadas.
cli-setup-codex-trust = O ai-memory está instalado, mas o Codex ainda não aprovou { $count } hook(s) ai-memory: { $hooks }. O Codex ignora hooks até que você os marque como confiáveis; até lá, suas sessões não são capturadas. Ação necessária: abra o Codex, execute /hooks e marque-os como confiáveis. { $closing }
cli-setup-web-sign-in = interface web: { $value0 } (o serviço executa com --enable-web); quando o navegador pedir usuário e senha, use qualquer nome de usuário e o conteúdo de { $value1 } como senha
cli-api-key-confirm-rotate = Renovar a chave de API '{ $value0 }'? Clientes com o segredo anterior receberão HTTP 401 imediatamente. (s/N) { " " }
cli-api-key-confirm-revoke = Revogar a chave de API '{ $value0 }'? (s/N) { " " }
cli-user-confirm-password = Redefinir a senha de '{ $value0 }'? A senha atual deixará de funcionar imediatamente. (s/N) { " " }
cli-user-confirm-disable = Desabilitar acesso humano para '{ $value0 }'? (s/N) { " " }
cli-setup-ready-prefix = { "O ai-memory está configurado. " }
cli-config-load-failed = Não foi possível carregar a configuração do ai-memory
cli-error-report = Erro: { $message }
cli-error-caused-by = Causado por:
