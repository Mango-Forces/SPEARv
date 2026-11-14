import createClient from "openapi-fetch"
import type { paths } from "./bindings/schema"

export const client = createClient<paths>({ baseUrl: "" })
