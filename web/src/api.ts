/// <reference types="vite/client" />
import createClient from "openapi-fetch"
import type { paths } from "./bindings/schema"

/** Where the app is served from, without a trailing slash: "" at the site root
 * (the Tailscale dev server), "/SPEARv" behind the public proxy. Every engine
 * URL hangs off it, so one build-time base moves the whole app. */
export const BASE = (import.meta.env?.BASE_URL ?? "/").replace(/\/$/, "")

export const client = createClient<paths>({ baseUrl: BASE })
