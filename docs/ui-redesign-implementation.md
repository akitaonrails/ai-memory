# Web UI redesign: implementation record

What changed when the built-in web interface and the management console moved
onto the Memory Archive identity. The audit that preceded it is
[`ui-redesign-audit.md`](ui-redesign-audit.md); the resulting rules are in
[`ui-design-system.md`](ui-design-system.md).

## Summary

- One token set taken from the institutional site
  (`akitaonrails/ai-memory-web` `src/styles/global.css` at `39f30475`) drives
  both themes, the dashboard's existing `--dash-*` names and the console's
  `--mg-*` names.
- Tailwind's stock palette is replaced by those tokens. 893 hard-coded color
  utilities and their `dark:` pairs left the templates; repeated inline
  patterns became shared classes from the site (`badge`, `callout`,
  kicker-style `eyebrow`, `section-label`, ghost and primary buttons,
  terminal blocks).
- Chakra Petch, IBM Plex Sans and JetBrains Mono replace Instrument Sans,
  Bricolage Grotesque and Commit Mono.
- The sidebar, code blocks, CLI blocks and the knowledge graph are dark
  technical panels in both themes. Navigation and cards use SVG line icons on
  the site's `CircuitIcon` grid instead of Unicode symbols.
- Squared 2px geometry, no card shadows, amber primary actions, copper focus
  outline, a 12px text floor (it was 9-11px in places).
- Routes, APIs, handlers, authentication, scripts' behavior, markup structure,
  ids and `data-*` hooks are unchanged. The theme mechanism is the existing
  one.

## Files

### Styling

| File | Change |
|---|---|
| `crates/ai-memory-web/static/input.css` | Rewritten: `@font-face` for the seven faces; `--ma-*` primitives for light, dark and system dark; `--dash-*`/`--mg-*` as semantic aliases; `data-hue` API; shared components in `@layer components`; every existing component restyled with its selectors kept; reading styles for Markdown; graph, diff and dialog classes; loading state; reduced-motion and coarse-pointer rules. The `.bg-white`/`.text-slate-*` override hacks are gone. |
| `crates/ai-memory-web/tailwind.config.js` | Colors replaced by tokens, font families, 2px radius, 13px `text-xs`. |
| `crates/ai-memory-web/static/tailwind.css` | Regenerated with `TAILWIND_BUILD=1`. |
| `crates/ai-memory-web/static/fonts/*` | New: seven WOFF2 files and `font-licenses.txt`. |
| `crates/ai-memory-web/static/management/*.woff2`, `font-licenses.txt` | Removed (replaced by `static/fonts/`). |

### Templates

| File | Change |
|---|---|
| `base.html` | SVG icons in the sidebar, menu button and search; `theme-color` per scheme; chip icon in the sidebar footer. |
| `_icon_*.html` (12) | `_icon_envelope.html` redrawn; eleven new icons. |
| `projects.html`, `project.html`, `_agent_cards.html`, `_usage_panel.html`, `inbox.html` | Token utilities, badges with hues (agents violet), callout, icons in metrics and empty states, dialog classes. `agent.html`, `documentation_*.html` and `management/*.html` already used component classes and changed only through the stylesheet. |
| `page.html` | The 20-line arbitrary-variant Markdown class list became `markdown-body`, shared with the documentation pages. |
| `graph.html` | Node, edge and label colors moved from SVG attributes (`fill="#2563eb"`) to classes; header search uses the shared top-bar style. |
| `search.html`, `namespace.html`, `session.html`, `retrievals.html`, `history.html`, `health.html`, `pending.html`, `workstreams.html` | Token utilities, `breadcrumbs`, `badge`, `callout`, `section-label`, `field-control`, button classes; terminal blocks for CLI commands; observation focus style. |
| `login.html`, `change_password.html`, `not_found.html`, `admin_required.html` | Token utilities, button and field classes, rose callout for auth errors. |
| `_header_search_scoped.html` | Utility classes removed; styled by `.topbar-search`. |

### Rust

| File | Change |
|---|---|
| `src/routes/statics.rs` | `font_file()` holds the seven faces; `GET /static/{font}` uses it. Test for #2003 now reads `--ma-font-sans` and `--ma-font-display`; new `every_stylesheet_font_is_embedded`. |
| `src/management/mod.rs` | The console's asset route takes fonts from `font_file()` instead of its own copy of the table. |

These two files are the only Rust changes. The fonts have to be embedded to
keep the binary self-contained and offline, and this match table is the only
way a font reaches the browser. No route, handler, API or auth path changed.

### Tests

