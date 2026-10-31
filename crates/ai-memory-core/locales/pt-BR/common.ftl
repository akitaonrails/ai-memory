## Shared elapsed times and inflected display nouns.

common-time-now = agora mesmo
common-time-minutes = { $count ->
    [one] há { $count } minuto
   *[other] há { $count } minutos
    }
common-time-hours = { $count ->
    [one] há { $count } hora
   *[other] há { $count } horas
    }
common-time-days = { $count ->
    [one] há { $count } dia
   *[other] há { $count } dias
    }
common-time-months = { $count ->
    [one] há { $count } mês
   *[other] há { $count } meses
    }
common-time-years = { $count ->
    [one] há { $count } ano
   *[other] há { $count } anos
    }
web-count-session = { $count ->
    [one] sessão
   *[other] sessões
    }
web-count-event = { $count ->
    [one] evento
   *[other] eventos
    }
web-count-observation = { $count ->
    [one] observação
   *[other] observações
    }
web-count-result = { $count ->
    [one] resultado
   *[other] resultados
    }
web-count-message = { $count ->
    [one] mensagem
   *[other] mensagens
    }
web-count-project = { $count ->
    [one] projeto
   *[other] projetos
    }
web-count-page = { $count ->
    [one] página
   *[other] páginas
    }
web-count-version = { $count ->
    [one] versão
   *[other] versões
    }
web-count-guide = { $count ->
    [one] guia
   *[other] guias
    }
web-count-captured-session = { $count ->
    [one] sessão capturada
   *[other] sessões capturadas
    }
web-count-sanitized-event = { $count ->
    [one] evento sanitizado
   *[other] eventos sanitizados
    }

common-status-active = ativo
common-status-interrupted = interrompido
common-status-completed = concluído
common-status-empty = vazio
common-status-pending = pendente
common-status-claimed = coletada
common-status-cancelled = cancelada
common-status-editing = em edição
common-status-disabled = desabilitado
common-status-unknown = desconhecido
common-status-ok = ok
common-status-error = erro
common-status-warn = aviso
common-status-open = aberta
common-status-ended = encerrada
common-status-revoked = revogada

## Fixed protocol errors. Wire values remain English.
common-error-invalid-credentials = Credenciais inválidas
common-error-auth-unavailable = Autenticação indisponível
common-error-password-confirmation = A confirmação da senha não confere
common-error-password-reserved = Esta senha é reservada e não pode ser usada
common-error-password-policy = A senha não atende à política de segurança (12 a 1024 bytes UTF-8)
common-error-csrf = A verificação de segurança expirou. Recarregue a página
common-error-password-change = É necessário alterar a senha
common-error-auth-required = É necessário entrar na conta
common-error-login-rate = Muitas tentativas de acesso. Aguarde antes de tentar novamente
common-error-cross-origin = Solicitação de formulário de outra origem recusada
common-error-management-password = É necessário alterar a senha
common-error-admin = Acesso de administrador necessário
common-error-password-length = Use pelo menos 12 caracteres.
common-error-scope-pair = Informe o espaço de trabalho e o projeto
common-error-session-required = O ID da sessão é obrigatório
common-error-files-remain = A exclusão no banco foi concluída, mas alguns arquivos da wiki não puderam ser removidos. Peça a um administrador para verificar os logs do servidor.
