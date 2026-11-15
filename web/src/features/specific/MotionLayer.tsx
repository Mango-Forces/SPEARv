import { heading, PIXEL_ARCSEC, type Mover } from "./motion"

/** Arrow length: where the source will be after this many years. Real tracks
 * are a pixel or two across one release, too short to see at a glance. */
export const ARROW_YEARS = 10

/** Rings on fitted movers, the fitted track through every epoch, and an
 * arrow ending in a dot where the motion carries it in `ARROW_YEARS`. A solid ring is
 * a 4σ candidate; dashed is a weaker 3σ to 4σ one. */
export function MotionLayer(props: {
  movers: Mover[]
  native: { w: number; h: number }
  scale: number
  onPick: (pick: { x: number; y: number }) => void
}) {
  const { movers, native, scale } = props
  if (movers.length === 0) return null
  const units = 1 / Math.max(scale, 0.01)
  return (
    <svg className="motion-layer" viewBox={`0 0 ${native.w} ${native.h}`} preserveAspectRatio="none" aria-label="Movers">
      {movers.map((mover, at) => {
        // Grid pixel centres sit at +.5 on the SVG.
        const x = mover.x + 0.5
        const y = mover.y + 0.5
        const tipX = x - (mover.pmRa / PIXEL_ARCSEC) * ARROW_YEARS
        const tipY = y - (mover.pmDec / PIXEL_ARCSEC) * ARROW_YEARS
        const label = describe(mover)
        const pick = () => props.onPick({ x: Math.floor(mover.x), y: Math.floor(mover.y) })
        return (
          <g
            key={at}
            className={`mover ${mover.tier}`}
            role="button"
            tabIndex={0}
            aria-label={label}
            onClick={(event) => {
              event.stopPropagation()
              pick()
            }}
            onKeyDown={(event) => {
              if (event.key !== "Enter" && event.key !== " ") return
              event.preventDefault()
              pick()
            }}
          >
            <title>{label}</title>
            <polyline
              className="mover-track"
              points={mover.track.map((point) => `${point.x + 0.5},${point.y + 0.5}`).join(" ")}
            />
            <line className="mover-arrow" x1={x} y1={y} x2={tipX} y2={tipY} />
            <circle className="mover-tip" cx={tipX} cy={tipY} r={3 * units} />
            <circle className="mover-ring" cx={x} cy={y} r={14 * units} />
            <text className="mover-label" x={x} y={y - 19 * units} fontSize={11 * units} strokeWidth={3 * units}>
              {mover.pm.toFixed(1)}″/yr {heading(mover.pmRa, mover.pmDec)}
            </text>
          </g>
        )
      })}
    </svg>
  )
}

export function describe(mover: Mover): string {
  const what = mover.tier === "candidate" ? "Moving candidate" : "Possible mover"
  return `${what}: ${mover.pm.toFixed(1)} ± ${mover.error.toFixed(1)}″/yr toward ${heading(mover.pmRa, mover.pmDec)}, ${mover.significance.toFixed(1)}σ over ${mover.epochs} epochs${mover.saturated ? ", saturated core" : ""}`
}
