/** Three-channel false-colour recipe. Bands are API numbering (0..101); the
 * interface shows them as 1..102, like the rest of the band controls. The
 * server's defaults (blue 5, green 40, red 90, width 5) match
 * `DEFAULT_COLOUR_BANDS`. Pure, so URL parsing shares it without React. */

export type ColourBand = {
  /** 0-based band index, 0..101. */
  pin: number
  /** Number of bands in the range, 1..102. */
  width: number
}

export type ColourBands = {
  r: ColourBand
  g: ColourBand
  b: ColourBand
}

/** What the colour controls emit, ready for a request body. */
export type ColourConfigValue = { colorBands: ColourBands }

/** The server's default recipe, in R/G/B order. */
export const DEFAULT_COLOUR_BANDS: ColourBands = {
  r: { pin: 90, width: 5 },
  g: { pin: 40, width: 5 },
  b: { pin: 5, width: 5 },
}

export const DEFAULT_COLOUR_CONFIG: ColourConfigValue = { colorBands: DEFAULT_COLOUR_BANDS }
