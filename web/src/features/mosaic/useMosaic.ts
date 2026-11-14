import { useEffect, useState } from "react"
import { client } from "../../api"
import type { components } from "../../bindings/schema"
import { DEFAULT_COLOUR_BANDS, type ColourBands } from "../../url-state/colour"
import { formatPin, type View } from "../../url-state/view"

export type MosaicInfo = components["schemas"]["MosaicInfo"]
export type MosaicStatus = components["schemas"]["MosaicStatus"]
export type Mosaic = { canvas: HTMLCanvasElement; info: MosaicInfo }

/** One painter box: a tile on the box path, in its channel's colour. */
export type MosaicBox = { x: number; y: number; color: string; label: string }

export type MosaicState = {
  mosaic?: Mosaic
  status?: MosaicStatus
  /** The live painter boxes. Empty once the final image is on the canvas. */
  frontier: MosaicBox[]
  /** Boxes that have reached the end of the path, out of the boxes running. */
  arrived: number
  /** Tiles the boxes can stand on: the full spiral, centre last. */
  pathLength: number
  /** Tiles reported changed but not on the canvas yet. */
  drawing: number
  /** Tiles the server has but this page could not draw. */
  undrawn: number
  /** True once the finished server RGB tiles are on the canvas. */
  final: boolean
  /** Positive once the completed image is settled; Color skips the final Color tile pass. */
  finalRevision?: number
  canvas?: HTMLCanvasElement
  info?: MosaicInfo
  /** The engine went away mid-job, so the view is waiting to start it again. */
  reconnecting?: boolean
  error?: string
}

const POLL_MS = 500
const TILE_FETCHES = 6
/** A slider drag commits many views. Only one that holds still starts a job. */
const SETTLE_MS = 150
/** A tile behind a box refetches at most this often while it gains data. */
const REFRESH_EVERY_MS = 1000
/** Background refreshes waiting at once, so a backlog of tiles that are all
 * gaining data cannot delay the boxes' own paint. */
const REFRESH_QUEUE = 24
/** Tiles fetched ahead of a box, within what is finished, so it need not wait a
 * whole fetch at each tile. */
const PAINT_AHEAD = 4
/** Glide bounds in path tiles per second: steady, never a jump. */
const GLIDE_MIN = 3
const GLIDE_MAX = 20
/** No animation frame moves a box more than this many path tiles, so a slow
 * frame under load never reads as a teleport. Normal frames move far less. */
const GLIDE_STEP = 6
/** The finished tiles get this many tries before the page reports a gap. */
const FINAL_ATTEMPTS = 3
const COLOR_PROGRESSIVE_EDGE = 1024
const RECONNECT_MS = 1000
const IDLE: MosaicState = { frontier: [], arrived: 0, pathLength: 0, drawing: 0, undrawn: 0, final: false }

const BOX_COLORS = [
  { channel: 2, color: "#ef4444", label: "R" },
  { channel: 1, color: "#22c55e", label: "G" },
  { channel: 0, color: "#60a5fa", label: "B" },
]
const PLUS_BOX = { channel: 0, color: "#fb923c", label: "+" }
const MINUS_BOX = { channel: 1, color: "#38bdf8", label: "−" }
const GREY_BOX = { channel: 0, color: "#ffffff", label: "" }

/** The engine restarted, which drops its jobs. Starting again is the whole fix. */
class EngineLost extends Error {}

type BoxSpec = { channel: number; color: string; label: string }

/** Starts or joins the job for this view and paints it layer by layer. One
 * box per channel walks the full spiral the server reports, from the
 * top-left tile to the centre. A box stands on the tile its channel is filling
 * and moves only when the server says that tile is finished and the finished
 * tile is on the canvas, so what is behind a box is complete on screen. The tile
 * under it is painted as its reads land. The box
 * glides with requestAnimationFrame, tile to adjacent tile
 * along the path, and every tile it passes is fetched and painted for its
 * channel, even before it is final. Each poll compares what is drawn on every
 * tile behind a box with the latest version and fetches the difference, so a
 * failed or outdated fetch is simply fetched again. Color's progressive
 * channel layers use bounded backing; the final display and export come from
 * server-rendered revisions after science is done. Composite keeps its CPU
 * raw-layer path. A delta cannot be split into channel layers, so its two boxes walk
 * tiles, only as far as tiles have settled, and only settled tiles are shown.
 * Leaving the view aborts polling, animation frames and fetches. */
