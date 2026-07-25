# Shell completions

`ai-memory completions <shell>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `powershell`, or `elvish`. The script is generated from
the binary's own command tree, so it covers every subcommand and flag of the
version that produced it — including nested commands like `user rotate-token`
and `auth login`.

The Docker wrapper's `upgrade` command is wrapper-owned rather than part of the
native clap command tree, so it is the one subcommand not present in generated
completions.

The command reads no config and does not need a data directory, so it can be
run before `ai-memory init` or inside a packaging step.

The Docker wrapper buffers this bounded command before writing it to stdout, so
short consumers such as `head` can close the pipe without Docker adding a
broken-pipe diagnostic. A helper-container failure still returns non-zero and
prints no partial completion script.

## Install

### fish

```fish
mkdir -p ~/.config/fish/completions
ai-memory completions fish > ~/.config/fish/completions/ai-memory.fish
```

Fish loads that path lazily on first use — no shell restart, no `config.fish`
edit.

### zsh

```zsh
mkdir -p ~/.zfunc
ai-memory completions zsh > ~/.zfunc/_ai-memory
```

`~/.zfunc` must be on `$fpath` before `compinit` runs. If it is not already,
add this to `~/.zshrc` above the `compinit` call:

```zsh
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```

Zsh caches completion metadata, so run `rm -f ~/.zcompdump` and start a new
shell if a freshly regenerated script does not take effect.

### bash

Requires [bash-completion](https://github.com/scop/bash-completion).

```bash
mkdir -p ~/.local/share/bash-completion/completions
ai-memory completions bash > ~/.local/share/bash-completion/completions/ai-memory
```

Start a new shell to pick it up. To load it for the current shell only:

```bash
source <(ai-memory completions bash)
```

### PowerShell

```powershell
ai-memory completions powershell | Out-String | Invoke-Expression
```

To persist the generated script, keep it beside your PowerShell profile and
dot-source it from `$PROFILE` once:

```powershell
$completionDir = Join-Path (Split-Path -Parent $PROFILE) "completions"
$completionPath = Join-Path $completionDir "ai-memory.ps1"
New-Item -ItemType Directory -Force $completionDir | Out-Null
ai-memory completions powershell | Set-Content -Encoding utf8 $completionPath
Add-Content -Path $PROFILE -Value ". '$completionPath'"
```

After upgrades, regenerate `$completionPath`; the profile line does not need to
be added again.

### elvish

```elvish
mkdir -p ~/.config/elvish/lib
ai-memory completions elvish > ~/.config/elvish/lib/ai-memory.elv
```

Then add `use ai-memory` to `~/.config/elvish/rc.elv`.

## Upgrades

The script is a snapshot of the command tree at the moment it was generated.
Re-run the same command after upgrading `ai-memory` so completions pick up new
subcommands and flags. Nothing is checked into the repository, precisely so a
stale script cannot ship alongside a newer binary.

Docker users can generate a script without a local install:

```bash
docker run --rm akitaonrails/ai-memory:latest completions fish \
  > ~/.config/fish/completions/ai-memory.fish
```
