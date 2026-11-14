import { useEffect, useId, useRef, useState } from "react"
import { DEFAULT_DISPLAY, toneBlurCommit, type BlackWhite, type DisplayState, type ResolvedLayer, type Stretch } from "../../url-state/display"
import type { DisplayControlsProps } from "./raw-protocol"
import "./raw-display.css"

export function RawLoadNotice(props: DisplayControlsProps & { automatic?: boolean }) {
  if (props.disabledReason) return <div className="raw-load" aria-live="polite"><p role="status" className="raw-note">{props.disabledReason}</p></div>
  const session = props.session
  const automatic = props.automatic === true
  return <div className="raw-load" aria-live="polite">
    <p>MJy/sr = sky brightness (megajanskys per steradian).</p>
    {session.phase === "awaiting-opt-in" ? <>
      <p>Local editing needs {session.estimateBytes.toLocaleString("en-US")} bytes ({(session.estimateBytes / 1048576).toFixed(1)} MiB).</p>
      <button type="button" onClick={props.onStartRaw}>Download raw layers (≈ {(session.estimateBytes / 1048576).toFixed(1)} MiB)</button>
      <p>The server image stays fully usable without them.</p>
    </> : null}
    {session.phase === "loading" ? <><p>{session.totalTiles ? `Loading ${automatic ? "three color layers" : "raw layers"} · ${session.loadedTiles} / ${session.totalTiles} tiles` : "Reading raw layer information…"}</p>{automatic ? null : <button type="button" onClick={props.onCancelRaw}>Cancel download</button>}</> : null}
    {session.phase === "unavailable" ? <><p>{session.reason}</p><button type="button" onClick={props.onStartRaw}>Retry local editing</button></> : null}
    {session.phase === "idle" ? props.metadata ? automatic ? <p>Preparing the three color layers…</p> : <><p>Local layers are not loaded.</p><button type="button" onClick={props.onStartRaw}>Load raw layers</button></> : <p>Local editing is available after the server image finishes.</p> : null}
    {session.phase === "ready" ? <p>{props.gate.label}</p> : null}
    {!automatic && props.metadata && session.phase !== "ready" && session.phase !== "unavailable" && !props.gate.exportReady ? <p>{props.gate.label}</p> : null}
  </div>
}

function formatPoint(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return ""
  const abs = Math.abs(value)
  if (abs !== 0 && (abs >= 1000 || abs < 0.001)) return value.toExponential(3)
  return String(Number(value.toPrecision(6)))
}

