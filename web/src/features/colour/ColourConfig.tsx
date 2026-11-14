import { useEffect, useState } from "react"
import { DEFAULT_COLOUR_CONFIG, type ColourBands, type ColourConfigValue } from "../../url-state/colour"
import "./colour.css"

/** Three-channel false-colour controls. App owns the value and passes it to
 * `useMosaic`, which sends the bands on `GET /api/v1/mosaic`. */

const CHANNELS = [
  { key: "r", name: "Red" },
  { key: "g", name: "Green" },
  { key: "b", name: "Blue" },
] as const

function sourceDraft(value: ColourConfigValue) {
  return Object.fromEntries(CHANNELS.map((channel) => [channel.key, { pin: String(value.colorBands[channel.key].pin + 1), width: String(value.colorBands[channel.key].width) }])) as Record<"r" | "g" | "b", { pin: string; width: string }>
}
export function ColourConfig(props: {
  value?: ColourConfigValue
  /** Called only on Apply, with the same API-numbered value as before. */
  onChange: (value: ColourConfigValue) => void
  disabled?: boolean
}) {
  const [internal, setInternal] = useState(DEFAULT_COLOUR_CONFIG)
  const applied = props.value ?? internal
  const appliedKey = JSON.stringify(applied)
  const [draft, setDraft] = useState(() => sourceDraft(applied))
  const [error, setError] = useState("")
  useEffect(() => { setDraft(sourceDraft(applied)); setError("") }, [appliedKey])
  const apply = () => {
    if (CHANNELS.some((channel) => [draft[channel.key].pin, draft[channel.key].width].some((text) => text.trim() === "" || !Number.isInteger(Number(text)) || Number(text) < 1 || Number(text) > 102))) {
      setError("Band centres and widths must be integers from 1 to 102."); return
    }
    const next = { colorBands: Object.fromEntries(CHANNELS.map((channel) => [channel.key, { pin: Number(draft[channel.key].pin) - 1, width: Number(draft[channel.key].width) }])) as ColourBands }
    setError("")
    if (props.value === undefined) setInternal(next)
    props.onChange(next)
  }
  return <fieldset className="colour-config" disabled={props.disabled}>
    <legend className="colour-legend">Source bands</legend>
    <div className="colour-head" aria-hidden="true"><span /><span /><span>Band</span><span>Width</span></div>
    {CHANNELS.map((channel) => <div className="colour-row" data-channel={channel.key} key={channel.key}>
      <span className={`colour-chip ${channel.key}`} aria-hidden="true" /><span className="colour-name">{channel.name}</span>
      {(["pin", "width"] as const).map((field) => <label key={field}><span>{field === "pin" ? "Band" : "Width"}</span>
        <input type="number" min={1} max={102} step={1} value={draft[channel.key][field]} aria-label={`${channel.name} ${field === "pin" ? "band centre, 1 to 102" : "band width"}`} onChange={(event) => {
          setDraft({ ...draft, [channel.key]: { ...draft[channel.key], [field]: event.target.value } }); setError("")
        }} />
      </label>)}
    </div>)}
    {error ? <p className="colour-error" role="alert">{error}</p> : null}
    <div className="colour-actions"><button type="button" onClick={apply}>Apply bands</button></div>
    <p className="colour-note">Apply anytime: dumps an in-flight mosaic and starts a new one with these band windows.</p>
  </fieldset>
}
