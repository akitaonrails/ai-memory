# Web UI design system: Memory Archive

The built-in web interface (`/web`) and the optional management console
(`/manage`) share the visual identity of the institutional site,
[aimemory.sh](https://aimemory.sh). The site calls it **Memory Archive**:
graphite architecture, smoked glass and copper conduits around the original
brain logo, with restrained accents taken from the logo itself.

The site is the source of truth. Its tokens live in
[`src/styles/global.css`](https://github.com/akitaonrails/ai-memory-web/blob/main/src/styles/global.css)
and its rules in
[`docs/design-system.md`](https://github.com/akitaonrails/ai-memory-web/blob/main/docs/design-system.md).
This page records how the dashboard applies them and where it has to differ,
because a management dashboard is denser than a landing page. When a value
changes on the site, copy it into `crates/ai-memory-web/static/input.css`;
never pick a new value here first.

Values below were taken from `akitaonrails/ai-memory-web` at commit
`39f30475` (4 November 2026). That repository's `docs/color-study.md`
predates the Memory Archive refresh and its tables no longer match
`global.css`; `global.css` wins.

## Where things live

| What | File |
|---|---|
| Fonts, tokens, themes, components | `crates/ai-memory-web/static/input.css` |
| Tailwind theme (token colors, fonts, radius, type step) | `crates/ai-memory-web/tailwind.config.js` |
| Compiled stylesheet (vendored, regenerated) | `crates/ai-memory-web/static/tailwind.css` |
| Font files and their OFL notices | `crates/ai-memory-web/static/fonts/` |
| Icons | `crates/ai-memory-web/templates/_icon_*.html` |
| Theme script (runs before first paint) | `crates/ai-memory-web/templates/_theme.html`, `static/management/theme.js` |
| Guard against stock Tailwind colors | `crates/ai-memory-web/tests/suite/design_tokens.rs` |

Regenerate the stylesheet after changing any of them:

```bash
TAILWIND_BUILD=1 cargo build -p ai-memory-web
```

## Palette

Three layers, so a template never names a hex value.

1. **Primitives** (`--ma-*`): the site's tokens, same names with an `--ma-`
   prefix, defined for light on `:root` and for dark on `html.dark`,
   `[data-theme="dark"]` and the system dark preference.
2. **Semantic roles** (`--dash-*` for the dashboard, `--mg-*` for the
   console): the names component CSS already used, now pointing at
   primitives. Brand-only values (amber, ink, panel, capture, recall) are read
   from `--ma-*` directly.
3. **Tailwind colors**: `tailwind.config.js` replaces Tailwind's palette with
   the primitives, as the site's `@theme inline` block does. `text-muted`,
   `bg-tint`, `border-line`, `text-green`, `bg-amber-soft` and so on switch
   with the theme by themselves, so templates carry no `dark:` color pairs.

### Themed tokens

| Token | Light | Dark | Use |
|---|---|---|---|
| `--ma-bg` | `#e4e7e3` | `#11181b` | page ground (mineral paper / graphite) |
| `--ma-surface` | `#f0f1ec` | `#192226` | cards, panels, inputs, tables |
| `--ma-tint` | `#d6ded9` | `#202d30` | table heads, inline code, neutral chips, hover fills |
| `--ma-line` | `#a7b3b3` | `#435054` | borders, rules, disabled text |
| `--ma-text` | `#202e33` | `#e5e9e8` | body text |
| `--ma-muted` | `#45565b` | `#b4bfc0` | secondary text, placeholders |
| `--ma-green` (= `--ma-teal`, `--ma-azure`) | `#28685f` | `#7cd9c6` | links, capture, success |
| `--ma-violet` | `#685184` | `#c3a2ed` | agents, recall, editing |
| `--ma-rose` | `#774b50` | `#dfbfc3` | failures, destructive actions |
| `--ma-amber-ink` | `#795034` | `#dbad82` | memory, attention, eyebrows |
| `--ma-focus` | `#795034` | `#edc49e` | focus outline |
| `--ma-green-soft` | `#dae6e3` | `#203b35` | fill behind green |
| `--ma-teal-soft` (= `--ma-azure-soft`) | `#d6ded9` | `#202d30` | equals tint in Memory Archive |
| `--ma-violet-soft` | `#dfe2e8` | `#30273f` | fill behind violet |
| `--ma-rose-soft` | `#e9dfe0` | `#40323c` | fill behind rose |
| `--ma-amber-soft` | `#ecdfd2` | `#40372e` | fill behind amber, highlights |
| `--ma-shadow` | `0 8px 24px #202e3314` | `0 8px 24px #00000026` | modal dialogs only |

### Theme-invariant tokens

| Token | Value | Use |
|---|---|---|
| `--ma-amber` | `#dbad82` | primary button, current-item marker, kicker bar |
| `--ma-amber-hi` | `#edc49e` | primary hover, focus on dark panels |
| `--ma-on-amber` | `#18242a` | text on amber |
| `--ma-ink` | `#10191e` | ground of technical panels |
| `--ma-panel-text` / `-muted` / `-line` / `-surface` | `#f0f4f3` / `#b4bfc0` / `#72878d` / `#26383f` | text, labels, rules and raised items on panels |
| `--ma-capture` | `#7cd9c6` | capture rail, graph nodes, success rules |
| `--ma-recall` | `#c3a2ed` | recall rail |
| `--ma-v-green`, `-v-teal`, `-v-azure`, `-v-violet`, `-v-rose` | `#7cd9c6`, `#7cd9c6`, `#92bccb`, `#bd99e8`, `#bd969d` | vivid tier: rules and dots, never text |

### Subject hues

The site's `data-hue` API is available unchanged. Put `data-hue` on a
container and read `--hue` (text), `--hue-soft` (fill) and `--hue-vivid`
(rule or dot).

| Hue | Means here |
|---|---|
| `green` | knowledge, capture, success, links |
| `violet` | agents, recall, an edit in progress |
| `amber` | memory, actions, anything waiting for attention |
| `rose` | failures, rejection, security |
| `teal` | available; its soft tier equals tint |

### States the site does not define

The site has no success, warning, error, hover, active or disabled tokens.
None were invented; each maps onto an existing one.

| State | Treatment |
|---|---|
| Success | `green` hue: `--ma-green` on `--ma-green-soft`, `--ma-capture` rule |
| Warning, attention | `amber` hue: `--ma-amber-ink` on `--ma-amber-soft`, `--ma-amber` rule |
| Error | `rose` hue: `--ma-rose` on `--ma-rose-soft`, `--ma-v-rose` rule; invalid fields get a `--ma-rose` border after interaction (`:user-invalid`) |
| Info | neutral: `--ma-muted` on `--ma-tint` |
| Hover | ghost controls: border turns `--ma-text` (site `.btn-ghost`); clickable cards: border turns `--ma-amber-ink` (site `.directory-link`); rows and menu items: `--ma-tint` fill |
| Active (pressed) | `transform: scale(.97)` (site `.btn:active`); selected tab: `--ma-surface` on a `--ma-tint` track (site `Tabs`); current nav item: `--ma-panel-surface` with a 3px `--ma-amber` marker |
| Focus | 2px solid `--ma-focus`, 3px offset (site `:focus-visible`); `--ma-amber-hi` on dark panels |
| Disabled | 50% opacity, `not-allowed` cursor |
| Loading | a 2px amber sweep under the form while it is `aria-busy` (none under reduced motion) |

## Typography

| Family | Weights | Use |
|---|---|---|
| Chakra Petch | 500, 600 | headings, page titles, large numbers, the brand |
| IBM Plex Sans | 400, 500, 600 | reading text, navigation, controls |
| JetBrains Mono | 400, 500 | code, identifiers, paths, breadcrumbs, eyebrows, section labels |

All three are the `@fontsource` 5.3.0 Latin subsets the site locks, embedded
in the binary and served from `/web/static/` and `/manage/static/`. There is
no font CDN request. The stacks continue with `system-ui` and the platform
symbol faces, because the Latin subsets lack the arrows and symbols some text
uses (#2003).

The site sets 18px body, 16px controls and a 14px floor. A dashboard shows
far more per screen, so the scale is one step smaller:

| Role | Size |
|---|---|
| Page title (`h1`) | `clamp(1.75rem, 1.35rem + 1.3vw, 2.375rem)` |
| Section title (`h2`) | 1.375rem |
| Card title | 1.125rem |
| Large numbers | 1.875rem (Chakra Petch 600, tabular figures) |
| Reading text (Markdown) | 1rem, line-height 1.75, paragraphs capped at 46rem |
| Interface text | 0.9375rem, line-height 1.6 |
| Controls | 0.875rem |
| Metadata, captions (`text-xs`) | 0.8125rem |
| Uppercase mono micro-labels (sidebar groups, brand tag) | 0.75rem |

Nothing renders below 12px. Headings use weight 600 and -0.012em tracking
with balanced wrapping; the site uses 500 for long display headings, which
reads too light at dashboard sizes. Ligatures are off in code
(`font-variant-ligatures: none`): JetBrains Mono would otherwise draw `->` and
`!=` as single glyphs, and a memory tool shows commands that must read as
typed.

## Geometry

- Radius: 2px everywhere (`--ma-radius`, Tailwind `rounded`, `rounded-md` and
  `rounded-lg`). Round shapes are kept for dots only.
- Borders: 1px `--ma-line`; ghost buttons and toggles 1.5px.
- Shadows: none on cards. `--ma-shadow` appears only under the modal dialog.
- Rules: 3px colored rules mark a subject (`.eyebrow::before`, callouts,
  current items, cards' top edge). The top bar ends with the site's 2px
  capture-to-recall rule.
- Spacing: the 0.25rem scale both projects use through Tailwind;
  `--ma-space-1` to `--ma-space-7` name the steps hand-written CSS uses.

## Themes

Light, Dark and System, chosen in the top bar (and in the console header).

- `_theme.html` runs in `<head>` before the stylesheet. It reads
  `localStorage["ai-memory-management-theme"]`, sets `data-theme`
  (`light`, `dark` or `system`), toggles `html.dark` and sets `color-scheme`,
  so the first paint already has the right theme.
- System follows `prefers-color-scheme` live and is the default. Choosing
  System removes the stored value. A change in another tab applies here too.
- The console's `theme.js` uses the same key and sets `data-theme`.
- Without JavaScript the media query alone picks the theme.
- `<meta name="theme-color">` gives the browser chrome `--ma-bg` for each
  scheme.

Technical panels keep the dark ink ground in both themes, as on the site:
the sidebar, Markdown code blocks, CLI command blocks and the knowledge graph.

## Components

| Class | What it is | Site source |
|---|---|---|
| `app-sidebar`, `app-nav`, `nav-label`, `nav-bottom` | dark navigation panel with line icons, amber current-item marker | `.signal-panel`, hero |
| `app-topbar` | page-ground bar with search, language, theme; capture-to-recall rule below | `Nav`, `PageHero` rule |
| `eyebrow` | mono label after a 1.75rem amber bar | `.kicker` |
| `page-heading`, `section-heading`, `breadcrumbs` | page and section titles, mono breadcrumb trail | `PageHero`, `Section` |
| `primary-button` | amber fill, `--ma-on-amber` text | `.btn-primary` |
| `secondary-button` | 1.5px line border, text border on hover | `.btn-ghost` |
| `quiet-button` | text-only action | |
| `danger-button` | rose ghost button for destructive actions | rose hue |
| `field-input`, `field-control`, `field-label`, `field-hint` | stacked and inline form controls | `.card` geometry |
| `badge` + `data-hue` | status and metadata chip; with a hue, a vivid dot | `.badge` |
| `status-pill`, `subtle-pill`, `message-*` | session and message states with a state dot | `.badge` |
| `callout` + `data-hue` | notice with a 4px rule on the soft fill | `Callout` |
| `insight-banner` | amber callout with an icon and title | `Callout` |
| `section-label` | small mono heading over a list or table | technical labels |
| `surface`, `metric-card`, `project-card`, `agent-card`, `docs-card` | bordered surfaces; cards are things you click | `.card` |
| `project-tabs`, `segmented-tabs`, `theme-switch` | tab track and toggles | `Tabs`, `.pick`, footer theme switch |
| `ops-table`, `usage-table` | tables with a tint head row | `.table-site` |
| `markdown-body` | reading styles for pages and guides | `.prose-site` |
| `terminal-block`, `.markdown-body pre` | dark terminal | `CodeBlock` |
| `graph-canvas`, `graph-edge`, `graph-node` | knowledge graph on an ink panel | `.memory-diagram` |
| `dialog-backdrop`, `dialog-panel` | modal over smoked ink | |
| `empty-state` | dashed outline with an icon tile | |
| `icon` (`_icon_*.html`) | 18px line icons | `CircuitIcon` |

### Icons

`folder`, `terminal`, `chip`, `file`, `search`, `server`, `shield`, `branch`
and `clock` reuse the paths of the site's `CircuitIcon` (120-unit grid, square
caps, round joins). `graph`, `envelope` and `menu` are drawn on the same grid
by the same rules. Every icon is `aria-hidden` next to a text label, inherits
`currentColor` and keeps a 1.75px stroke at any size
(`vector-effect: non-scaling-stroke`). Include one with
`{% include "_icon_folder.html" %}`.

## Breakpoints

The application keeps its own breakpoints; layouts change at these widths:

| Width | Change |
|---|---|
| ≥ 1700px | content column centered |
| ≤ 1400px | documentation outline moves above the article |
| ≤ 1250px | top-bar caption hidden; project cards two per row; metrics three per row |
| ≤ 1080px | inbox composer below the list |
| ≤ 960px | sidebar becomes a drawer (menu button, Escape closes it) |
| ≤ 700px | knowledge tree and documentation stack in one column |
| ≤ 640px | one card per row; metrics two per row; compact top bar |
| coarse pointer | buttons and fields at least 44px tall |

No page scrolls sideways at 360px; tables and code scroll inside their own
box.

## Rules of use

- Use a token utility (`text-muted`, `bg-surface`, `border-line`,
  `text-green`, `bg-amber-soft`) or a component class. Never a stock Tailwind
  shade (`text-slate-500`), a `dark:` color pair or a hex value in a template;
  `design_tokens.rs` fails on all three.
- Amber is the primary action. One primary button per view.
- Color the rail, border or dot, not the block: hue fills stay on chips,
  callouts and current items.
- Prefer open layouts and rules over boxes. Wrap something in a card when it
  is a thing you click or a self-contained record.
- Code, paths, ids and commands are mono; prose is sans; titles and numbers
  are Chakra Petch.
- New colors start on the site. If a state needs a color the site lacks, map
  it to a subject hue as the table above does.
- No neon, glows, CRT effects, multicolor gradients or decorative motion.
  Motion is limited to state changes and the loading sweep, and stops under
  `prefers-reduced-motion`.

## Contrast

WCAG 2.x ratios for every pairing the interface uses, computed from
`input.css`. Targets follow the site: 7:1 for body text, 4.5:1 for accents and
muted text, 3:1 for focus outlines and non-text markers.

| Pairing | Light | Dark | Target |
|---|---|---|---|
| text on bg / surface / tint | 11.23 / 12.33 / 10.21 | 14.66 / 13.21 / 11.59 | 7 |
| muted on bg / surface / tint | 6.16 / 6.76 / 5.60 | 9.53 / 8.59 / 7.54 | 4.5 |
| green on bg / surface / green-soft | 5.20 / 5.72 / 5.07 | 10.76 / 9.70 / 7.25 | 4.5 |
| violet on surface / violet-soft | 5.97 / 5.22 | 7.48 / 6.53 | 4.5 |
| rose on bg / surface / rose-soft | 5.79 / 6.36 / 5.54 | 10.58 / 9.54 / 7.12 | 4.5 |
| amber-ink on bg / surface / amber-soft | 5.59 / 6.15 / 5.33 | 8.81 / 7.94 / 5.72 | 4.5 |
| text on amber-soft / green-soft / rose-soft | 10.70 / 10.94 / 10.73 | 9.51 / 9.87 / 9.86 | 7 |
| focus on bg / surface / tint | 5.59 / 6.15 / 5.09 | 11.10 / 10.01 / 8.78 | 3 |

| Theme-invariant pairing | Ratio | Target |
|---|---|---|
| on-amber on amber / amber-hi | 7.78 / 9.81 | 7 |
| panel-text on ink / panel-surface | 16.05 / 11.01 | 7 |
| panel-muted on ink | 9.45 | 4.5 |
| amber on ink | 8.74 | 4.5 |
| amber on panel-surface (marker) | 6.00 | 3 |
| amber-hi on ink (focus) | 11.01 | 3 |
| capture on ink (graph node) | 10.68 | 3 |
| panel-line on ink (graph edge) | 4.72 | 3 |

## References

- Site tokens: [`src/styles/global.css`](https://github.com/akitaonrails/ai-memory-web/blob/main/src/styles/global.css)
- Site rules: [`docs/design-system.md`](https://github.com/akitaonrails/ai-memory-web/blob/main/docs/design-system.md)
- Approval record: [`docs/research/2026-10-15-memory-archive-implementation.md`](https://github.com/akitaonrails/ai-memory-web/blob/main/docs/research/2026-10-15-memory-archive-implementation.md)
- Icon geometry: [`src/components/CircuitIcon.astro`](https://github.com/akitaonrails/ai-memory-web/blob/main/src/components/CircuitIcon.astro)
- Audit before the redesign: [`ui-redesign-audit.md`](ui-redesign-audit.md)
- What changed: [`ui-redesign-implementation.md`](ui-redesign-implementation.md)
