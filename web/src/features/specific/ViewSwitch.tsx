export type SpecificView = "blink" | "lab"

/** Blink is the motion console; Lab lists every exposure, composes sectors by
 * hand, and plays them as one calibrated sequence. */
export function SpecificViewSwitch(props: { value: SpecificView; onChange: (next: SpecificView) => void }) {
  return (
    <div className="specific-view-switch" role="group" aria-label="Specific view">
      {(["blink", "lab"] as const).map((id) => (
        <button
          key={id}
          type="button"
          aria-pressed={props.value === id}
          title={id === "blink" ? "Blink epochs and track movers" : "Exposure lab: pick exposures, build sectors, play sequences"}
          onClick={() => props.onChange(id)}
        >
          {id === "blink" ? "Blink" : "Lab"}
        </button>
      ))}
    </div>
  )
}
