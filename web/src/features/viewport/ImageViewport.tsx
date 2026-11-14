import { useEffect, useLayoutEffect, useRef, useState } from "react"
import type { JSX, KeyboardEvent, PointerEvent, ReactNode } from "react"
import "./viewport.css"

const MIN_ZOOM = 0.25
const MAX_ZOOM = 16
const VISIBLE_MARGIN = 32
const WHEEL_LINE_PX = 16
const ZOOM_STEP = 1.2

type PictureGeometry = {
  nativeKey: string
  nativeW: number
  nativeH: number
  viewportW: number
  viewportH: number
  baseScale: number
  zoom: number
  tx: number
  ty: number
}

type DragState = {
  pointerId: number
  x: number
  y: number
  tx: number
  ty: number
}

export type ImageViewportNative = { w: number; h: number }

export type ImageViewportContext = {
  baseScale: number
  zoom: number
  effectiveScale: number
  toViewportPoint: (nativeX: number, nativeY: number) => { x: number; y: number }
}

export type ImageViewportProps = {
  native: ImageViewportNative | undefined
  resetKey: string
  label?: string
  hud?: ReactNode
  children: (ctx: ImageViewportContext) => ReactNode
}

export function ImageViewport(props: ImageViewportProps): JSX.Element {
  const rootRef = useRef<HTMLDivElement>(null)
  const pictureRef = useRef<HTMLDivElement>(null)
  const geometryRef = useRef<PictureGeometry | undefined>(undefined)
  const dragRef = useRef<DragState | undefined>(undefined)
  const resetKeyRef = useRef(props.resetKey)
  const pendingFitRef = useRef(false)
  const wheelFrameRef = useRef<number | undefined>(undefined)
  const wheelGeometryRef = useRef<PictureGeometry | undefined>(undefined)
  const [geometry, setGeometry] = useState<PictureGeometry>()
  const [dragging, setDragging] = useState(false)

  const positionHud = (next: PictureGeometry | undefined) => {
    const root = rootRef.current
    if (!root || !next) return
    const effectiveScale = next.baseScale * next.zoom
    root.querySelectorAll<HTMLElement>("[data-viewport-native-x][data-viewport-native-y]").forEach((element) => {
      const x = Number(element.dataset.viewportNativeX)
      const y = Number(element.dataset.viewportNativeY)
      if (!Number.isFinite(x) || !Number.isFinite(y)) return
      element.style.left = `${next.tx + x * effectiveScale}px`
      element.style.top = `${next.ty + y * effectiveScale}px`
    })
  }

  const applyPictureStyle = (next: PictureGeometry | undefined) => {
    const picture = pictureRef.current
    if (!picture || !next) return
    picture.style.width = `${next.nativeW * next.baseScale * next.zoom}px`
    picture.style.height = `${next.nativeH * next.baseScale * next.zoom}px`
    picture.style.transform = `translate(${next.tx}px, ${next.ty}px)`
  }

  const writeGeometry = (next: PictureGeometry | undefined, commit = true) => {
    if (commit && wheelFrameRef.current !== undefined) {
      window.cancelAnimationFrame(wheelFrameRef.current)
      wheelFrameRef.current = undefined
      wheelGeometryRef.current = undefined
    }
    geometryRef.current = next
    applyPictureStyle(next)
    positionHud(next)
    if (commit) setGeometry(next)
  }

  const measureViewport = (forceFit = false) => {
    const root = rootRef.current
    const native = props.native
    if (!root || !native || native.w <= 0 || native.h <= 0) return
    const rect = root.getBoundingClientRect()
    if (rect.width < 1 || rect.height < 1) return
    const nativeKey = `${native.w}x${native.h}`
    const previous = geometryRef.current
    const shouldFit = forceFit || pendingFitRef.current || !previous || previous.nativeKey !== nativeKey
    const baseScale = Math.min(rect.width / native.w, rect.height / native.h)
    const zoom = shouldFit ? 1 : previous.zoom
    const displayedW = native.w * baseScale * zoom
    const displayedH = native.h * baseScale * zoom
    const centerNativeX = previous && !shouldFit ? (previous.viewportW / 2 - previous.tx) / (previous.baseScale * previous.zoom) : undefined
    const centerNativeY = previous && !shouldFit ? (previous.viewportH / 2 - previous.ty) / (previous.baseScale * previous.zoom) : undefined
    const next = clampGeometry({
      nativeKey,
      nativeW: native.w,
      nativeH: native.h,
      viewportW: rect.width,
      viewportH: rect.height,
      baseScale,
      zoom,
      tx:
        centerNativeX === undefined
          ? (rect.width - displayedW) / 2
          : rect.width / 2 - centerNativeX * baseScale * zoom,
      ty:
        centerNativeY === undefined
          ? (rect.height - displayedH) / 2
          : rect.height / 2 - centerNativeY * baseScale * zoom,
    })
    pendingFitRef.current = false
    writeGeometry(next)
  }

  useLayoutEffect(() => {
    if (!props.native) {
      pendingFitRef.current = true
      writeGeometry(undefined)
      return
    }
    measureViewport()
    const root = rootRef.current
    if (!root) return
    const observer = new ResizeObserver(() => measureViewport())
    observer.observe(root)
    return () => observer.disconnect()
  }, [props.native?.w, props.native?.h])

  useLayoutEffect(() => {
    if (resetKeyRef.current === props.resetKey) return
    resetKeyRef.current = props.resetKey
    pendingFitRef.current = true
    measureViewport(true)
  }, [props.resetKey])

  useLayoutEffect(() => {
    applyPictureStyle(geometry)
    positionHud(geometry)
  }, [geometry, props.hud])

  useEffect(() => {
    const root = rootRef.current
    if (!root) return
    const onWheel = (event: globalThis.WheelEvent) => {
      const current = geometryRef.current
      if (!current) return
      event.preventDefault()
      const rect = root.getBoundingClientRect()
      const page = rect.height || window.innerHeight
      const multiplier = event.deltaMode === 1 ? WHEEL_LINE_PX : event.deltaMode === 2 ? page : 1
      const delta = event.deltaY * multiplier
      if (delta === 0) return
      const nextZoom = clampZoom(current.zoom * Math.exp(-delta * 0.0015))
      const pointerX = event.clientX - rect.left
      const pointerY = event.clientY - rect.top
      const next = zoomGeometry(current, nextZoom, pointerX, pointerY)
      writeGeometry(next, false)
      wheelGeometryRef.current = next
      if (wheelFrameRef.current !== undefined) return
      wheelFrameRef.current = window.requestAnimationFrame(() => {
        wheelFrameRef.current = undefined
        const queued = wheelGeometryRef.current
        wheelGeometryRef.current = undefined
        if (queued) setGeometry(queued)
      })
    }
    root.addEventListener("wheel", onWheel, { passive: false })
    return () => root.removeEventListener("wheel", onWheel)
  }, [])

  useEffect(() => {
    return () => {
      if (wheelFrameRef.current !== undefined) window.cancelAnimationFrame(wheelFrameRef.current)
    }
  }, [])

  const changeZoom = (nextZoom: number) => {
    const current = geometryRef.current
    const root = rootRef.current
    if (!current || !root) return
    const rect = root.getBoundingClientRect()
    writeGeometry(zoomGeometry(current, nextZoom, rect.width / 2, rect.height / 2))
  }

  const fit = () => {
    pendingFitRef.current = true
    measureViewport(true)
  }

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (isControlTarget(event.target)) return
    if (event.altKey || event.ctrlKey || event.metaKey) return
    const current = geometryRef.current
    if (!current) return
    if (event.key === "+" || (event.key === "=" && event.shiftKey)) {
      event.preventDefault()
      changeZoom(current.zoom * ZOOM_STEP)
      return
    }
    if (event.key === "-") {
      event.preventDefault()
      changeZoom(current.zoom / ZOOM_STEP)
      return
    }
    if (event.key === "0") {
      event.preventDefault()
      fit()
      return
    }
    const distance = event.shiftKey ? 160 : 40
    const movement =
      event.key === "ArrowLeft"
        ? { x: -distance, y: 0 }
        : event.key === "ArrowRight"
          ? { x: distance, y: 0 }
          : event.key === "ArrowUp"
            ? { x: 0, y: -distance }
            : event.key === "ArrowDown"
              ? { x: 0, y: distance }
              : undefined
    if (!movement) return
    event.preventDefault()
    writeGeometry(
      clampGeometry({
        ...current,
        tx: current.tx + movement.x,
        ty: current.ty + movement.y,
      }),
    )
  }

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 2 || !geometryRef.current) return
    if (event.target instanceof Element && event.target.closest(".image-viewport-hud")) return
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    const current = geometryRef.current
    dragRef.current = {
      pointerId: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      tx: current.tx,
      ty: current.ty,
    }
    setDragging(true)
  }

  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current
    const current = geometryRef.current
    if (!drag || drag.pointerId !== event.pointerId || !current) return
    event.preventDefault()
    writeGeometry(
      clampGeometry({
        ...current,
        tx: drag.tx + event.clientX - drag.x,
        ty: drag.ty + event.clientY - drag.y,
      }),
      false,
    )
  }

  const endDrag = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current
    if (!drag || drag.pointerId !== event.pointerId) return
    if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId)
    dragRef.current = undefined
    setDragging(false)
    if (wheelFrameRef.current !== undefined) {
      window.cancelAnimationFrame(wheelFrameRef.current)
      wheelFrameRef.current = undefined
      wheelGeometryRef.current = undefined
    }
    setGeometry(geometryRef.current)
  }

  const context = geometry
    ? {
        baseScale: geometry.baseScale,
        zoom: geometry.zoom,
        effectiveScale: geometry.baseScale * geometry.zoom,
        toViewportPoint: (nativeX: number, nativeY: number) => ({
          x: geometry.tx + nativeX * geometry.baseScale * geometry.zoom,
          y: geometry.ty + nativeY * geometry.baseScale * geometry.zoom,
        }),
      }
    : undefined
  const childContext =
    context ?? {
      baseScale: 1,
      zoom: 1,
      effectiveScale: 1,
      toViewportPoint: (nativeX: number, nativeY: number) => ({ x: nativeX, y: nativeY }),
    }

  return (
    <div
      ref={rootRef}
      className="image-viewport"
      tabIndex={0}
      aria-label={props.label ?? "Image viewport"}
      data-zoom={geometry?.zoom ?? 1}
      data-dragging={dragging}
      onKeyDown={onKeyDown}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onContextMenu={(event) => event.preventDefault()}
    >
      <div
        ref={pictureRef}
        className="image-viewport-picture"
        style={geometry ? undefined : { width: "100%", height: "100%" }}
      >
        {props.children(childContext)}
      </div>
      <div className="image-viewport-hud">
        {props.hud}
        <div className="image-viewport-controls" aria-label="Image view controls">
          <button type="button" aria-label="Zoom in" title="Zoom in" onClick={() => changeZoom((geometryRef.current?.zoom ?? 1) * ZOOM_STEP)}>
            +
          </button>
          <span aria-live="polite">{Math.round((geometry?.zoom ?? 1) * 100)}%</span>
          <button type="button" aria-label="Zoom out" title="Zoom out" onClick={() => changeZoom((geometryRef.current?.zoom ?? 1) / ZOOM_STEP)}>
            −
          </button>
          <button type="button" aria-label="Fit image" title="Fit image" onClick={fit}>
            Fit
          </button>
        </div>
      </div>
    </div>
  )
}