/** Shared single-plane tone form; no science or network code is imported. */
export function ToneControls(props: {
  label: string
  value: Pick<DisplayState["composite"], "gain" | "blackWhite" | "stretch">
  resolved?: ResolvedLayer
  onChange(next: Pick<DisplayState["composite"], "gain" | "blackWhite" | "stretch">): void
  /** Extra rule for this control; Composite keeps the shared finite-point rule. */
  pointPolicy?: (low: number, high: number) => string | undefined
}) {
  const id = useId()
  // Manual points live in the settings; metadata only fills Auto points.
  const manual = props.value.blackWhite.mode === "manual" ? props.value.blackWhite : null
  const low = manual?.low ?? props.resolved?.low ?? null
  const high = manual?.high ?? (low === null || props.resolved?.span == null ? null : low + props.resolved.span)
  const [black, setBlack] = useState(formatPoint(low))
  const [white, setWhite] = useState(formatPoint(high))
  const [error, setError] = useState("")
  // Focus alone must not promote rounded Auto points into Manual URL settings.
  const edited = useRef(false)
  useEffect(() => {
    setBlack(formatPoint(low))
    setWhite(formatPoint(high))
    setError("")
    edited.current = false
  }, [low, high, props.value.blackWhite.mode])
  const patch = (next: Partial<typeof props.value>) => props.onChange({ ...props.value, ...next })
  // A stored manual range the server cannot use still needs a field-level reason.
  const policyError = manual ? props.pointPolicy?.(manual.low, manual.high) : undefined
  const commitPoints = () => {
    const result = toneBlurCommit(black, white, edited.current, props.pointPolicy)
    if (result.kind === "manual") {
      setError("")
      edited.current = false
      patch({ blackWhite: result.blackWhite })
      return
    }
    if (result.kind === "error") setError(result.message)
  }
  const onGain = (raw: string) => {
    const next = Number(raw)
    if (raw !== "" && Number.isFinite(next) && next >= 0 && next <= 2) patch({ gain: next })
  }
  const onGamma = (raw: string) => {
    const next = Number(raw)
    if (raw !== "" && Number.isFinite(next) && next >= 0.2 && next <= 3) patch({ stretch: { kind: "gamma", gamma: next } })
  }
  return (
    <div className="raw-tone">
      <div className="raw-gain">
        <label htmlFor={`${id}-gain`}>Gain</label>
        <input
          id={`${id}-gain`}
          aria-label={`${props.label} gain`}
          type="range"
          min="0"
          max="2"
          step="0.05"
          value={props.value.gain}
          onChange={(event) => onGain(event.target.value)}
        />
        <input
          aria-label={`${props.label} gain value`}
          type="number"
          min="0"
          max="2"
          step="0.05"
          value={props.value.gain}
          onChange={(event) => onGain(event.target.value)}
        />
      </div>
      <div className="raw-points">
        <label>
          Black (MJy/sr)
          <input
            aria-label={`${props.label} black point, MJy/sr`}
            type="number"
            step="any"
            value={black}
            onChange={(event) => {
              edited.current = true
              setBlack(event.target.value)
            }}
            onBlur={commitPoints}
          />
        </label>
        <label>
          White (MJy/sr)
          <input
            aria-label={`${props.label} white point, MJy/sr`}
            type="number"
            step="any"
            value={white}
            onChange={(event) => {
              edited.current = true
              setWhite(event.target.value)
            }}
            onBlur={commitPoints}
          />
        </label>
      </div>
      <div className="raw-actions">
        <button
          type="button"
          onClick={() => {
            edited.current = false
            setBlack(formatPoint(low))
            setWhite(formatPoint(high))
            setError("")
            patch({ blackWhite: { mode: "auto" } satisfies BlackWhite })
          }}
        >
          Auto stretch
        </button>
        <span>{props.value.blackWhite.mode === "auto" ? "Auto" : "Manual"}</span>
      </div>
      {props.resolved && low === null ? <p className="raw-note">No finite pixels · Auto points unavailable</p> : null}
      {error || policyError ? (
        <p role="alert" className="raw-error">
          {error || policyError}
        </p>
      ) : null}
      <div className="raw-stretch">
        <label>
          Stretch
          <select
            aria-label={`${props.label} stretch`}
            value={props.value.stretch.kind}
            onChange={(event) => {
              const kind = event.target.value as Stretch["kind"]
              patch({ stretch: kind === "gamma" ? { kind, gamma: 0.7 } : { kind } })
            }}
          >
            <option value="linear">Linear</option>
            <option value="gamma">Gamma</option>
            <option value="asinh">Asinh</option>
          </select>
        </label>
        {props.value.stretch.kind === "gamma" ? (
          <label>
            Gamma
            <input
              aria-label={`${props.label} gamma`}
              type="number"
              min="0.2"
              max="3"
              step="0.05"
              value={props.value.stretch.gamma}
              onChange={(event) => onGamma(event.target.value)}
            />
          </label>
        ) : (
          <span />
        )}
      </div>
    </div>
  )
}

export function CompositeDisplayControls(props: DisplayControlsProps) {
  // Display prefs are always editable. Local paint only applies once raw layers
  // are ready; edits before that still stick in URL/state for when they load.
  return (
    <section className="raw-display">
      <RawLoadNotice {...props} />
      <fieldset disabled={!!props.disabledReason}>
        <legend>Local display — values unchanged</legend>
        <ToneControls
          label="Composite"
          value={props.display.composite}
          resolved={props.resolved.layers[0]}
          onChange={(next) => props.onDisplayChange({ ...props.display, composite: { ...props.display.composite, ...next } })}
        />
        <label className="raw-check">
          <input
            type="checkbox"
            checked={props.display.composite.invert}
            onChange={(event) =>
              props.onDisplayChange({ ...props.display, composite: { ...props.display.composite, invert: event.target.checked } })
            }
          />
          Invert
        </label>
        <button type="button" onClick={() => props.onDisplayChange({ ...props.display, composite: DEFAULT_DISPLAY.composite })}>
          Reset display
        </button>
      </fieldset>
    </section>
  )
}

export default CompositeDisplayControls
