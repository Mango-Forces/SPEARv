export const MIN_ZOOM = 0.25
export const MAX_ZOOM = 16
export const VISIBLE_MARGIN = 32
export const ZOOM_STEP = 1.2

export type ImageViewportNative = { w: number; h: number }

export type PictureGeometry = {
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

export type DragState = {
  pointerId: number
  x: number
  y: number
  tx: number
  ty: number
  lastX: number
  lastY: number
}

export function clampZoom(zoom: number): number {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom))
}

export function clampOffset(value: number, viewport: number, displayed: number): number {
  const min = VISIBLE_MARGIN - displayed
  const max = viewport - VISIBLE_MARGIN
  if (min > max) return (min + max) / 2
  return Math.min(max, Math.max(min, value))
}

export function clampGeometry(geometry: PictureGeometry, clamp = true): PictureGeometry {
  if (!clamp) return geometry
  const width = geometry.nativeW * geometry.baseScale * geometry.zoom
  const height = geometry.nativeH * geometry.baseScale * geometry.zoom
  return {
    ...geometry,
    tx: clampOffset(geometry.tx, geometry.viewportW, width),
    ty: clampOffset(geometry.ty, geometry.viewportH, height),
  }
}

export function zoomGeometry(geometry: PictureGeometry, nextZoom: number, x: number, y: number): PictureGeometry {
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

export function panGeometry(geometry: PictureGeometry, drag: DragState, clientX: number, clientY: number): PictureGeometry {
  return clampGeometry({
    ...geometry,
    tx: drag.tx + clientX - drag.x,
    ty: drag.ty + clientY - drag.y,
  })
}

export function rebaseDrag(drag: DragState, geometry: PictureGeometry): DragState {
  return {
    ...drag,
    x: drag.lastX,
    y: drag.lastY,
    tx: geometry.tx,
    ty: geometry.ty,
  }
}

export function measureGeometry(
  previous: PictureGeometry | undefined,
  native: ImageViewportNative,
  viewportW: number,
  viewportH: number,
  forceFit: boolean,
): { geometry: PictureGeometry; fitted: boolean } {
  const nativeKey = `${native.w}x${native.h}`
  const shouldFit = forceFit || !previous || previous.nativeKey !== nativeKey
  const baseScale = Math.min(viewportW / native.w, viewportH / native.h)
  const zoom = shouldFit ? 1 : previous.zoom
  const displayedW = native.w * baseScale * zoom
  const displayedH = native.h * baseScale * zoom
  const centerNativeX =
    previous && !shouldFit ? (previous.viewportW / 2 - previous.tx) / (previous.baseScale * previous.zoom) : undefined
  const centerNativeY =
    previous && !shouldFit ? (previous.viewportH / 2 - previous.ty) / (previous.baseScale * previous.zoom) : undefined
  const geometry = clampGeometry({
    nativeKey,
    nativeW: native.w,
    nativeH: native.h,
    viewportW,
    viewportH,
    baseScale,
    zoom,
    tx:
      centerNativeX === undefined
        ? (viewportW - displayedW) / 2
        : viewportW / 2 - centerNativeX * baseScale * zoom,
    ty:
      centerNativeY === undefined
        ? (viewportH - displayedH) / 2
        : viewportH / 2 - centerNativeY * baseScale * zoom,
  })
  return { geometry, fitted: shouldFit }
}
