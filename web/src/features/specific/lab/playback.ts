// Pure playback scheduling for Specific sector frames. No React, no DOM, no network.

/** Memory bound for decoded frame canvases, in canvas pixels (RGBA, 4 bytes each):
 * 48 Mpx is about 192 MiB. It bounds browser memory only; it never limits which
 * frames, bands or snippets are built, and frames outside it are still played. */
export const DECODED_PIXEL_BUDGET = 48 * 1024 * 1024

/** Frames decoded ahead of the current one when not every frame fits the budget.
 * At 8 fps a frame lasts 125 ms, so four frames ahead absorb a 500 ms tile-latency
 * spike. With LOADS_IN_FLIGHT frame loads running, sustained 8 fps needs each
 * frame's tiles fetched and decoded within LOADS_IN_FLIGHT × 125 ms = 250 ms.
 * A slower frame shows "Buffering frame N" instead of being skipped. */
export const MAX_AHEAD = 4
export const LOADS_IN_FLIGHT = 2

/** Frame ids to keep decoded, in load priority order: current, then the frames
 * ahead in playback order, then the previous one. When every frame fits the
 * budget, every frame is kept, so looping makes no further requests. Otherwise the
 * window is previous + current + K ahead, with K = min(MAX_AHEAD, capacity − 2) and
 * at least one frame ahead even if a single frame exceeds the budget. */
export function planWindow(order: string[], current: number, framePixels: number, budget = DECODED_PIXEL_BUDGET) {
  const count = order.length
  if (current < 0 || current >= count) return []
  const at = (offset: number) => order[(current + offset + count) % count]!
  if (fitsBudget(count, framePixels, budget)) return Array.from({ length: count }, (_, offset) => at(offset))
  // Three is the irreducible rolling window (previous/current/next). A single
  // very large frame can therefore make the practical floor exceed the budget.
  const capacity = Math.max(3, Math.floor(budget / Math.max(framePixels, 1)))
  const ahead = Math.min(MAX_AHEAD, Math.max(1, capacity - 2), count - 1)
  const ids = Array.from({ length: ahead + 1 }, (_, offset) => at(offset))
  ids.push(at(-1))
  return [...new Set(ids)]
}

export function fitsBudget(count: number, framePixels: number, budget = DECODED_PIXEL_BUDGET) {
  return count * framePixels <= budget
}

export function nextFrame(current: number, count: number, loop: boolean) {
  if (current + 1 < count) return current + 1
  return loop ? 0 : undefined
}

export type FrameStatus = "decoded" | "failed" | "pending"
export type TickDecision =
  | { kind: "wait"; ms: number }
  | { kind: "advance"; index: number }
  | { kind: "buffer"; index: number }
  | { kind: "stop" }

/** One scheduler step. A frame is due one period after the previous frame was
 * actually swapped onto the screen, so every frame gets equal screen time. The
 * current frame is checked before advancing: it can still be decoding after Next,
 * Seek or an earlier advance, and must be buffered rather than skipped. A due
 * next frame that is not decoded yet is also buffered, never skipped; a failed
 * frame is advanced to so its error is shown. */
export function tickDecision(input: { now: number; lastSwap: number; period: number; current: number; count: number; loop: boolean; status: (index: number) => FrameStatus }): TickDecision {
  if (input.count < 2) return { kind: "stop" }
  const due = input.lastSwap + input.period
  if (input.now < due) return { kind: "wait", ms: due - input.now }
  if (input.status(input.current) === "pending") return { kind: "buffer", index: input.current }
  const index = nextFrame(input.current, input.count, input.loop)
  if (index === undefined) return { kind: "stop" }
  return input.status(index) === "pending" ? { kind: "buffer", index } : { kind: "advance", index }
}
