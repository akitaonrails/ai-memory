// Memory Archive theme. Mirrors the `@theme inline` block of the
// institutional site (akitaonrails/ai-memory-web, src/styles/global.css):
// every color is a CSS variable from static/input.css, so one utility covers
// both themes and templates never pair a light shade with a `dark:` shade.
// `colors` is replaced, not extended, so Tailwind's stock palette (slate,
// blue, ...) cannot creep back into a template. See docs/ui-design-system.md.
const token = (name) => `var(--ma-${name})`;

module.exports = {
  content: ["./templates/**/*.html"],
  darkMode: 'class',
  theme: {
    colors: {
      transparent: 'transparent',
      current: 'currentColor',
      inherit: 'inherit',
      bg: token('bg'),
      surface: token('surface'),
      tint: token('tint'),
      line: token('line'),
      text: token('text'),
      muted: token('muted'),
      focus: token('focus'),
      green: token('green'),
      teal: token('teal'),
      violet: token('violet'),
      rose: token('rose'),
      'amber-ink': token('amber-ink'),
      'green-soft': token('green-soft'),
      'teal-soft': token('teal-soft'),
      'violet-soft': token('violet-soft'),
      'rose-soft': token('rose-soft'),
      'amber-soft': token('amber-soft'),
      amber: token('amber'),
      'amber-hi': token('amber-hi'),
      'on-amber': token('on-amber'),
      ink: token('ink'),
      'panel-text': token('panel-text'),
      'panel-muted': token('panel-muted'),
      'panel-line': token('panel-line'),
      capture: token('capture'),
      recall: token('recall'),
    },
    extend: {
      borderColor: { DEFAULT: token('line') },
      ringColor: { DEFAULT: token('focus') },
      fontFamily: {
        sans: token('font-sans'),
        display: token('font-display'),
        mono: token('font-mono'),
      },
      // Squared controls share the device-panel geometry (radius-card: 2px).
      borderRadius: { DEFAULT: '2px', sm: '2px', md: '2px', lg: '2px' },
      // The dashboard is denser than the site; its smallest step is 13px.
      fontSize: { xs: ['0.8125rem', { lineHeight: '1.25rem' }] },
    },
  },
  plugins: [],
};