| File | Change |
|---|---|
| `tests/suite/design_tokens.rs` | New: templates may not use stock Tailwind shades, `dark:` color pairs or literal colors; a third test checks the detector. |
| `tests/suite/management.rs` | Requests the new font files; a retired font name must 404. |
| `tests/suite/history_replay.rs`, `tests/suite/routes.rs` | Two assertions on a full class string updated to the token classes; what they check (the History link, the wide-screen outline heading) is unchanged. |
| `tests/suite/mod.rs` | Declares `design_tokens`. |

### Documentation

`docs/ui-redesign-audit.md`, `docs/ui-design-system.md`, this file,
`docs/management-ui.md` (fonts moved), `docs/security-boundaries.md` row 24
(shared font table), `CHANGELOG.md` (`Changed`).

## Dependencies

None added or removed. The three font families are static files under the SIL
Open Font License 1.1 from `@fontsource/chakra-petch`, `@fontsource/ibm-plex-sans`
and `@fontsource/jetbrains-mono` 5.3.0, the versions the site locks; each
tarball's SHA-512 matched the integrity recorded in the site's
`package-lock.json`. The embedded fonts total 134 KB.

## Pre-existing defects fixed on the way

All visual, all found while testing:

- The token-usage search button used `button-secondary`, a class that does
  not exist, so it rendered unstyled.
- The token-usage call filter shrank to about 20px wide on every width.
- The project mailbox's "Open inbox" button had `float:right` inside a flex
  row, which does nothing.
- The session scrubber stuck at 56px and a wiki page's outline column at 16px,
  both under the 74px top bar.
- Message states `editing`, the inbox editor and its actions had no styles.

## Verification

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | pass |
| `git diff --check` | pass |
| `cargo test -p ai-memory-web --features management-ui` | 342 passed |
| `cargo test -p ai-memory-web` | 341 passed |
| `cargo test -p ai-memory-cli --features management-ui` | 2487 + 486 passed, 3 ignored (the crate that mounts the web UI) |
| `cargo clippy -p ai-memory-web --no-deps --all-targets -- -D warnings` | clean apart from `clippy::result_large_err`, see limitations |
| Contrast, every pairing in both themes | 59 pairings, 0 below target (table in `ui-design-system.md`) |
| Browser sweep (Chromium over CDP): 17 pages x 360/390/768/1440px x light/dark | 136 combinations: no horizontal overflow, fonts loaded, no text under 12px, no console errors except the expected 404 status on the missing-page check |
| Theme switch | Light and Dark persist across reloads; System follows a live OS change; the pressed state matches |
| Keyboard | skip link first; focus outline visible on every control reached |
| Mobile drawer | the menu button opens the sidebar; Escape closes it |
| Management console | login screen checked in both themes |
| Screenshots | before and after, every page, both themes, 1440px and 390px |

The browser checks ran against an isolated server (`--data-dir` in a temp
directory, port 49399) seeded with sessions through `/hook`, pages through
`write-page` and one message.

## Known limitations

- **No syntax highlighting.** Code blocks get the terminal treatment without
  token colors. Highlighting needs a dependency (a Rust highlighter or a
  JavaScript one), which this change does not add.
- **Graph interactions.** The graph is server-rendered SVG with no zoom, pan,
  legend or selection; this change styles what exists and adds none of them.
- **Local clippy.** This machine runs Rust 1.99, not the pinned 1.95. Its
  clippy reports `result_large_err` in 29 functions of `ai-memory-web` and
  `#[must_use]` findings in `ai-memory-llm`, none in code this change touched.
  CI's pinned toolchain is the reference.
- **Full tier.** `cargo tf` (nextest) is not installed here; the web and CLI
  crates' suites ran with `cargo test`.
- **Humanizer.** The maintainer-side humanizer pass on the new docs has not
  run.
- **CHANGELOG reference.** The entry ends with `(#TBD)` until a pull request
  number exists.

## Not changed, noticed while testing

- The documentation page shows two `h1` elements: the page heading and the
  guide's own `# Title` rendered from Markdown.
- The "New message" label starts with U+FF0B (fullwidth plus) from the
  catalog; without a CJK font installed it renders as a missing glyph. It did
  before this change too.
- On project sub-pages the sidebar marks both "Projects" and the section
  (graph) as current.
- The console's language select spans the header width, pushing the theme
  switch to a second line.

## Pending

- Assign the pull request number in `CHANGELOG.md`.
- Run the full CI matrix (`cargo tf`, Windows, macOS) on the pull request.
- Optional follow-ups: syntax highlighting, a graph legend, the four items
  above.
