const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"] as const
const MONTH_NUMBERS = new Map([
  ["jan", 1], ["january", 1], ["feb", 2], ["february", 2], ["mar", 3], ["march", 3],
  ["apr", 4], ["april", 4], ["may", 5], ["jun", 6], ["june", 6], ["jul", 7], ["july", 7],
  ["aug", 8], ["august", 8], ["sep", 9], ["september", 9], ["oct", 10], ["october", 10],
  ["nov", 11], ["november", 11], ["dec", 12], ["december", 12],
])
const HUMAN_DATE = /^(\d{1,2}) ([A-Za-z]+) (\d{4}),? (\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)? (UTC|Z)$/i
const ISO_DATE = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,3}))?(Z)?$/
const ISO_DAY = /^(\d{4})-(\d{2})-(\d{2})$/

/** Return canonical UTC ISO milliseconds, null for a blank/open bound. */
export function parseUtcText(value: string, day: "start" | "end" = "start"): string | null {
  const text = value.trim()
  if (!text) return null

  const isoDay = ISO_DAY.exec(text)
  if (isoDay) {
    const end = day === "end"
    return canonicalUtc(Number(isoDay[1]), Number(isoDay[2]), Number(isoDay[3]), end ? 23 : 0, end ? 59 : 0, end ? 59 : 0, end ? 999 : 0)
  }

  const iso = ISO_DATE.exec(text)
  if (iso) return canonicalUtc(Number(iso[1]), Number(iso[2]), Number(iso[3]), Number(iso[4]), Number(iso[5]), Number(iso[6]), Number((iso[7] ?? "").padEnd(3, "0")))

  const human = HUMAN_DATE.exec(text)
  if (!human) throw new Error("Enter a UTC date, e.g. 2026-06-01, or date and time, e.g. 1 Jun 2026, 14:30 UTC.")
  const month = MONTH_NUMBERS.get(human[2].toLowerCase())
  if (!month) throw new Error("Use an English month name, e.g. 12 Nov 2026, 14:30 UTC.")
  return canonicalUtc(Number(human[3]), month, Number(human[1]), Number(human[4]), Number(human[5]), Number(human[6] ?? 0), Number((human[7] ?? "").padEnd(3, "0")))
}

export function formatUtcText(canonical: string): string {
  const parts = canonicalParts(canonical)
  if (!parts) return canonical
  const [year, month, day, hour, minute, second, millisecond] = parts
  return `${day} ${MONTHS[month - 1]} ${year}, ${formatClock(hour, minute, second, millisecond)}`
}

export function formatUtcDate(canonical: string): string {
  const parts = canonicalParts(canonical)
  if (!parts) return canonical
  const [year, month, day] = parts
  return `${day} ${MONTHS[month - 1]} ${year}`
}

export function formatUtcClock(canonical: string): string {
  const parts = canonicalParts(canonical)
  if (!parts) return canonical
  return formatClock(parts[3], parts[4], parts[5], parts[6])
}

function canonicalUtc(year: number, month: number, day: number, hour: number, minute: number, second: number, millisecond: number): string {
  const date = new Date(0)
  date.setUTCFullYear(year, month - 1, day)
  date.setUTCHours(hour, minute, second, millisecond)
  if (year < 1 || !Number.isFinite(date.getTime()) || date.getUTCFullYear() !== year || date.getUTCMonth() !== month - 1 || date.getUTCDate() !== day || date.getUTCHours() !== hour || date.getUTCMinutes() !== minute || date.getUTCSeconds() !== second || date.getUTCMilliseconds() !== millisecond) {
    throw new Error("Enter a valid UTC calendar date and time.")
  }
  return date.toISOString()
}

function canonicalParts(value: string): [number, number, number, number, number, number, number] | undefined {
  let normalized: string | null
  try {
    normalized = parseUtcText(value)
  } catch {
    return undefined
  }
  if (!normalized) return undefined
  const match = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})\.(\d{3})Z$/.exec(normalized)
  if (!match) return undefined
  return [Number(match[1]), Number(match[2]), Number(match[3]), Number(match[4]), Number(match[5]), Number(match[6]), Number(match[7])]
}

function formatClock(hour: number, minute: number, second: number, millisecond: number): string {
  const clock = `${pad(hour)}:${pad(minute)}${second || millisecond ? `:${pad(second)}${millisecond ? `.${pad(millisecond, 3)}` : ""}` : ""}`
  return `${clock} UTC`
}

function pad(value: number, length = 2): string {
  return String(value).padStart(length, "0")
}
