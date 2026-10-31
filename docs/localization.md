# Interface languages

The built-in web interface and human CLI output support English (`en`) and
Brazilian Portuguese (`pt-BR`). English remains the CLI default.

```sh
ai-memory --language pt-BR status
ai-memory --language pt-BR --help
AI_MEMORY_LANGUAGE=pt-BR ai-memory doctor
```

For a persistent terminal preference, set the top-level `language = "pt-BR"`
in `config.toml`. Precedence is `--language`, then `AI_MEMORY_LANGUAGE`, then
the config file, then English. Help and argument-validation errors work before
configuration is loaded, so they use the flag or environment preference.
A `--language` after the harness in `ai-memory run` belongs to that harness,
just like its other native arguments.

The web interface negotiates `Accept-Language` on each request. The language
selector saves an `ai_memory_language` preference cookie scoped to the
installation's URL prefix. It takes precedence over the browser header. Two
operators can use different languages on the same server. HTML responses carry
`Content-Language` and vary by language header and cookie; the JSON API does not.
The one-shot `setup --web` wizard uses the launching CLI's language and passes
it to its setup child.

## Translation boundary

Localize application-owned interface text, accessible labels, human reports,
confirmation questions, help, and explanatory error messages. Keep command
names, flags, identifiers, paths and copied commands intact. Confirmation
prompts in Portuguese accept `s`/`sim` and `y`/`yes`. A prompt that must
recognize a refusal, such as the `--yolo` warning, also declines `não`/`nao`
next to `n`/`no`.

Do not translate JSON keys or values, MCP payloads, stored pages, conversation
content, generated agent instructions, generated configuration files, log event
identities, or diagnostic details supplied by an OS, provider or third-party
tool. Those remain usable by automation and retain their original evidence.
Known fixed authentication errors are localized at the display boundary; their
wire representation stays unchanged. Unknown diagnostic text is preserved.
Bundled product guides and release notes keep their source language.

### Known gaps

These strings still appear in English under `pt-BR`:

- Text assembled at runtime and passed along by value, so it never reaches the
  catalog as a known template. The `--jail` summary and the web's invalid-URL
  errors, such as `invalid session id`, are examples. The drift test only sees
  format-string literals, so it does not catch this kind of message.
- Log output (`tracing`), by design.

## Catalogs and ownership

Fluent resources live under `crates/ai-memory-core/locales/<language>/`:

- `common.ftl`: elapsed times, status labels and fixed protocol errors.
- `cli.ftl`: human command output and interactive prompts.
- `cli-help.ftl`: clap command and argument descriptions.
- `web.ftl`: HTML, progressive enhancement and setup wizard text.
- `diagnostics.ftl`: CLI errors and status lines whose source stays in English,
  plus fixed web labels defined in Rust, such as feature states and guide titles.

Resources are embedded with `include_str!`. Cargo builds the complete binary;
there is no runtime catalog download, JavaScript package or additional build
step. The core `Localizer` owns concurrent Fluent bundles behind `Arc`, so
cloning a context is cheap. Configuration resolves the CLI context once;
web middleware selects a request-local context from router-owned catalogs.
There is no mutable global language.

The English catalog is the reference and the fallback. Keep message IDs stable
when editing wording. Use descriptive names for new messages and named
parameters for values that translators may need to reorder:

```rust
let text = i18n.format("cli-http-server-response", &[
    ("method", method.to_string()),
    ("path", path.to_owned()),
    ("status", status.to_string()),
    ("detail", detail.to_owned()),
]);
```

CLI errors and status lines deep in a command keep their English format string
in the source, where tests and logs rely on it. `Localizer::diagnostic` recognizes
the rendered text at the terminal boundary and returns the translation, keeping
the values it captured. The failure printed when a command exits, and each cause
in its chain, go through it automatically. Text it does not recognize stays as
written, so OS, library and provider details are preserved. Each English value
in `diagnostics.ftl` must equal the source format string. Adding a `bail!`,
`anyhow!`, `.context` or `diagnostic` message means adding it to both catalogs;
the `every_cli_diagnostic_has_a_catalog_entry` test lists anything missing.
Web templates use `label`, the same lookup, for fixed text that Rust passes to them.
Hook commands run before configuration loads, so they follow only
`AI_MEMORY_LANGUAGE`. Commented configuration snippets printed for pasting stay
in English, like other generated configuration.

Use `message` for typed Fluent values and `count` for numeric plural selectors.
Translate the complete inflected phrase; do not concatenate English suffixes.
`one`, `two` and `three` are positional conveniences for askama, whose pinned
version does not support constructing argument arrays in templates. Their
parameters are `value0`, `value1` and `value2`. Keep a sentence in one message
with its values as parameters; do not assemble it from fragments in a template.

Askama escapes ordinary translated text and substitutions. Never mark a
translation as safe HTML, with one exception: `rich` and `rich_one` escape the
whole message first and then turn its `` `backtick` `` spans into `<code>`, so a
sentence can carry a code token and their output may use the `safe` filter.
Inside a script, use `js`, `js_one`, `js_two`, or `errors_js`: they produce JSON
and escape HTML script delimiters and Unicode line separators. Only these
already-encoded values may use askama's `safe` filter. Progressive enhancement
must use `textContent` for displayed values.

## Adding a language or message

Copy all five English resources into a new BCP 47 language directory, translate
them, then register the tag, accepted aliases and embedded resources in
`ai-memory-core::i18n`. Add the language to the browser selector. Preserve the
parameters in each message; use Fluent selectors for the language's plural
forms. Keep technical tokens inside translated prose unchanged.

A new message belongs in every shipped language in the same change. Help IDs
follow the command/argument path, such as `cli-help-root-status-json-help`.
The CLI metadata test compares the reference catalog with clap, so editing a
help description or adding a flag also requires updating its catalog entries.
The resources are ordinary UTF-8 text; contributors can edit them directly.

## Validation

Run the repository's normal formatting, clippy, test and dependency gates.
Focused checks during translation work are:

```sh
cargo test -p ai-memory-core i18n::tests
cargo test -p ai-memory-cli --lib localization::tests
cargo test -p ai-memory-web localization --all-features
```

Tests validate Fluent syntax, duplicate IDs, language parity, parameter parity,
CLI help drift, CLI diagnostic coverage, language negotiation and isolation,
script escaping, unchanged API responses, and the administrative gate with
Portuguese HTML. When changing
browser messages, inspect rendered pages in both languages and check keyboard
navigation, labels, long text and the small-screen layout.
