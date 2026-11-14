import type { CSSProperties, ReactNode, RefObject } from "react"
import "./edge-sidebar.css"

export type EdgeSide = "left" | "right"
export type EdgeWidth = "controls" | "drawer" | "editor"
export type EdgeLayout = "dock" | "overlay"

export type EdgeSidebarProps = {
  id: string
  label: string
  side: EdgeSide
  width: EdgeWidth
  layout?: EdgeLayout
  open: boolean
  obscured?: boolean
  onOpenChange: (open: boolean) => void
  tabRef?: RefObject<HTMLButtonElement | null>
  /** Vertical offset from vertical center, in px. Fixed; tongues do not jump. */
  tabOffset?: number
  /** Optional vertical word on the tongue (e.g. Extras). */
  tabLabel?: string
  /** When set without tabLabel, replaces the chevron (e.g. Share icon). */
  tabIcon?: ReactNode
  hidden?: boolean
  className?: string
  children: ReactNode
}

export function EdgeSidebar(props: EdgeSidebarProps) {
  const layout = props.layout ?? "dock"
  const openLabel = `${props.open ? "Collapse" : "Expand"} ${props.label}`
  const labeled = Boolean(props.tabLabel)
  const iconOnly = Boolean(props.tabIcon) && !labeled
  const classes = [
    "edge-sidebar",
    `edge-sidebar--${props.side}`,
    `edge-sidebar--${layout}`,
    `edge-sidebar--${props.width}`,
    labeled ? "edge-sidebar--labeled" : undefined,
    iconOnly ? "edge-sidebar--icon" : undefined,
    props.className,
  ]
    .filter(Boolean)
    .join(" ")

  return (
    <div
      className={classes}
      data-open={props.open}
      data-obscured={props.obscured ?? false}
      data-width={props.width}
      hidden={props.hidden}
      inert={props.obscured}
      style={{ "--edge-tab-offset": `${props.tabOffset ?? 0}px` } as CSSProperties}
    >
      <aside
        id={props.id}
        className="edge-sidebar__panel"
        aria-label={props.label}
        role={props.side === "right" ? "dialog" : undefined}
        aria-modal={props.side === "right" ? false : undefined}
        data-edge-panel=""
        inert={!props.open || props.obscured}
      >
        {props.children}
      </aside>
      <button
        ref={props.tabRef}
        className="edge-sidebar__tab"
        type="button"
        aria-expanded={props.open}
        aria-controls={props.id}
        aria-label={openLabel}
        title={openLabel}
        data-edge-tab=""
        onClick={() => props.onOpenChange(!props.open)}
      >
        <span
          className={`edge-sidebar__tongue${labeled ? " edge-sidebar__tongue--labeled" : ""}${iconOnly ? " edge-sidebar__tongue--icon" : ""}`}
          aria-hidden="true"
        >
          {labeled ? <span className="edge-sidebar__tab-label">{props.tabLabel}</span> : null}
          {props.tabIcon ? (
            <span className="edge-sidebar__icon">{props.tabIcon}</span>
          ) : (
            <span className="edge-sidebar__chevron">
              <svg
                viewBox="0 0 16 16"
                width="16"
                height="16"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.8"
                strokeLinecap="round"
                strokeLinejoin="round"
                focusable="false"
              >
                <path d="m6 3 5 5-5 5" />
              </svg>
            </span>
          )}
        </span>
      </button>
    </div>
  )
}