function clampZoom(zoom: number): number {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom))
}

function clampGeometry(geometry: PictureGeometry, clamp = true): PictureGeometry {
  if (!clamp) return geometry
  const width = geometry.nativeW * geometry.baseScale * geometry.zoom
  const height = geometry.nativeH * geometry.baseScale * geometry.zoom
  return {
    ...geometry,
    tx: clampOffset(geometry.tx, geometry.viewportW, width),
    ty: clampOffset(geometry.ty, geometry.viewportH, height),
  }
}

function clampOffset(value: number, viewport: number, displayed: number): number {
  const min = VISIBLE_MARGIN - displayed
  const max = viewport - VISIBLE_MARGIN
  if (min > max) return (min + max) / 2
  return Math.min(max, Math.max(min, value))
}

function zoomGeometry(geometry: PictureGeometry, nextZoom: number, x: number, y: number): PictureGeometry {
  const zoom = clampZoom(nextZoom)
  const effectiveScale = geometry.baseScale * geometry.zoom
  const nativeX = (x - geometry.tx) / effectiveScale
  const nativeY = (y - geometry.ty) / effectiveScale
  const nextScale = geometry.baseScale * zoom
  return clampGeometry({
    ...geometry,
    zoom,
    tx: x - nativeX * nextScale,
    ty: y - nativeY * nextScale,
  })
}

function isControlTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false
  return target.closest("input, textarea, select, button, [contenteditable='true'], [role='button']") !== null
}
