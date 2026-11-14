import { hostname } from "node:os"
import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"

// The web app is its own process. It reaches the engine only over HTTP, so either
// can restart or rebuild without touching the other.
const engine = process.env.SPEARV_ENGINE ?? "http://127.0.0.1:8787"

// Hosts the dev server answers to: a comma-separated SPEARV_WEB_ALLOWED_HOSTS, or
// localhost, this machine's hostname and tailnet names.
const allowedHosts = process.env.SPEARV_WEB_ALLOWED_HOSTS
  ? process.env.SPEARV_WEB_ALLOWED_HOSTS.split(",").map((host) => host.trim()).filter(Boolean)
  : ["localhost", hostname(), ".ts.net"]

export default defineConfig({
  plugins: [react()],
  server: {
    host: process.env.SPEARV_WEB_HOST ?? "127.0.0.1",
    port: Number(process.env.SPEARV_WEB_PORT ?? 8080),
    strictPort: true,
    allowedHosts,
    proxy: {
      "/api": engine,
      "/embed": engine,
    },
  },
})
