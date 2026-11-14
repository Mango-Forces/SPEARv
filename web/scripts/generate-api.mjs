import { execFileSync } from "node:child_process"
import { mkdirSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const spec = resolve(web, "openapi.json")
execFileSync("cargo", ["run", "-q", "-p", "spearv-binder", "--bin", "spearv", "--", "--openapi", spec], {
  cwd: resolve(web, "../spine"),
  stdio: "inherit",
})
mkdirSync(resolve(web, "src/bindings"), { recursive: true })
execFileSync("npx", ["openapi-typescript", spec, "-o", "src/bindings/schema.ts"], {
  cwd: web,
  stdio: "inherit",
})
