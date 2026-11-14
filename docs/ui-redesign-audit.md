# Web UI redesign audit: Memory Archive

Audit of the built-in web interface (`/web`, plus the optional `/manage`
console) before moving it onto the Memory Archive identity of the
institutional site. Written before any interface file changed.

- Application: `crates/ai-memory-web` on `release/3.5` (`eec72287`).
- Reference: [`akitaonrails/ai-memory-web`](https://github.com/akitaonrails/ai-memory-web)
  at `39f30475` (4 November 2026), read from `src/styles/global.css`,
  `docs/design-system.md`, `docs/color-study.md`, `src/components/` and
  `docs/research/2026-10-15-memory-archive-implementation.md`.

## 1. Current frontend architecture

The interface is server-rendered. There is no SPA and no JavaScript build.

| Layer | Where | Notes |
|---|---|---|
| Routes | `src/routes/*.rs`, mounted by `src/mount.rs` | Axum handlers render askama templates. Static assets are public (`/static/*`). |
| Templates | `templates/*.html` (32 files incl. partials), `templates/management/*.html` (3) | askama, compiled into the binary. `base.html` is the shell for every `/web` page. |
| Stylesheet | `static/input.css` -> `static/tailwind.css` | Tailwind CSS 3.4.17, standalone CLI pinned and checksum-verified in `build.rs`. The compiled file is vendored; `TAILWIND_BUILD=1 cargo build -p ai-memory-web` regenerates it and CI fails when it is stale. `tailwind.config.js` scans `templates/**/*.html`, `darkMode: 'class'`, no theme extension. |
| Scripts | Inline `<script>` blocks in templates; `static/management/theme.js` | Progressive enhancement only: theme, navigation drawer, tab switching, filters, usage refresh, inbox actions, pending decisions, session scrubber. |
| Fonts | `static/management/*.woff2` | Instrument Sans, Bricolage Grotesque, Commit Mono (Latin subsets, OFL). Served by a fixed match table in `src/routes/statics.rs` (`/web/static/{font}`) and again in `src/management/mod.rs` (`/manage/static/{asset}`). |
| Logo | `docs/logo.png` embedded by `statics.rs` | Same brain artwork as the site. |
| Markdown | `src/markdown.rs` (pulldown-cmark 0.12) | Tables, footnotes, strikethrough, task lists, smart punctuation. No syntax highlighting. Search snippets arrive as escaped text with `<mark>` highlights. |
| Graph | `templates/graph.html` | Server-computed layout drawn as inline SVG: lines, circles, text, one link per node. No zoom, pan, legend or client script. |

## 2. Technologies

Rust (askama, axum), Tailwind CSS 3.4 utilities plus hand-written component
CSS, vanilla inline JavaScript, self-hosted WOFF2 fonts. AGENTS.md forbids a
Node toolchain, bundler or framework in anything the server ships; the
Tailwind CLI is the one pinned exception.

## 3. Existing components

Two styling systems live side by side.

**Semantic component classes** in `input.css`, token-driven (`--dash-*`):
`app-sidebar`, `app-brand`, `app-nav`, `nav-label`, `nav-bottom`,
`app-topbar`, `topbar-search`, `theme-switch`, `language-select`,
`app-content`, `app-footer`, `page-heading`, `eyebrow`, `breadcrumbs`,
`metric-grid`/`metric-card`, `section-block`/`section-heading`, `surface`,
`panel-heading`, `subtle-pill`/`status-pill`, `primary-button`,
`secondary-button`, `quiet-button`, `project-toolbar`, `project-card`,
`project-navigation`/`project-tabs`, `agent-card`, `coverage-bar`,
`empty-state`, `ops-table`, `usage-panel`/`usage-table`, `inbox-layout`,
`mail-row`, `composer`, `field-label`/`field-input`, `segmented-tabs`,
`knowledge-layout`, `docs-*`, `markdown-body`, `insight-banner`,
`capture-health`, `knowledge-evidence`.

Used by: `base.html`, `projects.html`, `project.html`, `agent.html`,
`inbox.html`, `_agent_cards.html`, `_usage_panel.html`, `_knowledge.html`,
`documentation_*.html`.

**Raw Tailwind utilities with hard-coded palette colors** and `dark:` pairs:
893 color utilities (`slate`, `blue`, `amber`, `emerald`, `green`, `red`,
`rose`, `yellow`) across 17 templates: `project.html` (126), `pending.html`
(108), `page.html` (94), `search.html` (89), `history.html` (81),
`session.html` (70), `retrievals.html`, `workstreams.html` (60),
`projects.html` (56), `graph.html` (53), `namespace.html` (42),
`change_password.html` (36), `login.html` (27), `health.html` (26),
`not_found.html`, `admin_required.html`, `_header_search_scoped.html`.
These pages re-implement badges, notices, buttons, inputs and section labels
inline, each a slightly different copy. `page.html` styles rendered Markdown
with a 20-line arbitrary-variant class list (`[&_h1]:text-3xl ...`) that
duplicates `.markdown-body` from the documentation pages.

`graph.html` hard-codes the node fill (`#2563eb`). `base.html` hard-codes
`theme-color` (`#0f766e`). `pending.html` toggles `bg-green-50`/`bg-red-50`
from JavaScript.

**Management console** (`management-ui` feature): its own `.management`
scope with `--mg-*` tokens written with `light-dark()`, the same three
fonts, and a CSP of `style-src 'self'`, so no inline styles.

**Navigation**: fixed 228px sidebar (brand, three labelled groups, inbox
notifications, a "context that stays" footer) that becomes an off-canvas
drawer under 960px; sticky top bar with breadcrumb caption, search, log out,
language and theme switch. Icons are Unicode symbols (`▦ ⑂ ⌕ ⌘ ☷ ⌁ ▤ ↻`)
that need a platform symbol font (#2003).

**Themes**: `templates/_theme.html` runs in `<head>` before the stylesheet,
reads `localStorage["ai-memory-management-theme"]`, sets `data-theme`
(`light|dark|system`), toggles `html.dark` and `color-scheme`, follows
`prefers-color-scheme` while on System and syncs across tabs. No flash of the
wrong theme. The console uses `theme.js` with the same storage key.

**Responsiveness**: breakpoints at 640, 700, 960, 1080, 1250, 1400 and
1700px in `input.css`; Tailwind `sm`/`lg` in utility templates. Tables
scroll inside wrappers; metric grids collapse to two columns on phones.

## 4. Files that carry the visual identity

`static/input.css`, `tailwind.config.js`, `static/tailwind.css` (generated),
`templates/base.html`, `templates/_theme.html`, every template listed in
section 3, `templates/management/base.html`, `static/management/*.woff2` and
`font-licenses.txt`, the font tables in `src/routes/statics.rs` and
`src/management/mod.rs`.

## 5. The official design system found

All values below are read from `src/styles/global.css`, which
`docs/design-system.md` names as the home of the colors.

**Palette.** Light ("mineral paper") / dark ("graphite"):

| Token | Light | Dark | Role |
|---|---|---|---|
| `--bg` | `#e4e7e3` | `#11181b` | page ground |
| `--surface` | `#f0f1ec` | `#192226` | cards, tables, inputs |
| `--tint` | `#d6ded9` | `#202d30` | alternate ground, table heads, inline code |
| `--line` | `#a7b3b3` | `#435054` | borders, rules |
| `--text` | `#202e33` | `#e5e9e8` | body text |
| `--muted` | `#45565b` | `#b4bfc0` | secondary text |
| `--green` = `--teal` = `--azure` | `#28685f` | `#7cd9c6` | capture, inputs, links |
| `--violet` | `#685184` | `#c3a2ed` | recall, agents, history |
| `--rose` | `#774b50` | `#dfbfc3` | security |
| `--amber-ink` | `#795034` | `#dbad82` | memory, actions (text) |
| `--focus` | `#795034` | `#edc49e` | focus outline |
| `--green-soft` | `#dae6e3` | `#203b35` | fill behind teal |
| `--teal-soft` = `--azure-soft` | `#d6ded9` | `#202d30` | |
| `--violet-soft` | `#dfe2e8` | `#30273f` | |
| `--rose-soft` | `#e9dfe0` | `#40323c` | |
| `--amber-soft` | `#ecdfd2` | `#40372e` | |
| `--shadow` | `0 8px 24px #202e3314` | `0 8px 24px #00000026` | popovers only |

Theme-invariant (technical panels, primary action): `--amber #dbad82`,
`--amber-hi #edc49e`, `--on-amber #18242a`, `--ink #10191e`,
`--panel-text #f0f4f3`, `--panel-muted #b4bfc0`, `--panel-line #72878d`,
`--panel-surface #26383f`, `--capture #7cd9c6`, `--recall #c3a2ed`, vivid
`--v-azure #92bccb`, `--v-violet #bd99e8`, `--v-rose #bd969d`.

`docs/color-study.md` is out of date: its OKLCH indigo palette and contrast
tables predate the Memory Archive refresh of 15 October 2026 and no longer
match `global.css`. Its rules still apply (three tiers per hue, amber reserved
for the primary action, one hue per subject, contrast targets of 7:1 for body
text and 4.5:1 for accents). The redesign takes values from `global.css` only.

**Type.** Chakra Petch 500/600 for headings and short labels, IBM Plex Sans
400/500/600 for reading and controls, JetBrains Mono 400/500 for code and
technical labels; all `@fontsource` 5.3.0, Latin. Headings weight 500,
line-height 1.04 (h3/h4 1.15), tracking -0.012em, `text-wrap: balance`. The
site's floor is 18px body, 16px controls, 14px labels.

**Geometry.** 2px radius on cards, chips, buttons and tabs ("squared
controls share the device-panel geometry"); 1px `--line` borders; 1.5px
borders on ghost buttons and pills; no shadow on cards.

**Components.** `.btn-primary` (amber ground, `--on-amber` text, `--amber-hi`
hover, `scale(.97)` on press), `.btn-ghost` (1.5px line border, text border on
hover), `.card`, `.badge` + `data-hue` (soft fill, ink text, vivid dot),
`.kicker` (mono label after a 1.75rem x 3px vivid bar), `.pick` (toggle with
`aria-pressed`/`aria-selected`), `.table-wrap`/`.table-site`, `Callout`
(4px inline-start rule in the vivid hue on the soft fill), `CodeBlock` (dark
`--ink` terminal in both themes), `.signal-panel` (dark ground with panel
tokens in both themes), `CircuitIcon` (120px grid, 2.5 stroke, square caps),
the capture-to-recall 3px rule under page heroes, focus outline 2px
`--focus` with 3px offset, amber `::selection`.

**Principles.** Graphite and smoked glass, copper conduits, open circuit
traces; no neon, CRT effects, ambient glows or rainbow headlines; open
layouts over boxes; cards are for things you click; amber is reserved for
the primary action.

**Breakpoints.** Tailwind defaults plus 600, 700, 767 and 900px. Mobile
first; tested at 360px with no horizontal page scroll.

## 6. Differences between the app and the reference

| Aspect | App today | Memory Archive |
|---|---|---|
| Neutrals | Cool blue-grey (`#f5f7fa`/`#101720`) and Tailwind slate | Mineral paper / graphite |
| Accent | Teal `#0c786a` plus Tailwind blue `#2563eb` for links, buttons, graph | Teal `#28685f`/`#7cd9c6` for capture and links, violet for recall and agents, copper for memory and actions |
| Primary button | Teal or blue fill, white text | Amber fill, `--on-amber` text |
| Fonts | Instrument Sans, Bricolage Grotesque, Commit Mono | IBM Plex Sans, Chakra Petch, JetBrains Mono |
| Radius | 7-12px | 2px |
| Card shadow | `0 4px 24px -16px` on every card | Border only |
| Small text | 9-11px labels throughout | 14px floor |
| Section labels | Uppercase, letter-spaced sans | Mono kicker with a hue bar |
| Code blocks | Tinted box | Dark terminal in both themes |
| Status | Tailwind amber/emerald/red | Subject hues, soft tier fill |
| Icons | Unicode symbols needing a symbol font | Inline SVG line icons |
| Dark mode | Two parallel systems (`--dash-*` tokens and `dark:` utilities) | One token set that flips |

## 7. Regression risks

1. **Tests that read markup or CSS.** `localization.rs` asserts the
   `.language-select` rule uses `--dash-line`, `--dash-soft`, `--dash-text`
   and `--dash-accent`; `management.rs` requests the three current font
   files; `statics.rs` checks that stacks starting with `Management Sans` or
   `Management Display` reach a symbol face; `history_replay.rs` matches a
   link's full class string. Others match structural classes (`project-panel`,
   `mail-row`, `usage-results`, `usage-note`, `send-button`). Mitigation: keep
   `--dash-*` as the app's semantic token names, keep every structural class,
   and update the four assertions deliberately.
2. **Cascade.** `input.css` component rules sit after `@tailwind utilities`,
   so they beat utilities of equal specificity. New shared helpers go in
   `@layer components` so per-element utilities can still override them.
3. **Purge.** Tailwind only emits utilities it finds in `templates/`. Classes
   set from JavaScript must appear literally in a template.
4. **JavaScript hooks.** Scripts select by id, `data-*` and structural classes
   (`.pending-item`, `.mail-row`, `.status-pill`, `.usage-scroll`,
   `#token-usage .usage-metrics`). Those stay unchanged.
5. **Font swap.** The faces are embedded and served by a fixed match table.
   Changing them is a Rust change (see section 9).
6. **Symbol glyphs.** Unicode symbols that stay in content (arrows, `↗`, `·`)
   still need the symbol fallback stacks from #2003.
7. **Management CSP.** `style-src 'self'` forbids inline styles there.
8. **Localization.** Uppercase eyebrow strings live in the Fluent catalogs;
   copy stays as is.

## 8. Implementation strategy

1. **Tokens.** Add the official palette to `input.css` as primitives named
   after the site's tokens (`--ma-bg`, `--ma-surface`, ... `--ma-panel-*`),
   switched to dark by `html.dark`, `[data-theme="dark"]`, or the system
   preference when no light choice is stored. Point the app's existing
   semantic tokens (`--dash-*`) and the console's `--mg-*` at them, so
   component CSS keeps its names and both shells share one palette.
2. **Tailwind theme.** Extend `tailwind.config.js` with the same tokens as
   colors (`bg`, `surface`, `tint`, `line`, `text`, `muted`, `teal`,
   `violet`, `rose`, `amber-ink` and soft tiers, `amber`, `on-amber`, `ink`),
   the three font families, the 2px radius and a dashboard type scale. This
   mirrors the site's `@theme inline` block.
3. **Templates.** Replace hard-coded palette utilities with token utilities
   and drop the now redundant `dark:` color pairs. Repeated inline patterns
   move to shared classes taken from the site: `badge` + `data-hue`,
   `callout` + `data-hue`, `primary-button` / `secondary-button` /
   `danger-button`, `field-input` / `field-control`, `section-label`.
   `page.html` uses `.markdown-body`. Markup structure, ids, `data-*`
   attributes, routes and scripts stay as they are.
4. **Shell.** Restyle sidebar (dark signal panel in both themes, SVG line
   icons drawn on the CircuitIcon grid), top bar, breadcrumbs, footer, theme
   switch, page headings (kicker eyebrow and the capture-to-recall rule).
5. **Content.** Reading styles for Markdown (Chakra headings, Plex body, dark
   terminal code blocks, site tables, task lists, `mark`), graph colors from
   tokens, search highlights, status badges, diff colors.
6. **Fonts.** Vendor the seven `@fontsource` 5.3.0 Latin WOFF2 files and
   their OFL notices, serve them from one shared table, remove the old faces.
7. **Verification.** `cargo fmt`, `clippy -D warnings`, the full test tier,
   a contrast check of every pairing used, browser screenshots in both themes
   at desktop and phone widths, before and after.

## 9. Files to be modified

- `crates/ai-memory-web/static/input.css`, `tailwind.config.js`,
  `static/tailwind.css` (regenerated).
- `crates/ai-memory-web/templates/*.html` and `templates/management/base.html`.
- `crates/ai-memory-web/static/fonts/` (new WOFF2 files and licenses),
  removal of the three old faces in `static/management/`.
- `crates/ai-memory-web/src/routes/statics.rs` and
  `src/management/mod.rs`: the font lookup only. This is the one Rust change.
  It is required, not cosmetic: the binary must stay self-contained and
  offline, the site itself rules out a font CDN, and the only way a font
  reaches the browser is through this embedded table. One shared function
  replaces the two copies. No route, handler, API or auth path changes.
- Tests that assert font names or token names (section 7, item 1).
- `docs/ui-design-system.md`, `docs/ui-redesign-implementation.md`,
  `CHANGELOG.md` (`Changed`).

## 10. Additional dependencies

None. The fonts are static files under the SIL Open Font License 1.1,
taken from the same `@fontsource` 5.3.0 packages the site locks in
`package-lock.json`. No syntax highlighter is added (that would be a new
dependency); code blocks get the terminal treatment without token colors.
