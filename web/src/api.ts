/// <reference types="vite/client" />
import createClient from "openapi-fetch"
import type { paths } from "./bindings/schema"

/** Where the app is served from, without a trailing slash: "" at the site root
 * (Tailscale dev and the public host). Set Vite `base` / SPEARV_BASE only if the
 * app is ever mounted under a path prefix again. */
export const BASE = (import.meta.env?.BASE_URL ?? "/").replace(/\/$/, "")

export const client = createClient<paths>({ baseUrl: BASE })