export function useMosaic(view: View, bands: ColourBands = DEFAULT_COLOUR_BANDS): MosaicState {
  const [state, setState] = useState<MosaicState>(IDLE)
  const { ra, dec, fovL, fovH, dr, mask, color, mode } = view
  const pinKey = view.pins.map(formatPin).join("&")
  // The colour recipe is part of the job identity: a different mapping must ask
  // the engine for a different job, not join the previous one's tiles.
  const bandsKey = color ? `${bands.r.pin}:${bands.r.width}:${bands.g.pin}:${bands.g.width}:${bands.b.pin}:${bands.b.width}` : ""

  useEffect(() => {
    setState(IDLE)
    const ac = new AbortController()
    const update = (next: Partial<MosaicState>) => {
      if (!ac.signal.aborted) setState((state) => ({ ...state, ...next }))
    }
    const pin = pinKey ? pinKey.split("&") : []
    const query = {
      ra,
      dec,
      fov_l: fovL,
      fov_h: fovH,
      dr,
      mask: mask ? "on" : "off",
      mode,
      pin,
      ...(color
        ? {
            color: "1",
            color_r: bands.r.pin,
            color_r_width: bands.r.width,
            color_g: bands.g.pin,
            color_g_width: bands.g.width,
            color_b: bands.b.pin,
            color_b_width: bands.b.width,
          }
        : {}),
    }
    let timer = 0
    const start = () => {
      follow(query, ac.signal, update).catch((err: unknown) => {
        if (ac.signal.aborted) return
        if (!(err instanceof EngineLost)) return update({ error: err instanceof Error ? err.message : "the mosaic failed" })
        setState({ ...IDLE, reconnecting: true })
        timer = window.setTimeout(start, RECONNECT_MS)
      })
    }
    timer = window.setTimeout(start, SETTLE_MS)
    return () => {
      window.clearTimeout(timer)
      ac.abort()
    }
  }, [ra, dec, fovL, fovH, dr, mask, color, mode, pinKey, bandsKey])

  return state
}

