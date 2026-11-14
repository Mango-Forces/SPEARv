import type { ReactNode } from "react"

/** Inline stroke icons for the shell chrome. They inherit the button text
 * colour, so the lime accent only marks the active toggle. */
function IconShape(props: { children: ReactNode }) {
  return (
    <svg
      viewBox="0 0 16 16"
      width="15"
      height="15"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {props.children}
    </svg>
  )
}

export function ShareIcon() {
  return (
    <IconShape>
      <circle cx="12.5" cy="3.5" r="2" />
      <circle cx="3.5" cy="8" r="2" />
      <circle cx="12.5" cy="12.5" r="2" />
      <path d="M5.3 7 10.7 4.4M5.3 9l5.4 2.6" />
    </IconShape>
  )
}

export function LayersIcon() {
  return (
    <IconShape>
      <path d="M8 1.8 14.5 5 8 8.2 1.5 5Z" />
      <path d="m1.5 8.5 6.5 3.2 6.5-3.2M1.5 11.5 8 14.7l6.5-3.2" />
    </IconShape>
  )
}

export function SpectrumIcon() {
  return (
    <IconShape>
      <path d="M2 13.5V9M5.5 13.5V5.5M9 13.5V3M12.5 13.5V7.5" />
      <path d="M1.5 13.5h13" />
    </IconShape>
  )
}
