// /embed route compatibility: a cross-origin iframe gets a frameable, minimal
// image page for render and delta views, and the direct route forwards the
// typed view exactly. The app has no embed-copy toolbar, so this script
// never looks for it.
//
//   SPEARV_WEB_ORIGIN=http://127.0.0.1:8103 \
//   PLAYWRIGHT_PATH=/home/ubuntu/e2e-tools/node_modules/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/home/ubuntu/.cache/ms-playwright \
//   node web/e2e/embed.mjs
//
// The test serves its host page on 127.0.0.1:8104, an origin different from the
// app on 127.0.0.1:8103, so the iframe framing is real cross-origin framing.

import assert from "node:assert/strict"
import { createServer } from "node:http"
import { createRequire } from "node:module"
import { mkdir } from "node:fs/promises"
import { join } from "node:path"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const web = process.env.SPEARV_WEB_ORIGIN || "http://127.0.0.1:8103"
const hostPort = Number(process.env.SPEARV_HOST_PORT || 8104)
const host = `http://127.0.0.1:${hostPort}`
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-verify/embed"
await mkdir(shots, { recursive: true })

const renderQuery = "ra=56.75&dec=24.12&fov=0.02&mode=slider&dr=qr3&mask=on&pin=42:1"
const deltaQuery =
  "ra=58.2783&dec=26.1398&fov=0.1&mode=delta&dr=qr2&mask=off&pin=25:1:v0:%2B&pin=25:1:v1:-"
const hostHtml = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>embed host</title>
<style>
body{margin:0;padding:12px;background:#20231a;color:#eee;font:14px ui-sans-serif,system-ui,sans-serif;display:flex;gap:12px;align-items:flex-start}
figure{margin:0}iframe{border:0;width:320px;height:360px;background:#0c0d09}
figcaption{padding:6px 0}
</style></head><body>
<figure><iframe id="render" title="render embed" src="${web}/embed?${renderQuery}"></iframe><figcaption>render.png view</figcaption></figure>
<figure><iframe id="delta" title="delta embed" src="${web}/embed?${deltaQuery}"></iframe><figcaption>delta.png view</figcaption></figure>
</body></html>`

const server = createServer((request, response) => {
  response.writeHead(200, { "content-type": "text/html; charset=utf-8" })
  response.end(hostHtml)
})
await new Promise((resolve, reject) => {
  server.once("error", reject)
  server.listen(hostPort, "127.0.0.1", resolve)
})

const browser = await chromium.launch()
const context = await browser.newContext({ viewport: { width: 1000, height: 760 } })

async function waitForImage(locator, label, timeoutMs = 120000) {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const size = await locator
      .evaluate((image) => (image.complete && image.naturalWidth > 0 ? image.naturalWidth : 0))
      .catch(() => 0)
    if (size > 0) return size
    await new Promise((resolve) => setTimeout(resolve, 250))
  }
  throw new Error(`${label} did not finish loading in ${timeoutMs} ms`)
}

try {
  const page = await context.newPage()

  // 1. /embed inside a page on another origin.
  await page.goto(host, { waitUntil: "domcontentloaded" })
  const renderFrame = page.frameLocator("#render")
  const deltaFrame = page.frameLocator("#delta")
  const renderImage = renderFrame.locator("img")
  const deltaImage = deltaFrame.locator("img")
  await renderImage.waitFor()
  await deltaImage.waitFor()
  const renderWidth = await waitForImage(renderImage, "render embed")
  const deltaWidth = await waitForImage(deltaImage, "delta embed")
  const renderSrc = await renderImage.getAttribute("src")
  const deltaSrc = await deltaImage.getAttribute("src")
  assert.match(renderSrc, /^\/api\/v1\/render\.png\?/)
  assert.match(deltaSrc, /^\/api\/v1\/delta\.png\?/)
  const renderParams = new URL(renderSrc, web).searchParams
  assert.equal(renderParams.get("ra"), "56.75")
  assert.equal(renderParams.get("dec"), "24.12")
  assert.equal(renderParams.get("fov"), "0.02")
  assert.equal(renderParams.get("mode"), "slider")
  assert.equal(renderParams.get("dr"), "qr3")
  assert.equal(renderParams.get("mask"), "on")
  assert.deepEqual(renderParams.getAll("pin"), ["42:1"])
  const deltaParams = new URL(deltaSrc, web).searchParams
  assert.equal(deltaParams.get("mode"), "delta")
  assert.deepEqual(deltaParams.getAll("pin"), ["25:1:v0:+", "25:1:v1:-"])

  const deltaLinkLocator = deltaFrame.locator("figcaption a")
  await deltaLinkLocator.waitFor()
  const deltaLinkBox = await deltaLinkLocator.boundingBox()
  assert.ok(deltaLinkBox && deltaLinkBox.height > 0, "attribution link is not visible")
  const deltaLink = await deltaLinkLocator.getAttribute("href")
  assert.ok(deltaLink.startsWith("/?"), `attribution link ${deltaLink}`)
  assert.equal(new URL(deltaLink, web).searchParams.get("mode"), "delta")
  const frameUrls = page.frames().map((frame) => frame.url())
  assert.ok(
    frameUrls.some((url) => url.startsWith(`${web}/embed?`)),
    `no embed frame among ${frameUrls.join(", ")}`,
  )
  await page.screenshot({ path: join(shots, "cross-origin-iframe.png"), fullPage: true })
  console.log(
    `PASS cross-origin iframe: render ${renderWidth}px, delta ${deltaWidth}px, host ${host}, app ${web}`,
  )

  // 2. The direct route is still a self-contained image page, not the app.
  for (const [query, imagePath, mode] of [
    [renderQuery, "/api/v1/render.png", "slider"],
    [deltaQuery, "/api/v1/delta.png", "delta"],
  ]) {
    const embed = await context.newPage()
    await embed.goto(`${web}/embed?${query}`, { waitUntil: "domcontentloaded" })
    const image = embed.locator("img")
    await image.waitFor()
    const width = await waitForImage(image, `${mode} direct embed`)
    assert.ok(width > 0)
    const src = await image.getAttribute("src")
    assert.ok(src.startsWith(`${imagePath}?`), `${mode} embed used ${src}`)
    const srcParams = new URL(src, web).searchParams
    const asked = new URLSearchParams(query)
    for (const key of ["ra", "dec", "fov", "mode", "dr", "mask"]) {
      assert.equal(srcParams.get(key), asked.get(key), `${mode} embed dropped ${key}`)
    }
    assert.deepEqual(srcParams.getAll("pin"), asked.getAll("pin"), `${mode} embed dropped a pin`)
    assert.equal(await embed.getByRole("button").count(), 0, "embed must not expose the app toolbar")
    assert.equal(await embed.locator("header, .topbar, .embed-actions, canvas").count(), 0)
    await embed.screenshot({ path: join(shots, `direct-${mode}.png`), fullPage: true })
    console.log(`PASS direct ${mode} embed: ${width}px, no app toolbar`)
    await embed.close()
  }

  // 3. Invalid view parameters still fail closed with a generic 400.
  const invalid = await context.request.get(`${web}/embed?ra=56.75&dec=24.12&fov=11&mode=slider`)
  assert.equal(invalid.status(), 400)
  assert.equal((await invalid.text()).trim(), "invalid view parameters")
  console.log("PASS invalid embed query is a generic 400")
} finally {
  await context.close()
  await browser.close()
  await new Promise((resolve) => server.close(resolve))
}
