# Apresentação do relatório de diagnóstico

Em um terminal interativo, `ai-memory status` e `ai-memory doctor` agrupam
seus relatórios humanos em seções e usam cores para contagens e vereditos.
Linhas de relatórios interativos compartilham uma margem mínima de duas colunas e quebram nos espaços
para se ajustar à largura do terminal, mantendo tokens longos intactos.
Todos os campos de diagnóstico, avisos e comandos de remediação permanecem visíveis.

Redirecionar o stdout, definir `NO_COLOR` (incluindo um valor vazio) ou definir
`TERM=dumb` preserva o relatório simples original no stdout. A saída em JSON permanece
inalterada. Esses controles de apresentação se aplicam aos relatórios no stdout;
o tracing no stderr continua a usar a configuração de log existente.