async function follow(
  query: {
    ra: number
    dec: number
    fov_l: number
    fov_h: number
    dr: string
    mask: string
    mode: string
    pin: string[]
    color?: string
    color_r?: number
    color_r_width?: number
    color_g?: number
    color_g_width?: number
    color_b?: number
    color_b_width?: number
  },
  signal: AbortSignal,
  update: (next: Partial<MosaicState>) => void,
) {
  const started = await reach(client.GET("/api/v1/mosaic", { params: { query }, signal }), signal)
  if (started.response.status >= 500) throw new EngineLost()
  if (!started.data) throw new Error(serverError(started.error) ?? `the mosaic did not start (${started.response.status})`)
  const info = started.data
  update({ reconnecting: false })
  const canvas = document.createElement("canvas")
  const color = query.color === "1"
  const backingScale = color ? Math.min(1, COLOR_PROGRESSIVE_EDGE / Math.max(info.width, info.height)) : 1
  canvas.width = Math.max(1, Math.ceil(info.width * backingScale))
  canvas.height = Math.max(1, Math.ceil(info.height * backingScale))
  const backingScaleX = canvas.width / info.width
  const backingScaleY = canvas.height / info.height
  canvas.setAttribute("role", "img")
  canvas.setAttribute("aria-label", "Rendered sky, painted layer by layer. Dark tiles have no data yet.")
  const context = canvas.getContext("2d")
  if (!context) throw new Error("the mosaic has no 2d context")
  const ctx: CanvasRenderingContext2D = context
  ctx.setTransform(backingScaleX, 0, 0, backingScaleY, 0, 0)
  ctx.fillStyle = "#000"
  ctx.fillRect(0, 0, info.width, info.height)
  update({ mosaic: { canvas, info }, canvas, info })

  const tileKey = (x: number, y: number) => `${x},${y}`
  const latest = new Map<string, { version: number; settled: boolean }>()
  /** The version on the canvas per layer tile, set only once it is drawn. */
  const drawn = new Map<string, number>()
  const refreshedAt = new Map<string, number>()
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches
  const window_ = window as unknown as {
    __mosaicBoxes?: MosaicBox[]
    __mosaicBoxPath?: string[]
    __mosaicId?: string
    /** Per box, the path indices actually painted on its layer. */
    __mosaicPainted?: number[][]
    /** Per channel settled-prefix counts from the last status. */
    __mosaicFrontiers?: number[]
    /** Paint keys queued or being fetched, for telling in-flight from lost. */
    __mosaicPending?: () => string[]
    /** Per box, tiles it has passed whose drawn version is behind the latest. */
    __mosaicStale?: () => number[]
    __mosaicReadsDoneAt?: number
    __mosaicFinalAt?: number
  }
  window_.__mosaicId = info.id
  let finished = false
  let raf = 0
  let boxes: BoxSpec[] = []
  const layers: HTMLCanvasElement[] = []
  const dirty: { x: number; y: number }[] = []
  const releaseLayers = () => {
    layers.forEach((layer) => { layer.width = 0; layer.height = 0 })
    layers.length = 0
  }
  signal.addEventListener("abort", () => {
    window.cancelAnimationFrame(raf)
    releaseLayers()
  })

  // At most `TILE_FETCHES` tile fetches at once. A tile keeps one fetch in
  // flight, so a newer version cannot be overdrawn by a slower, older one.
  // Box passes queue ahead of background refreshes.
  const wanted: { key: string; run: () => Promise<void>; priority: boolean }[] = []
  const inFlight = new Set<string>()
  /** Tiles whose last fetch failed. A later success takes them off. */
  const failed = new Set<string>()
  const note = () => update({ drawing: wanted.length + inFlight.size, undrawn: failed.size })
  window_.__mosaicPending = () => [...inFlight, ...wanted.map((entry) => entry.key)]
  const pump = () => {
    if (signal.aborted) return
    wanted.sort((a, b) => Number(b.priority) - Number(a.priority))
    while (wanted.length > 0 && inFlight.size < TILE_FETCHES) {
      const next = wanted.shift()
      if (!next || inFlight.has(next.key)) continue
      inFlight.add(next.key)
      next
        .run()
        .then(() => failed.delete(next.key))
        .catch(() => {
          if (!signal.aborted) failed.add(next.key)
        })
        .finally(() => {
          inFlight.delete(next.key)
          note()
          pump()
        })
    }
  }
  const want = (key: string, priority: boolean, run: () => Promise<void>) => {
    if (inFlight.has(key)) return
    const at = wanted.findIndex((entry) => entry.key === key)
    if (at >= 0) wanted[at] = { key, run, priority: priority || wanted[at].priority }
    else wanted.push({ key, run, priority })
    note()
    pump()
  }
  const fetchBitmap = async (x: number, y: number, version: number, channel: number | undefined) => {
    const result = await client.GET("/api/v1/mosaic/{id}/tile", {
      params: { path: { id: info.id }, query: { x, y, v: version, ...(channel === undefined ? {} : { channel }) } },
      parseAs: "blob",
      signal,
    })
    if (!result.data) throw new Error("tile failed")
    const bitmap = await createImageBitmap(result.data as Blob)
    if (signal.aborted) {
      bitmap.close()
      throw new Error("aborted")
    }
    return bitmap
  }

  /** Land complete tiles. Composite replaces with final tiles; Color keeps the
   * bounded progressive paint until the server display preview takes over. */
  async function land(end: MosaicStatus, replay = false) {
    finished = true
    window.cancelAnimationFrame(raf)
    update({ status: end, frontier: [], arrived: boxes.length })
    if (signal.aborted) return
    wanted.length = 0
    failed.clear()
    // Drain old layer blits before final tiles overwrite the same canvas.
    while (!signal.aborted && inFlight.size > 0) {
      await new Promise((resolve) => window.setTimeout(resolve, 100))
    }
    failed.clear()
    const tiles = end.order.length > 0 ? end.order : Array.from(
      { length: info.rows * info.cols },
      (_, index) => ({ x: index % info.cols, y: Math.floor(index / info.cols) }),
    )
    if (replay) {
      boxes = end.channels_total.length === 3 ? BOX_COLORS : end.channels_total.length === 2 ? [PLUS_BOX, MINUS_BOX] : [GREY_BOX]
      window_.__mosaicBoxPath = tiles.map((tile) => tileKey(tile.x, tile.y))
      window_.__mosaicPainted = boxes.map(() => [])
      window_.__mosaicFrontiers = boxes.map(() => tiles.length)
      window_.__mosaicReadsDoneAt = Date.now()
    }
    if (color) {
      flushDirty()
      window_.__mosaicFinalAt = Date.now()
      window_.__mosaicBoxes = []
      releaseLayers()
      update({ final: true, finalRevision: 1, frontier: [], arrived: boxes.length, drawing: 0, undrawn: 0 })
      return
    }
    // At most six tile images are retained, not another full-field backing
    // store. Fetch ahead in batches, then reveal them in path order.
    for (let offset = 0; offset < tiles.length && !signal.aborted; offset += TILE_FETCHES) {
      update({ drawing: tiles.length - offset })
      const batch = await Promise.all(tiles.slice(offset, offset + TILE_FETCHES).map(async (tile) => {
        const key = `final:${tileKey(tile.x, tile.y)}`
        for (let attempt = 0; attempt < FINAL_ATTEMPTS && !signal.aborted; attempt++) {
          try {
            const bitmap = await finalTile(tile.x, tile.y)
            failed.delete(key)
            return bitmap
          } catch {
            if (!signal.aborted) failed.add(key)
          }
        }
        return undefined
      }))
      for (let index = 0; index < batch.length; index++) {
        const bitmap = batch[index]
        if (!bitmap) continue
        // One tile per frame makes even a cached done-join visibly assemble.
        // Reduced motion skips the deliberate reveal, not the tile delivery.
        if (replay && !reduced && !signal.aborted) await new Promise<void>((resolve) => window.requestAnimationFrame(() => resolve()))
        if (!signal.aborted) {
          const tile = tiles[offset + index]
          ctx.globalCompositeOperation = "source-over"
          ctx.drawImage(bitmap, tile.x * info.tile, tile.y * info.tile)
          if (replay) {
            const frontier = boxes.map((box) => ({ x: tile.x, y: tile.y, color: box.color, label: box.label }))
            window_.__mosaicBoxes = frontier
            window_.__mosaicPainted?.forEach((painted) => painted.push(offset + index))
            update({ frontier, pathLength: tiles.length, drawing: tiles.length - offset - index - 1 })
          }
        }
        bitmap.close()
      }
      update({ undrawn: failed.size })
    }
    if (!signal.aborted) {
      window_.__mosaicFinalAt = Date.now()
      window_.__mosaicBoxes = []
      if (failed.size === 0) {
        // Releasing progressive layers cannot erase the complete canvas. The
        // raw client writes its local mix over these pixels, without a clear.
        layers.forEach((layer) => { layer.width = 0; layer.height = 0 })
        layers.length = 0
      }
      update({ final: true, finalRevision: failed.size === 0 ? 1 : 0, frontier: [], arrived: boxes.length, drawing: 0, undrawn: failed.size })
    }
  }

  async function finalTile(x: number, y: number) {
    return fetchBitmap(x, y, 0, undefined)
  }

  const readStatus = async (since: number) => {
    const result = await reach(
      client.GET("/api/v1/mosaic/{id}/status", { params: { path: { id: info.id }, query: { since } }, signal }),
      signal,
    )
    // A 404 here is a job the engine no longer has, not a bad view.
    if (result.response.status === 404 || result.response.status >= 500) throw new EngineLost()
    if (!result.data) throw new Error(serverError(result.error) ?? `mosaic status failed (${result.response.status})`)
    return result.data
  }


  /** While the tab is open, touch status often enough that the engine will not
   * abandon this job (server grace is 10 minutes after last activity). Visibility
   * pauses the timer; closing the tab aborts via signal. */
  const keepAlive = () => {
    const beat = async () => {
      if (signal.aborted) return
      try {
        await readStatus(since)
      } catch {
        /* engine lost; useMosaic reconnect path owns recovery on next view */
      }
    }
    const schedule = () => {
      if (signal.aborted) return
      const ms = document.visibilityState === "visible" ? 60_000 : 120_000
      window.setTimeout(async () => {
        if (signal.aborted) return
        await beat()
        schedule()
      }, ms)
    }
    void beat()
    schedule()
    document.addEventListener("visibilitychange", () => {
      if (!signal.aborted && document.visibilityState === "visible") void beat()
    }, { signal })
  }

  let status = await readStatus(0)
  let since = status.seq
  if (status.state === "done" || status.state === "failed" || status.state === "cancelled") {
    // A done-join replays complete tiles instead of mounting an empty final canvas.
    if (status.state === "done") {
      await land(status, true)
      keepAlive()
    } else update({ status, error: status.error ?? undefined })
    return
  }

  // The path is empty while planning; the boxes start once it is known.
  while (status.order.length === 0 && (status.state === "planning" || status.state === "reading") && !signal.aborted) {
    update({ status })
    await new Promise((resolve) => window.setTimeout(resolve, POLL_MS))
    status = await readStatus(since)
    since = status.seq
  }
  if (signal.aborted) return
  if (status.state === "done") {
    await land(status, true)
    keepAlive()
    return
  }
  if (status.state === "failed" || status.state === "cancelled") {
    update({ status, error: status.error ?? undefined })
    return
  }

  const path = status.order
  const layered = status.channels_total.length !== 2
  boxes = status.channels_total.length === 3 ? BOX_COLORS : status.channels_total.length === 2 ? [PLUS_BOX, MINUS_BOX] : [GREY_BOX]
  if (layered) layers.push(...boxes.map(() => freshLayer(info.width, info.height)))
  const positions = boxes.map(() => 0)
  const paintedThrough = boxes.map(() => -1)
  /** Tiles from the start that are finished and drawn at their finished version. */
  const drawnThrough = boxes.map(() => 0)
  window_.__mosaicBoxPath = path.map((tile) => tileKey(tile.x, tile.y))
  window_.__mosaicPainted = boxes.map(() => [])
  window_.__mosaicFrontiers = []
  const paintedPath = boxes.map(() => new Set<number>())
  const notePainted = (box: number, pathIndex: number) => {
    if (!paintedPath[box].has(pathIndex)) {
      paintedPath[box].add(pathIndex)
      window_.__mosaicPainted = paintedPath.map((set) => [...set].sort((a, b) => a - b))
    }
  }

  const showBoxes = () => {
    const at = positions.map((position, i) => {
      const tile = path[Math.min(path.length - 1, Math.max(0, Math.floor(position)))]
      return { x: tile.x, y: tile.y, color: boxes[i].color, label: boxes[i].label }
    })
    window_.__mosaicBoxes = at.map((box) => ({ ...box }))
    const arrived = positions.filter((position) => position >= path.length - 1).length
    // The picture is whole by then, and depth can take far longer than the walk.
    update({ frontier: arrived === boxes.length ? [] : at, arrived, pathLength: path.length })
  }

  const blit = (box: number, tile: { x: number; y: number }, pathIndex: number, bitmap: ImageBitmap) => {
    if (layered) {
      const layer = layers[box].getContext("2d")
      layer?.clearRect(tile.x * info.tile, tile.y * info.tile, info.tile, info.tile)
      layer?.drawImage(bitmap, tile.x * info.tile, tile.y * info.tile)
      queueComposite(tile)
    } else {
      ctx.drawImage(bitmap, tile.x * info.tile, tile.y * info.tile)
    }
    notePainted(box, pathIndex)
    bitmap.close()
  }
  // Only changed tiles recomposite: the whole canvas per frame is three full
  // layers of work for one 256 px tile.
  function paintComposite(tiles: { x: number; y: number }[]) {
    for (const { x, y } of tiles) {
      const [left, top, size] = [x * info.tile, y * info.tile, info.tile]
      ctx.globalCompositeOperation = "source-over"
      ctx.fillStyle = "#000"
      ctx.fillRect(left, top, size, size)
      ctx.globalCompositeOperation = "lighter"
      for (const layer of layers) {
        ctx.drawImage(layer, left * backingScaleX, top * backingScaleY, size * backingScaleX, size * backingScaleY, left, top, size, size)
      }
    }
    ctx.globalCompositeOperation = "source-over"
  }
  function flushDirty() {
    if (dirty.length > 0) paintComposite(dirty.splice(0))
  }
  function queueComposite(tile: { x: number; y: number }) {
    dirty.push(tile)
    if (dirty.length > 1 || signal.aborted) return
    window.requestAnimationFrame(() => {
      if (signal.aborted || finished) return
      flushDirty()
    })
  }

  const paintKeyOf = (box: number, tile: { x: number; y: number }) =>
    layered ? `${box}:${tileKey(tile.x, tile.y)}` : tileKey(tile.x, tile.y)

  /** Fetch one tile's layer as it stands. Delta paints only settled tiles. The
   * version is read when the fetch starts, so `drawn` never claims more than
   * the pixels can hold: the server serves at least that version. */
  const paint = (box: number, pathIndex: number, priority: boolean) => {
    const tile = path[pathIndex]
    const key = tileKey(tile.x, tile.y)
    if (!layered && !latest.get(key)?.settled) return
    const paintKey = paintKeyOf(box, tile)
    refreshedAt.set(paintKey, Date.now())
    want(paintKey, priority, async () => {
      const version = latest.get(key)?.version ?? 0
      const bitmap = await fetchBitmap(tile.x, tile.y, version, layered ? boxes[box].channel : undefined)
      if (signal.aborted) {
        bitmap.close()
        return
      }
      blit(box, tile, pathIndex, bitmap)
      drawn.set(paintKey, version)
    })
  }

  /** Every tile a box has passed whose drawn version is behind the latest. */
  const behind = (box: number) => {
    const stale: { pathIndex: number; paintKey: string; missing: boolean }[] = []
    for (let pathIndex = 0; pathIndex <= paintedThrough[box]; pathIndex++) {
      const tile = path[pathIndex]
      const paintKey = paintKeyOf(box, tile)
      const shown = drawn.get(paintKey)
      if (shown === undefined || shown < (latest.get(tileKey(tile.x, tile.y))?.version ?? 0)) {
        stale.push({ pathIndex, paintKey, missing: shown === undefined })
      }
    }
    return stale
  }
  window_.__mosaicStale = () => boxes.map((_, box) => behind(box).length)

  /** Draw what is missing behind the boxes, then refresh the stalest tiles. It
   * compares drawn state each time, so nothing depends on one fetch landing. */
  const refresh = () => {
    if (finished) return
    const now = Date.now()
    const due = boxes
      .flatMap((_, box) => behind(box).map((tile) => ({ ...tile, box })))
      .filter((tile) => now - (refreshedAt.get(tile.paintKey) ?? 0) >= REFRESH_EVERY_MS)
      .sort((a, b) => a.pathIndex - b.pathIndex)
    for (const tile of due) {
      // The tile a box is waiting on goes first: the path runs in that order.
      const blocking = tile.missing || tile.pathIndex <= drawnThrough[tile.box] + PAINT_AHEAD
      if (!blocking && wanted.length >= REFRESH_QUEUE) break
      paint(tile.box, tile.pathIndex, blocking)
    }
  }

  const finishedOf = (box: number, state: MosaicStatus) =>
    layered ? (state.frontiers[boxes[box].channel] ?? 0) : Math.min(...boxes.map((spec) => state.frontiers[spec.channel] ?? 0))

  /** A tile that could not be drawn counts, so one bad fetch cannot park a box;
   * the page reports it instead. */
  const onCanvas = (box: number, pathIndex: number) => {
    const tile = path[pathIndex]
    const paintKey = paintKeyOf(box, tile)
    const shown = drawn.get(paintKey)
    if (failed.has(paintKey)) return true
    if (shown === undefined) return false
    // The end of a job bumps every version for the final stretch. Composite
    // replaces these tiles; Color immediately starts its normalized delivery.
    return status.state === "done" || shown >= (latest.get(tileKey(tile.x, tile.y))?.version ?? 0)
  }

  /** The first tile not yet finished and on the canvas: where the box stands. */
  const targetOf = (box: number, state: MosaicStatus) => {
    const finished = Math.min(finishedOf(box, state), path.length)
    while (drawnThrough[box] < finished && onCanvas(box, drawnThrough[box])) drawnThrough[box] += 1
    return Math.min(drawnThrough[box], path.length - 1)
  }

  let lastFrame = performance.now()
  let boxesShown = false
  const tick = (now: number) => {
    if (signal.aborted || finished) return
    const elapsed = Math.min(1, (now - lastFrame) / 1000)
    lastFrame = now
    let moved = false
    for (let box = 0; box < boxes.length; box++) {
      const target = targetOf(box, status)
      if (reduced) {
        moved ||= Math.floor(positions[box]) !== Math.floor(target)
        positions[box] = target
      } else {
        const gap = target - positions[box]
        if (gap > 0) {
          const speed = Math.min(GLIDE_MAX, Math.max(GLIDE_MIN, gap * 2))
          const step = Math.min(speed * elapsed, GLIDE_STEP)
          const next = Math.min(target, positions[box] + step)
          moved ||= Math.floor(positions[box]) !== Math.floor(next)
          positions[box] = next
        }
      }
      const at = Math.min(path.length - 1, Math.floor(positions[box]))
      const ahead = Math.min(at + PAINT_AHEAD, finishedOf(box, status), path.length - 1)
      while (paintedThrough[box] < ahead) {
        paintedThrough[box] += 1
        paint(box, paintedThrough[box], true)
      }
    }
    if (moved || !boxesShown) {
      boxesShown = true
      showBoxes()
    }
    raf = window.requestAnimationFrame(tick)
  }
  raf = window.requestAnimationFrame(tick)

  while (!signal.aborted) {
    status = await readStatus(since)
    since = status.seq
    window_.__mosaicFrontiers = boxes.map((spec) => status.frontiers[spec.channel] ?? 0)
    for (const tile of status.tiles) {
      const key = tileKey(tile.x, tile.y)
      latest.set(key, { version: tile.version, settled: tile.final })
    }
    if (status.state === "done" || status.state === "failed" || status.state === "cancelled") {
      if (status.state !== "done") {
        finished = true
        window.cancelAnimationFrame(raf)
        update({ status, frontier: [], error: status.error ?? undefined })
      } else {
        update({ status, error: status.error ?? undefined })
        window_.__mosaicReadsDoneAt = Date.now()
        // Let the boxes glide home first; `land` starts the final delivery.
        while (!signal.aborted && !positions.every((position) => position >= path.length - 1)) {
          await new Promise((resolve) => window.setTimeout(resolve, 200))
        }
        await land(status, false)
        keepAlive()
      }
      return
    }
    update({ status, error: status.error ?? undefined })
    refresh()
    await new Promise((resolve) => window.setTimeout(resolve, POLL_MS))
  }

  function freshLayer(width: number, height: number) {
    const layer = document.createElement("canvas")
    layer.width = color ? canvas.width : width
    layer.height = color ? canvas.height : height
    if (color) layer.getContext("2d")?.setTransform(backingScaleX, 0, 0, backingScaleY, 0, 0)
    return layer
  }
}

