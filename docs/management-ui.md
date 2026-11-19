# Management console

The optional console at `/manage` is server-rendered by askama in
`ai-memory-web`, using the existing vendored Tailwind stylesheet. Ordinary
Cargo builds require no Node, npm, bundler, or JavaScript framework. A small
hash-authorized inline script enhances theme selection and password validation;
login, account management, and session/key operations also work without JavaScript.

## Enable it

```bash
cargo build --locked --release -p ai-memory-cli --features management-ui
./target/release/ai-memory serve --transport http --enable-management-ui
```

The runtime setting is `AI_MEMORY_ENABLE_MANAGEMENT_UI=true`. Both build and
runtime switches default to off. Without either, the console is absent. The UI
build includes the independently reviewable `session-management` and
`self-service-keys` features; enabling the UI also enables their HTTP routes.
They can instead be built and enabled individually without a UI; see
[session management](session-management.md) and [self-service keys](self-service-keys.md).

With `AI_MEMORY_BASE_PATH=/memory`, open `/memory/manage`. No `--enable-web` or
`--enable-api` is required. If the wiki browser is also enabled, its `--web-slug`
must differ from `/manage`.

For Docker source builds, pass `--build-arg AI_MEMORY_FEATURES=management-ui` and
set `AI_MEMORY_ENABLE_MANAGEMENT_UI=true` when running the image. The standard
release image does not include the optional console.

## Authentication and deployment

Use the existing human-password bootstrap/recovery described in
[installation](install.md#human-password-bootstrap-and-recovery). The console
reuses Argon2id verification, browser sessions, CSRF hashes, native credential
hashing, capability gates, and the same SQLite database. It adds no identity
provider, credential format, authentication implementation, or database schema.

The existing `root` role is labeled Admin and `user` is labeled User. Admins
create users, change roles, reset passwords, disable/enable accounts, issue and
revoke keys, and inspect or purge any session. Users inspect their own and shared
legacy sessions, purge exactly owned sessions with current project write access,
change passwords, and manage their own keys. The last-enabled-root safeguard
remains in force. New users must replace the temporary password before using
the console. The password form requires 12 Unicode code points; the engine's
underlying UTF-8 byte policy is unchanged.

Browser credentials cannot authenticate MCP or hooks. Use a personal `aim_` key
from MCP connection. Native keys remain User-level even for an Admin owner.
Disabling an account removes browser access but leaves existing machine keys
active until explicitly revoked, as with the existing CLI.

Use the existing [HTTPS reverse-proxy setup](https-via-proxy.md), secure cookies,
and `AI_MEMORY_ALLOWED_HOSTS`. A VPN is optional. Forward the original `Host`
header: plain POST forms require an HTTP(S) Origin with the same authority and
accept only `Sec-Fetch-Site: same-origin` or `none` when that browser metadata
is present; `same-site`, `cross-site`, malformed, and mismatched-origin
requests are refused. This preserves the login-CSRF barrier when
converting HTML form data to the existing JSON authentication handler. Mutation
forms also carry the existing CSRF token. Machine bearers cannot use the console.

Templates autoescape stored text. Forms select from fixed operations and typed
session/key ids, and delegate in-process to the same authenticated engine
routers. Only transport peer identity is forwarded in request extensions, so
login rate limits retain the real peer and stale route/auth metadata cannot
change the delegated request. No localhost HTTP proxy is created. Every console
response denies framing and uses `private, no-store`; the inline enhancement has
a CSP SHA-256 hash. Passwords and new keys never appear in URLs or browser storage.

## Session deletion

Deletion first previews counts and then requires an explicit confirmation.
Previews have no file, tombstone, audit, or admission effects. Confirmation uses
the existing wiki mutation guard, admission webhooks, pre/post git checkpoints,
and the single-writer transaction. Ownership and current write access are
rechecked in that transaction. Shared ownerless sessions and cross-project
collateral require an Admin; only an Admin receives their collateral preview.
Both collateral observations and handoffs appear in the preview.

Purging removes captured observations, authored handoffs, generated session-page
versions, and review records. Maintained knowledge and later manual page versions
remain, as do handoffs accepted by the session but authored elsewhere. Git
history, backups, and native transcripts may retain earlier content. A partial
file cleanup cannot roll back the database purge; the UI reports it and replay
and reindex respect the purge tombstone.

## Interface and development

The console shares the web interface's Memory Archive palette and type
([`ui-design-system.md`](ui-design-system.md)): mineral-paper and graphite
surfaces, amber primary actions and a copper focus outline. System is the
default theme; explicit Light/Dark preferences persist locally. The fonts are
embedded from `static/fonts/` and served from one table shared with `/web`;
their license notice is served at `static/font-licenses.txt` by the console.

Templates live in `templates/management/`. `src/management/views.rs` builds
escaped view models, `forms.rs` maps plain POST operations, and `backend.rs`
delegates to engine routers. The normal wiki browser remains read-only. Session
and event pagination uses the page size returned by the engine API.

```bash
TAILWIND_BUILD=1 cargo build -p ai-memory-web --features management-ui
cargo fmt --all -- --check
git diff --check
cargo clippy --workspace --all-targets --features ai-memory-cli/management-ui -- -D warnings
cargo tf --features ai-memory-cli/management-ui
```

Commit regenerated `static/tailwind.css` when templates or styles change.
The pinned standalone Tailwind CLI is the project's existing regeneration
exception; normal builds copy the vendored CSS and need no downloads.

Rust integration tests exercise real human sessions, cookie forwarding, same-
origin plain login, CSRF refusal, pending-password gates, foreign sessions/scopes/
credentials, HTML escaping, typed action ids, admission rejection, owned-purge
cleanup/recovery, optional assets, and subpath mounting. The same tests are run
in the feature-enabled CI job; no JavaScript toolchain is required there.
