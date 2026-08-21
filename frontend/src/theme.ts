// Theme helpers: the accent color the UI is themed with.
//
// The accent lives in `index.css` as CSS custom properties (`--accent`,
// `--accent-bg`, `--accent-bg-hover`) with a dark and a light variant. The
// gateway serves a single configured hex (`GET /api/meta` → `theme_color`,
// from `mo.toml`); this module derives the per-theme values from it and
// applies them as inline styles on `<html>`, which override the stylesheet
// defaults. Light mode uses the color verbatim; dark mode lightens it (mix
// with white) so the accent stands out on the dark background, and both
// modes get translucent tints for backgrounds/hovers.

export type Theme = 'dark' | 'light'

export interface Rgb {
  r: number
  g: number
  b: number
}

/** Translucent tint alphas for the accent backgrounds (light mode uses the
 *  color as-is, dark mode the lightened variant). */
const DARK_BG_ALPHA = 0.12
const DARK_BG_HOVER_ALPHA = 0.2
const LIGHT_BG_ALPHA = 0.09
const LIGHT_BG_HOVER_ALPHA = 0.16

/** How much of the configured color to mix with white for the dark-mode
 *  accent (0 = unchanged, 1 = white). 0.4 keeps the hue while making the
 *  accent stand out on the dark background. */
const DARK_LIGHTEN = 0.4

/** Parse a `#RGB` / `#RRGGBB` hex color (case-insensitive); null for
 *  anything else. */
export function parseHexColor(hex: string): Rgb | null {
  const match = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim())
  if (!match) return null
  let digits = match[1]
  if (digits.length === 3) {
    digits = digits
      .split('')
      .map((c) => c + c)
      .join('')
  }
  const value = parseInt(digits, 16)
  return { r: (value >> 16) & 0xff, g: (value >> 8) & 0xff, b: value & 0xff }
}

/** Format back to a `#RRGGBB` hex string. */
export function toHex(rgb: Rgb): string {
  const channel = (v: number) => v.toString(16).padStart(2, '0')
  return `#${channel(rgb.r)}${channel(rgb.g)}${channel(rgb.b)}`
}

/** `rgba(r, g, b, alpha)` CSS string. */
export function toRgba(rgb: Rgb, alpha: number): string {
  return `rgba(${rgb.r}, ${rgb.g}, ${rgb.b}, ${alpha})`
}

/** Mix a color with white: `amount` 0..1 (0 = unchanged, 1 = white). */
export function lighten(rgb: Rgb, amount: number): Rgb {
  const mix = (v: number) => Math.round(v + (255 - v) * amount)
  return { r: mix(rgb.r), g: mix(rgb.g), b: mix(rgb.b) }
}

/** The three accent CSS variables this module overrides. */
const ACCENT_VARS = ['--accent', '--accent-bg', '--accent-bg-hover'] as const

/** Apply the configured accent color to the current theme's CSS custom
 *  properties on `<html>`. Light mode uses the color verbatim; dark mode
 *  lightens it for contrast. `hex` null (or invalid) restores the
 *  stylesheet defaults (deep cyan) by removing the overrides. */
export function applyThemeColor(theme: Theme, hex: string | null): void {
  const root = document.documentElement
  const rgb = hex ? parseHexColor(hex) : null
  if (!rgb) {
    for (const variable of ACCENT_VARS) {
      root.style.removeProperty(variable)
    }
    return
  }
  const accent = theme === 'dark' ? lighten(rgb, DARK_LIGHTEN) : rgb
  const bgAlpha = theme === 'dark' ? DARK_BG_ALPHA : LIGHT_BG_ALPHA
  const hoverAlpha = theme === 'dark' ? DARK_BG_HOVER_ALPHA : LIGHT_BG_HOVER_ALPHA
  root.style.setProperty('--accent', toHex(accent))
  root.style.setProperty('--accent-bg', toRgba(accent, bgAlpha))
  root.style.setProperty('--accent-bg-hover', toRgba(accent, hoverAlpha))
}