/** A refused connection is an engine that is restarting, not a failed view. */
async function reach<T>(request: Promise<T>, signal: AbortSignal): Promise<T> {
  try {
    return await request
  } catch (err) {
    if (signal.aborted) throw err
    throw new EngineLost()
  }
}

function serverError(error: unknown): string | undefined {
  return error && typeof error === "object" && "error" in error && typeof error.error === "string" ? error.error : undefined
}

/** Text for the status line. Numbers come straight from the server: each
 * channel's settled reads over its total, then bytes and seconds. */
export function statusText(state: MosaicState): string {
  const status = state.status
  if (state.error) return "failed"
  if (state.reconnecting) return "Engine restarting, resuming shortly"
  if (!status) return "Starting mosaic"
  const seconds = `${Math.round(status.elapsed_ms / 1000)} s`
  const size = gigabytes(status.bytes_read)
  const channels = channelText(status)
  if (status.state === "planning") return `Planning ${status.exposures_total} exposures · ${seconds}`
  if (status.state === "reading" || (status.state === "done" && !state.final)) {
    const planned = status.bytes_planned > 0 ? ` of ${gigabytes(status.bytes_planned)}` : ""
    return `${channels} · ${size}${planned} · ${seconds}`
  }
  if (status.state === "cancelled") return "cancelled"
  if (status.state === "failed") return "failed"
  if (state.drawing > 0) return `Drawing ${state.drawing} tiles`
  if (status.reads_failed > 0 || state.undrawn > 0) return `incomplete · ${channels} · ${seconds}`
  return "ready"
}

