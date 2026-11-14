/** Sky position entry for the location form.
 *
 * RA accepts decimal degrees ("56.75") or hours on the sky
 * ("20h 30m 32.0s", "20:30:32"). Dec accepts decimal degrees ("24.12") or
 * degrees, arcminutes and arcseconds ("40° 17' 50.2\"", "40:17:50.2",
 * "40d 17m 50.2s"). The returned values are always decimal degrees, the unit
 * the URL and the engine use. Cases live in `position.test.ts`.
 */

export const MAX_FOV = 30

/** Decimal text for URLs, request parameters and file names: six places,
 * trailing zeros dropped. */
export function formatNumber(value: number): string {
  return String(Number(value.toFixed(6)))
}

/** FOV above this (degrees) shows an advisory huge-field warning. Exactly this value is silent. */
export const FOV_WARN_DEG = 10

export function isLargeField(fov: number): boolean {
  return Number.isFinite(fov) && fov > FOV_WARN_DEG
}

const NUMBER = String.raw`[+-]?(?:\d+(?:\.\d*)?|\.\d+)`

const RA_HOURS = new RegExp(
  `^(${NUMBER})\\s*h(?:\\s*(${NUMBER})\\s*m?)?(?:\\s*(${NUMBER})\\s*s?)?$`,
  "i",
)
const DEC_DEGREES = new RegExp(
  `^(${NUMBER})\\s*(?:°|d)(?:\\s*(${NUMBER})\\s*(?:'|m))?(?:\\s*(${NUMBER})\\s*(?:"|s))?$`,
  "i",
)

/** Typographic minus, primes and degree signs are common in copied coordinates. */
function normalize(text: string): string {
  return text
    .trim()
    .replace(/[\u2212\u2013\u2014]/g, "-")
    .replace(/[\u2032\u2019]/g, "'")
    .replace(/[\u2033\u201d]/g, '"')
    .replace(/[\u00ba\u00b0]/g, "°")
}

/** "20:30:32" or "40:17:50.2": two or three colon-separated parts. */
function colonParts(text: string): number[] | undefined {
  if (!text.includes(":")) return undefined
  const chunks = text.split(":").map((part) => part.trim())
  if (chunks.length < 2 || chunks.length > 3 || chunks.some((chunk) => chunk === "")) return undefined
  const values = chunks.map(Number)
  return values.every(Number.isFinite) ? values : undefined
}

export function parseRa(input: string): number | undefined {
  const text = normalize(input)
  if (text === "") return undefined
  const match = text.match(RA_HOURS)
  if (match) {
    return raFromHours(Number(match[1]), Number(match[2] ?? 0), Number(match[3] ?? 0))
  }
  const parts = colonParts(text)
  if (parts) {
    const [hours = 0, minutes = 0, seconds = 0] = parts
    return raFromHours(hours, minutes, seconds)
  }
  const degrees = Number(text)
  if (!Number.isFinite(degrees) || degrees < 0 || degrees > 360) return undefined
  return ((degrees % 360) + 360) % 360
}

function raFromHours(hours: number, minutes: number, seconds: number): number | undefined {
  if (!(hours >= 0 && hours < 24)) return undefined
  if (!(minutes >= 0 && minutes < 60) || !(seconds >= 0 && seconds < 60)) return undefined
  return ((hours + minutes / 60 + seconds / 3600) * 15) % 360
}

export function parseDec(input: string): number | undefined {
  const text = normalize(input)
  if (text === "") return undefined
  const negative = text.startsWith("-")
  const match = text.match(DEC_DEGREES)
  if (match) {
    return decFromParts(negative, Math.abs(Number(match[1])), Number(match[2] ?? 0), Number(match[3] ?? 0))
  }
  const parts = colonParts(text)
  if (parts) {
    if (parts.slice(1).some((part) => part < 0)) return undefined
    const [degrees = 0, minutes = 0, seconds = 0] = parts
    return decFromParts(negative, Math.abs(degrees), minutes, seconds)
  }
  const degrees = Number(text)
  if (!Number.isFinite(degrees) || Math.abs(degrees) > 90) return undefined
  return degrees
}

function decFromParts(negative: boolean, degrees: number, minutes: number, seconds: number): number | undefined {
  if (!(degrees >= 0 && degrees <= 90)) return undefined
  if (!(minutes >= 0 && minutes < 60) || !(seconds >= 0 && seconds < 60)) return undefined
  if (degrees === 90 && (minutes > 0 || seconds > 0)) return undefined
  const value = degrees + minutes / 60 + seconds / 3600
  if (value === 0) return 0
  return negative ? -value : value
}

/** Maximum field of view (degrees) the mosaic painter supports. */
export function parseFov(input: string): number | undefined {
  const text = normalize(input).replace(/\s*°$/, "")
  if (text === "") return undefined
  const value = Number(text)
  if (!Number.isFinite(value) || value <= 0 || value > MAX_FOV) return undefined
  return value
}
