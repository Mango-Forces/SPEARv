import type { View } from "../../url-state/view.ts"

/** Specific owns its own canvas export even if an old link carries `color`. */
export function isColorImage(view: Pick<View, "color" | "mode">): boolean {
  return view.color === true && view.mode !== "specific"
}