function channelText(status: MosaicStatus): string {
  const percent = (done: number, total: number) => (total > 0 ? `${Math.round((Math.min(done, total) / total) * 100)}%` : "-")
  const done = status.channels_done ?? []
  const total = status.channels_total ?? []
  // Channels run blue, green, red; the line reads R, G, B.
  if (total.length === 3) return `R ${percent(done[2] ?? 0, total[2] ?? 0)} · G ${percent(done[1] ?? 0, total[1] ?? 0)} · B ${percent(done[0] ?? 0, total[0] ?? 0)}`
  if (total.length === 2) return `+ ${percent(done[0] ?? 0, total[0] ?? 0)} · − ${percent(done[1] ?? 0, total[1] ?? 0)}`
  if (total.length === 1) return `${percent(done[0] ?? 0, total[0] ?? 0)}`
  return `${status.exposures_done}/${status.exposures_total} exposures`
}

/** A finished job can still have holes. This is what the page must not hide. */
export function warningText(state: MosaicState): string | undefined {
  const failed = state.status?.reads_failed ?? 0
  const parts = [
    failed > 0 ? `${failed} of ${state.status?.exposures_total} exposure reads failed, so this picture has gaps.` : "",
    state.undrawn > 0 ? `${state.undrawn} tiles could not be drawn.` : "",
  ]
  return parts.filter(Boolean).join(" ") || undefined
}

export function isBusy(state: MosaicState): boolean {
  if (state.error || state.final) return false
  const phase = state.status?.state
  if (phase === "failed" || phase === "cancelled") return false
  return true
}

function gigabytes(bytes: number): string {
  return bytes >= 1e9 ? `${(bytes / 1e9).toFixed(1)} GB` : `${Math.round(bytes / 1e6)} MB`
}
