import { useEffect, useLayoutEffect, useRef, useState } from "react"
import type { JSX, KeyboardEvent, PointerEvent, ReactNode } from "react"
import {
  clampGeometry,
  clampZoom,
  measureGeometry,
  panGeometry,
  rebaseDrag,
  ZOOM_STEP,
  zoomGeometry,
  type DragState,
  type ImageViewportNative,
  type PictureGeometry,
} from "./viewport-geometry.ts"
import "./viewport.css"

const WHEEL_LINE_PX = 16

export type { ImageViewportNative }

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

  const clearDrag = (target?: HTMLDivElement | null, pointerId?: number) => {
    const drag = dragRef.current
    if (!drag) return false
    if (pointerId !== undefined && drag.pointerId !== pointerId) return false
    dragRef.current = undefined
    if (target && target.hasPointerCapture(drag.pointerId)) {
      target.releasePointerCapture(drag.pointerId)
    }
    setDragging(false)
    return true
  }

  const finishDrag = (target?: HTMLDivElement | null, pointerId?: number) => {
    if (!clearDrag(target, pointerId)) return
    if (wheelFrameRef.current !== undefined) {
      window.cancelAnimationFrame(wheelFrameRef.current)
      wheelFrameRef.current = undefined
    }
    setGeometry(geometryRef.current)
  }

  const writeGeometry = (next: PictureGeometry | undefined, commit = true) => {
    if (commit && wheelFrameRef.current !== undefined) {
      window.cancelAnimationFrame(wheelFrameRef.current)
      wheelFrameRef.current = undefined
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
    if (!shouldFit && previous && previous.viewportW === rect.width && previous.viewportH === rect.height) {
      pendingFitRef.current = false
      return
    }
    const result = measureGeometry(previous, native, rect.width, rect.height, shouldFit)
    pendingFitRef.current = false
    if (result.fitted) {
      clearDrag(root)
    } else if (dragRef.current) {
      dragRef.current = rebaseDrag(dragRef.current, result.geometry)
    }
    writeGeometry(result.geometry)
  }

  useLayoutEffect(() => {
    if (!props.native) {
      pendingFitRef.current = true
      clearDrag(rootRef.current)
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
    const active = geometryRef.current ?? geometry
    applyPictureStyle(active)
    positionHud(active)
  }, [geometry])

  useLayoutEffect(() => {
    positionHud(geometryRef.current ?? geometry)
  }, [props.hud])

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
      if (dragRef.current) {
        dragRef.current = rebaseDrag(dragRef.current, next)
      }
      writeGeometry(next, false)
      if (wheelFrameRef.current !== undefined) return
      wheelFrameRef.current = window.requestAnimationFrame(() => {
        wheelFrameRef.current = undefined
        if (geometryRef.current) setGeometry(geometryRef.current)
      })
    }
    const onBlur = () => finishDrag(root)
    root.addEventListener("wheel", onWheel, { passive: false })
    window.addEventListener("blur", onBlur)
    return () => {
      root.removeEventListener("wheel", onWheel)
      window.removeEventListener("blur", onBlur)
      if (wheelFrameRef.current !== undefined) {
        window.cancelAnimationFrame(wheelFrameRef.current)
        wheelFrameRef.current = undefined
      }
    }
  }, [])

  const changeZoom = (nextZoom: number) => {
    const current = geometryRef.current
    const root = rootRef.current
    if (!current || !root) return
    const rect = root.getBoundingClientRect()
    const next = zoomGeometry(current, nextZoom, rect.width / 2, rect.height / 2)
    if (dragRef.current) dragRef.current = rebaseDrag(dragRef.current, next)
    writeGeometry(next)
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
    const next = clampGeometry({
      ...current,
      tx: current.tx + movement.x,
      ty: current.ty + movement.y,
    })
    if (dragRef.current) dragRef.current = rebaseDrag(dragRef.current, next)
    writeGeometry(next)
  }

  const onPointerDown = (event: PointerEvent<HTMLDivElement>) => {
    if (event.button !== 2 || !geometryRef.current) return
    if (event.target instanceof Element && event.target.closest(".image-viewport-hud")) return
    event.preventDefault()
    if (dragRef.current && dragRef.current.pointerId !== event.pointerId) {
      clearDrag(event.currentTarget, dragRef.current.pointerId)
    }
    event.currentTarget.setPointerCapture(event.pointerId)
    const current = geometryRef.current
    dragRef.current = {
      pointerId: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      tx: current.tx,
      ty: current.ty,
      lastX: event.clientX,
      lastY: event.clientY,
    }
    setDragging(true)
  }

  const onPointerMove = (event: PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current
    const current = geometryRef.current
    if (!drag || drag.pointerId !== event.pointerId || !current) return
    if (event.pointerType === "mouse" && (event.buttons & 2) === 0) {
      finishDrag(event.currentTarget, event.pointerId)
      return
    }
    event.preventDefault()
    drag.lastX = event.clientX
    drag.lastY = event.clientY
    writeGeometry(panGeometry(current, drag, event.clientX, event.clientY), false)
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
      onPointerUp={(event) => finishDrag(event.currentTarget, event.pointerId)}
      onPointerCancel={(event) => finishDrag(event.currentTarget, event.pointerId)}
      onLostPointerCapture={(event) => finishDrag(event.currentTarget, event.pointerId)}
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

function isControlTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false
  return target.closest("input, textarea, select, button, [contenteditable=true], [role=button]") !== null
}
