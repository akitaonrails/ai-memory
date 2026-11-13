# Diagnostic report presentation

On an interactive terminal, `ai-memory status` and `ai-memory doctor` group
their human reports into sections and use colors for counts and verdicts.
Interactive report lines share a minimum two-column margin and wrap at spaces
to fit the terminal width, keeping long tokens intact.
All diagnostic fields, warnings and remediation commands remain visible.

Redirecting stdout, setting `NO_COLOR` (including an empty value), or setting
`TERM=dumb` preserves the original plain report on stdout. JSON output is
unchanged. These presentation controls apply to the reports on stdout;
tracing on stderr continues to use the existing logging configuration.

`status --json` includes the server's `write_queue` (`[queued, capacity]`)
and `wiki_format` fields. Queue depth shows whether writes are falling
behind; wiki format includes migration state and any pre-migration backup
still on disk. These fields are omitted when an older server does not
report them.
