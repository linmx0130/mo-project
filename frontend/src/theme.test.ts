// Tests for the theme color helpers (src/theme.ts): hex parsing,
// dark-mode lightening, rgba formatting, and applying the configured
// accent to the `--accent*` CSS custom properties on <html>.
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { applyThemeColor, lighten, parseHexColor, toHex, toRgba } from './theme'

/** A minimal CSSStyleDeclaration stub backed by a Map, stubbed on
 *  globalThis (the vitest node environment has no DOM). */
function stubDocument(): Map<string, string> {
  const style = new Map<string, string>()
  vi.stubGlobal('document', {
    documentElement: {
      style: {
        setProperty: (name: string, value: string) => void style.set(name, value),
        removeProperty: (name: string) => void style.delete(name),
      },
    },
  })
  return style
}

beforeEach(() => {
  vi.unstubAllGlobals()
})

describe('parseHexColor', () => {
  it('parses #RRGGBB, case-insensitive', () => {
    expect(parseHexColor('#009dc4')).toEqual({ r: 0x00, g: 0x9d, b: 0xc4 })
    expect(parseHexColor('#A1B2C3')).toEqual({ r: 0xa1, g: 0xb2, b: 0xc3 })
  })

  it('expands #RGB shorthand', () => {
    expect(parseHexColor('#0af')).toEqual({ r: 0x00, g: 0xaa, b: 0xff })
  })

  it('rejects anything that is not a 3/6-digit hex color', () => {
    for (const bad of ['purple', '#12345', '#1234567', '#gggggg', '#', '', 'c084fc', '#c084fc80']) {
      expect(parseHexColor(bad)).toBeNull()
    }
  })
})

describe('lighten / toHex / toRgba', () => {
  it('lighten mixes with white (0 = unchanged, 1 = white)', () => {
    const rgb = { r: 0, g: 157, b: 196 }
    expect(lighten(rgb, 0)).toEqual(rgb)
    expect(lighten(rgb, 1)).toEqual({ r: 255, g: 255, b: 255 })
    expect(lighten(rgb, 0.5)).toEqual({ r: 128, g: 206, b: 226 })
  })

  it('toHex formats back to #RRGGBB', () => {
    expect(toHex({ r: 0, g: 157, b: 196 })).toBe('#009dc4')
    expect(toHex({ r: 0, g: 10, b: 255 })).toBe('#000aff')
  })

  it('toRgba formats an rgba() CSS string', () => {
    expect(toRgba({ r: 0, g: 157, b: 196 }, 0.12)).toBe('rgba(0, 157, 196, 0.12)')
  })
})

describe('applyThemeColor', () => {
  it('sets the accent variables for light mode (the color is the source, verbatim)', () => {
    const style = stubDocument()
    applyThemeColor('light', '#009dc4')
    expect(style.get('--accent')).toBe('#009dc4')
    expect(style.get('--accent-bg')).toBe('rgba(0, 157, 196, 0.09)')
    expect(style.get('--accent-bg-hover')).toBe('rgba(0, 157, 196, 0.16)')
  })

  it('sets the accent variables for dark mode (lightened for contrast)', () => {
    const style = stubDocument()
    applyThemeColor('dark', '#009dc4')
    // #009dc4 lightened 40% toward white: (102, 196, 220) = #66c4dc.
    expect(style.get('--accent')).toBe('#66c4dc')
    expect(style.get('--accent-bg')).toBe('rgba(102, 196, 220, 0.12)')
    expect(style.get('--accent-bg-hover')).toBe('rgba(102, 196, 220, 0.2)')
  })

  it('removes the overrides for null (restores the stylesheet defaults)', () => {
    const style = stubDocument()
    applyThemeColor('dark', '#009dc4')
    expect(style.size).toBe(3)
    applyThemeColor('dark', null)
    expect(style.size).toBe(0)
  })

  it('treats an invalid hex like null', () => {
    const style = stubDocument()
    applyThemeColor('dark', 'nope')
    expect(style.size).toBe(0)
  })

  it('re-applying for the other theme replaces the values', () => {
    const style = stubDocument()
    applyThemeColor('dark', '#22c55e')
    // #22c55e lightened 40% toward white: (122, 220, 158) = #7adc9e.
    expect(style.get('--accent')).toBe('#7adc9e')
    applyThemeColor('light', '#22c55e')
    expect(style.get('--accent')).toBe('#22c55e')
    expect(style.size).toBe(3)
  })
})
