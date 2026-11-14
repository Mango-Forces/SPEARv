/** `band` is the 0-based BANDS index; the interface numbers bands 1-102. */
export function bandNumber(band: number | null | undefined): string {
  return band == null ? "—" : String(band + 1)
}

/** SPHEREx packs 17 consecutive subchannels onto each detector. */
export function detectorNumber(band: number | null | undefined): string {
  if (band == null || band < 0 || band > 101) return "—"
  return String(Math.floor(band / 17) + 1)
}
