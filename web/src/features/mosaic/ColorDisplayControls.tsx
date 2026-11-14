import { useEffect, useState } from "react"
import { colorPointError, DEFAULT_DISPLAY, DISPLAY_KEYS, type DisplayState, type LayerDisplay, type ResolvedDisplay } from "../../url-state/display"
import type { ColorDisplayView } from "./useColorDisplay"
import type { DisplayManifest } from "./server-display-protocol"
import { ToneControls } from "./CompositeDisplayControls"
import "./raw-display.css"

function TintControl(props: { label: string; tint: string; onChange(tint: string): void }) {
  const [hex, setHex] = useState(props.tint)
  useEffect(() => setHex(props.tint), [props.tint])
  return (
    <div className="raw-tint">
      <input
        aria-label={`${props.label} tint color`}
        type="color"
        value={props.tint}
        onChange={(event) => props.onChange(event.target.value)}
        title="Pick tint"
      />
      <input
        aria-label={`${props.label} tint hex`}
        type="text"
        maxLength={7}
        spellCheck={false}
        value={hex}
        onChange={(event) => {
          setHex(event.target.value)
          if (/^#[\da-f]{6}$/i.test(event.target.value)) props.onChange(event.target.value.toLowerCase())
        }}
        onBlur={() => setHex(props.tint)}
      />
    </div>
  )
}

export type ColorDisplayControlsProps = {
  display: DisplayState
  resolved: ResolvedDisplay
  metadata: DisplayManifest | undefined
  colorDisplay?: ColorDisplayView
  onDisplayChange(next: DisplayState): void
}

export function ColorDisplayControls(props: ColorDisplayControlsProps) {
  const status = props.colorDisplay
  return (
    <section className="raw-display">
      <p className="raw-note">Server-rendered false colour from three band-window means. Display edits re-tone completed channel values without starting a science job.</p>
      <ColorDisplayStatus display={status} />
      <fieldset>
        <legend>Server-rendered display</legend>
        {DISPLAY_KEYS.map((key, index) => {
          const label = ["Red", "Green", "Blue"][index]
          const channel = props.metadata?.channels.find((item) => item.index === 2 - index)
          const value = props.display.color[key]
          const update = (patch: Partial<LayerDisplay>) =>
            props.onDisplayChange({ ...props.display, color: { ...props.display.color, [key]: { ...value, ...patch } } })
          return (
            <div className={`raw-layer${value.enabled ? "" : " raw-layer--off"}`} key={key} data-enabled={value.enabled}>
              <div className="raw-layer-head">
                <h4 className="raw-layer-title">{label}</h4>
                <label className="raw-layer-toggle" title={value.enabled ? `Disable ${label}` : `Enable ${label}`}>
                  <input
                    type="checkbox"
                    checked={value.enabled}
                    onChange={(event) => update({ enabled: event.target.checked })}
                    aria-label={`${label} enabled`}
                  />
                </label>
                <button
                  className="raw-layer-reset"
                  type="button"
                  onClick={() =>
                    props.onDisplayChange({
                      ...props.display,
                      color: { ...props.display.color, [key]: DEFAULT_DISPLAY.color[key] },
                    })
                  }
                >
                  Reset
                </button>
              </div>
              <TintControl label={label} tint={value.tint} onChange={(tint) => update({ tint })} />
              {channel ? (
                <p className="raw-note">
                  {label} source · bands {channel.bands[0]}–{channel.bands.at(-1)} · {channel.wavelength_min_um.toFixed(2)}–
                  {channel.wavelength_max_um.toFixed(2)} µm{channel.finite_count === 0 ? " · no finite pixels" : ""}
                </p>
              ) : null}
              <div className="raw-layer-body">
                <ToneControls
                  label={label}
                  value={value}
                  resolved={props.resolved.layers.find((layer) => layer.channel === 2 - index)}
                  onChange={update}
                  pointPolicy={colorPointError}
                />
              </div>
            </div>
          )
        })}
        <button type="button" onClick={() => props.onDisplayChange(DEFAULT_DISPLAY)}>
          Reset all display
        </button>
      </fieldset>
    </section>
  )
}

function ColorDisplayStatus(props: { display: ColorDisplayView | undefined }) {
  const state = props.display
  if (!state) return <p className="raw-note" role="status">Settings queued while science builds.</p>
  if (state.phase === "queued") return <p className="raw-note" role="status">Settings queued while science builds. They will be sent after the completed channel values are ready.</p>
  if (state.phase === "creating") return <p className="raw-note" role="status">{state.gate.label}</p>
  if (state.phase === "rendering") return <p className="raw-note" role="status">{state.gate.label}</p>
  if (state.phase === "ready") return <p className="raw-note" role="status">{state.error ? `${state.error} ` : ""}Current server-rendered PNG is ready. Color export saves these exact PNG bytes.</p>
  if (state.phase === "failed" || state.phase === "expired") return <div className="raw-load" role="alert">
    <p className="raw-error">{state.error ?? state.gate.label}</p>
    <button type="button" onClick={state.retry}>Retry current display</button>
  </div>
  return null
}

export default ColorDisplayControls
