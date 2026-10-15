# Design — Vigil

A locked design system for this app. Every page redesign reads this file before
emitting code. Do not regenerate per page — extend or amend this file when the
system needs to grow.

## Genre
atmospheric

## Macrostructure family
App pages only (index + detail share one shell).

- Index: Catalogue — monitors as elevated entries on the canvas, generous
  rhythm, one entry per monitor with status, bars, chart.
- Detail: Long Document — the incident record as a memo: status line,
  90-day record, latency account, incident ledger.

## Theme
Aurora (atmospheric): night-sky canvas with teal + violet blooms; mist-light
day canvas with faint blooms. Light and dark both first-class via toggle.

- `--color-paper`    oklch(14% 0.012 270) dark / oklch(97% 0.008 270) light
- `--color-paper-2`  oklch(18% 0.014 270) dark / oklch(94% 0.010 270) light
- `--color-ink`      oklch(93% 0.008 270) dark / oklch(21% 0.012 270) light
- `--color-rule`     oklch(30% 0.012 270) dark / oklch(87% 0.010 270) light
- `--color-accent`   oklch(70% 0.16 250)  dark / oklch(55% 0.20 260) light
- `--color-focus`    same as accent
- Status colors stay semantic (ok green / bad red), not themed.

## Typography
- Display: Geist Sans, weight 600, roman, tracking -0.03em
- Body:    Geist Sans, weight 400
- Mono:    JetBrains Mono, 400/600 — instrument readings only
  (status words, stat numerals, table heads, timestamps)
- Display cap: `clamp(3rem, 6vw + 1rem, 6rem)`

## Spacing
4-point named scale in `tokens.css`. Monitor entries breathe:
`--space-xl` between entries, `--space-lg` inside. Never raw values.

## Motion
- Easings: `--ease-out` cubic-bezier(0.16, 1, 0.3, 1) et al.
- One fade-in entrance + hero counter. Tooltips instant.
- Cross-document view transitions (`@view-transition: navigation auto`):
  drill into a monitor slides the new page in from the right (`forward`),
  returning slides it in from the left (`back`); same-path refreshes (range
  menu) stay untagged and keep the UA default. Direction is tagged on both
  sides by a blocking head script (`pageswap` from the destination URL,
  `pagereveal` from `navigation.activation.from` — types set in the old
  document never reach the new one), reloads keep the UA default.
- Reduced-motion fallback: opacity-only, ≤ 150ms; view transitions off.

## Microinteractions stance
- Silent success; no toasts (no async user actions on these pages).
- Hover tooltips instant; focus shows them with 0ms delay where focusable.
- Theme toggle persists to localStorage, respects OS default.

## CTA voice
- Links: accent underline offset; details-link is a typographic arrow link.
- Buttons: one pill toggle (theme), bordered, mono-free.

## Per-page allowances
- App pages MUST NOT use enrichment beyond the canvas blooms.
- Blooms: max two radial gradients, ~20-30% footprint, fixed-attached,
  no animation. Never on text.

## What pages MUST share
- The wordmark, N5 nav, Ft1 footer.
- The accent colour and its placement (≤ 5% per viewport).
- Display + body + mono fonts.
- Section rhythm: hero figure, stat strip, entries, footer.

## What pages MAY differ on
- Macrostructure within the family (Catalogue vs Long Document).
- Detail-only components: incident ledger table, axis chart.

## Exports

See skill `export-formats.md` for the canonical mapping.

### tokens.css
```css
:root {
  --color-paper:      oklch(14% 0.012 270);
  --color-paper-2:    oklch(18% 0.014 270);
  --color-ink:        oklch(93% 0.008 270);
  --color-rule:       oklch(30% 0.012 270);
  --color-accent:     oklch(70% 0.16 250);
  --color-focus:      oklch(70% 0.16 250);

  --font-display: "Geist Sans", ui-sans-serif, system-ui, sans-serif;
  --font-body:    "Geist Sans", ui-sans-serif, system-ui, sans-serif;
  --font-mono:   "JetBrains Mono", ui-monospace, monospace;

  --space-3xs: 0.125rem; --space-2xs: 0.25rem; --space-xs: 0.5rem;
  --space-sm:  0.75rem;  --space-md:  1rem;    --space-lg:  1.5rem;
  --space-xl:  2.5rem;   --space-2xl: 4rem;    --space-3xl: 6rem;

  --text-2xs: 0.7rem; --text-xs: 0.8rem;  --text-sm: 0.875rem; --text-base: 1rem;
  --text-md: 1.25rem; --text-lg: 1.5625rem; --text-xl: 1.9531rem;
  --text-display: clamp(3rem, 6vw + 1rem, 6rem);

  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
  --ease-in: cubic-bezier(0.7, 0, 0.84, 0);
  --ease-in-out: cubic-bezier(0.65, 0, 0.35, 1);
  --dur-micro: 120ms; --dur-short: 220ms; --dur-long: 420ms;

  --radius-s: 6px; --radius-m: 12px; --radius-pill: 999px;
}
```
