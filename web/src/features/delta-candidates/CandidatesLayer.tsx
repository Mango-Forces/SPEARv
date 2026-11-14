import type { Candidate, CandidateState } from "./useCandidates"
import "./candidates.css"

/** Markers for the blobs behind a delta. Square means `moving_candidate`,
 * triangle means `unknown`, a cross means `star_residue`. Shape carries the
 * class, so the labels survive greyscale and colour blindness. Nothing is
 * removed from the image: these are annotations on top of it. */
export function CandidatesLayer(props: {
  state: CandidateState
  native: { w: number; h: number }
  scale: number
  onPick: (pick: { x: number; y: number }) => void
}) {
  const { state, native, scale } = props
  if (state.status !== "ready") return null
  const rows = state.body.candidates.filter((candidate) => candidate.class !== "star_residue")
  if (rows.length === 0) return null
  const units = 1 / Math.max(scale, 0.01)
  return (
    <svg
      className="candidates"
      viewBox={`0 0 ${native.w} ${native.h}`}
      preserveAspectRatio="none"
      aria-label="Delta candidates"
    >
      {rows.map((candidate) => (
        <g
          key={`${candidate.sign}${candidate.x}-${candidate.y}`}
          className={`candidate ${candidate.class} ${candidate.sign === "+" ? "plus" : "minus"}`}
          role="button"
          tabIndex={0}
          aria-label={`${describe(candidate)} at pixel ${candidate.x}, ${candidate.y}`}
          onClick={(event) => {
            event.stopPropagation()
            props.onPick({ x: Math.floor(candidate.x), y: Math.floor(candidate.y) })
          }}
          onKeyDown={(event) => {
            if (event.key !== "Enter" && event.key !== " ") return
            event.preventDefault()
            props.onPick({ x: Math.floor(candidate.x), y: Math.floor(candidate.y) })
          }}
        >
          <title>{describe(candidate)}</title>
          <circle className="candidate-ring" cx={candidate.x} cy={candidate.y} r={20 * units} />
          <text
            className="candidate-label"
            x={candidate.x}
            y={candidate.y - 25 * units}
            fontSize={11 * units}
          >
            {candidate.class === "unknown" ? "?" : `${candidate.sign}${Math.round(candidate.significance)}σ`}
          </text>
        </g>
      ))}
    </svg>
  )
}

function describe(candidate: Candidate): string {
  const what =
    candidate.class === "moving_candidate"
      ? "moving candidate, no fixed-star evidence"
      : "unknown, no catalog coverage here"
  return `${what}; ${candidate.sign}${candidate.significance.toFixed(0)}σ, ${candidate.mjd.toFixed(2)} MJD, ${candidate.reason}`
}
