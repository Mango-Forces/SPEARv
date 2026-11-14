// Deterministic UI-only checks; no live engine or science work.
// SPEARV_BASE_URL=http://127.0.0.1:8193 PLAYWRIGHT_PATH=... node e2e/specific-polish.mjs
import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir } from "node:fs/promises"
import { fileURLToPath } from "node:url"
import { join } from "node:path"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")
const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8080"
const shots = process.env.SPEARV_SCREENSHOT_DIR || fileURLToPath(new URL("../dist/specific-polish-smoke", import.meta.url))
await mkdir(shots, { recursive: true })
const rows = Array.from({ length: 53 }, (_, index) => ({
  id: `polish-${index}`, snippet_ref: `polish-ref-${index}`, band: index % 102 + 1,
  mjd: 61356 + index / 24, utc: `2026-11-12T${String(index % 24).padStart(2, "0")}:30:12.345Z`,
  release: index % 2 ? "qr2" : "qr3", detector: index % 6 + 1,
  obsid: `observation-with-a-long-identifier-${index}`, version: "2026.11.12",
  wl_min_um: 0.75, wl_max_um: 0.8, quality: { source: "raw quality with a long value" },
}))
const browser = await chromium.launch()
try {
  const page = await browser.newPage()
  const errors = []
  const queries = []
  page.on("pageerror", (error) => errors.push(error.message))
  await page.route("**/api/**", async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const json = (body, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) })
    if (url.pathname === "/api/v1/releases") return json({ default_release: "qr3", releases: [{ id: "qr2" }, { id: "qr3" }] })
    if (url.pathname === "/api/v1/coverage") return json({ release: "qr3", exposures: 0, bands: [], visits: [] })
    if (url.pathname === "/api/v1/observations/query") {
      queries.push(request.postDataJSON())
      return json({ dataset_id: "polish", state: "done", revision: 1 })
    }
    if (url.pathname === "/api/v1/observations/polish" && request.method() === "GET") {
      const offset = Number(url.searchParams.get("offset") || 0)
      return json({ rows: rows.slice(offset, offset + 50), matches_so_far: rows.length, pending_files: 0, failed_files: 0, revision: 1, state: "done" })
    }
    if (request.method() === "DELETE" || url.pathname === "/api/v1/snippets/retain") return route.fulfill({ status: 204 })
    // Color tongue checks do not need image rendering; fail closed for all other routes.
    return json({ error: "No rendering in this UI-only fixture" }, 404)
  })

  async function tabStyle(tab) {
    return tab.evaluate((element) => {
      const tongue = element.querySelector(".edge-sidebar__tongue")
      const button = getComputedStyle(element)
      const chrome = getComputedStyle(tongue)
      const chevron = getComputedStyle(element.querySelector(".edge-sidebar__chevron"))
      return {
        hitWidth: element.offsetWidth, hitHeight: element.offsetHeight,
        width: tongue.offsetWidth, height: tongue.offsetHeight,
        padding: button.padding, border: button.borderWidth, background: button.backgroundColor,
        color: button.color, outline: button.outlineStyle, display: button.display,
        chromeBackground: chrome.backgroundColor, chromeBorder: chrome.borderWidth,
        left: chrome.borderLeftColor, right: chrome.borderRightColor,
        top: chrome.borderTopColor, bottom: chrome.borderBottomColor,
        radius: chrome.borderRadius, chromeOutline: chrome.outlineStyle,
        chromeOutlineColor: chrome.outlineColor, chevron: chevron.transform,
      }
    })
  }
  async function tongueStates(mode, hover) {
    await page.getByRole("button", { name: mode, exact: true }).click()
    const tab = page.getByRole("button", { name: mode === "Specific" ? /^(Expand|Collapse) Sector editor$/ : /^(Expand|Collapse) (Color|Composite) controls$/ })
    await tab.waitFor({ state: "visible" })
    if (await tab.getAttribute("aria-expanded") === "false") await tab.evaluate((element) => element.click())
    await page.mouse.move(1, 1)
    await tab.evaluate((element) => element.blur())
    const open = await tabStyle(tab)
    assert.equal(open.width, 30, `${mode}: tongue width`)
    assert.equal(open.height, 72, `${mode}: tongue height`)
    assert.equal(open.hitWidth, 44, `${mode}: hit width`)
    assert.equal(open.hitHeight, 84, `${mode}: hit height`)
    assert.equal(open.padding, "0px", `${mode}: tongue hit target inherits control padding`)
    assert.equal(open.border, "0px", `${mode}: extra border box`)
    assert.equal(open.background, "rgba(0, 0, 0, 0)", `${mode}: extra background box`)
    assert.equal(open.right, "rgb(198, 255, 46)", `${mode}: outer-edge highlight`)
    assert.equal(open.left, "rgba(0, 0, 0, 0)", `${mode}: attachment edge must stay open`)
    assert.equal(open.color, "rgb(198, 255, 46)", `${mode}: active chevron`)
    if (hover) await tab.hover()
    const openHover = hover ? await tabStyle(tab) : open
    await page.keyboard.press("Tab")
    await tab.focus()
    const openFocus = await tabStyle(tab)
    assert.equal(openFocus.outline, "none", `${mode}: focus box on hit target`)
    assert.equal(openFocus.chromeOutline, "solid", `${mode}: missing tongue focus ring`)
    await tab.evaluate((element) => element.click())
    await page.waitForFunction((id) => document.getElementById(id).inert, await tab.getAttribute("aria-controls"))
    await page.mouse.move(1, 1)
    await tab.evaluate((element) => element.blur())
    const closed = await tabStyle(tab)
    if (hover) await tab.hover()
    const closedHover = hover ? await tabStyle(tab) : closed
    await page.keyboard.press("Tab")
    await tab.focus()
    const closedFocus = await tabStyle(tab)
    await tab.press("Enter")
    await page.waitForFunction((id) => !document.getElementById(id).inert, await tab.getAttribute("aria-controls"))
    return { open, openHover, openFocus, closed, closedHover, closedFocus }
  }

  for (const [width, height] of [[1440, 960], [1000, 760], [768, 800], [390, 844], [320, 720]]) {
    await page.setViewportSize({ width, height })
    await page.goto(`${base}/?ra=56.75&dec=24.12&fov=0.1&mode=color`)
    // Remove animation timing from geometry assertions, not application behavior.
    await page.emulateMedia({ reducedMotion: "reduce" })
    await page.addStyleTag({ content: "* { transition: none !important; animation: none !important; }" })
    // At 320 the right-edge drawer hit target overlaps the open left tongue; that
    // shell collision predates this change, so only geometry/focus is asserted there.
    const color = await tongueStates("Color", width >= 390)
    const composite = await tongueStates("Composite", width >= 390)
    const specific = await tongueStates("Specific", width >= 390)
    assert.deepEqual(composite, color, `${width}px: Composite tongue chrome differs from Color`)
    assert.deepEqual(specific, color, `${width}px: Specific tongue chrome differs from Color`)
    if (width === 1440 || width === 390) await page.screenshot({ path: join(shots, `specific-initial-${width}.png`), fullPage: true })
    await page.locator(".snippet-scroll tbody tr").first().waitFor()
    const filters = page.locator(".specific-section").filter({ has: page.locator("summary", { hasText: /^Snippet filters$/ }) })
    assert.equal(await filters.locator("summary").innerText(), "Snippet filters")
    await filters.locator("summary").click()
    await filters.getByText("Set your lower and upper time bound.", { exact: true }).waitFor()
    for (const label of ["Start UTC", "Finish UTC"]) {
      const inline = await page.getByLabel(label, { exact: true }).evaluate((input) => {
        const word = input.closest("label").querySelector("span").getBoundingClientRect()
        const field = input.getBoundingClientRect()
        return { beside: field.left > word.right, aligned: Math.abs((word.top + word.bottom) / 2 - (field.top + field.bottom) / 2) < 3 }
      })
      assert.deepEqual(inline, { beside: true, aligned: true }, `${width}px: ${label} is not inline`)
    }
    assert.equal(await page.getByText(/All bands · all dates|UTC, e.g\./).count(), 0)
    if (width === 1440 || width === 390) await page.screenshot({ path: join(shots, `specific-filters-${width}.png`), fullPage: true })
    const metrics = await page.locator(".snippet-scroll").evaluate((scroller) => {
      const table = scroller.querySelector("table")
      return { width: scroller.clientWidth, scrollWidth: scroller.scrollWidth, height: scroller.clientHeight, scrollHeight: scroller.scrollHeight, tableWidth: table.getBoundingClientRect().width, overflow: getComputedStyle(scroller).overflow }
    })
    assert.equal(metrics.overflow, "auto", `${width}px: table does not scroll in both directions`)
    assert.ok(metrics.tableWidth >= 320, `${width}px: columns crushed ${JSON.stringify(metrics)}`)
    if (width <= 320) assert.ok(metrics.scrollWidth > metrics.width, `${width}px: fixture must exercise horizontal overflow`)
    assert.ok(metrics.scrollHeight > metrics.height, `${width}px: fixture must exercise vertical overflow`)
    const time = await page.locator(".snippet-scroll tbody tr").first().locator("time").evaluate((element) => ({ content: element.scrollWidth, cell: element.parentElement.clientWidth }))
    assert.ok(time.content <= time.cell, `${width}px: full UTC timestamp is clipped`)
    await page.getByRole("button", { name: "New sector", exact: true }).click()
    await page.locator(".snippet-scroll tbody tr").first().getByRole("button", { name: /^Join band/ }).click()
    await page.locator(".sector-members li").first().waitFor()
    await page.getByRole("button", { name: "Next page", exact: true }).click()
    await page.locator(".snippet-scroll tbody tr").first().getByRole("button", { name: /Join band 51 / }).waitFor()
    const lastInfo = page.locator(".snippet-info-toggle").last()
    await lastInfo.click()
    const detail = page.locator(".snippet-info-row dl").last()
    await detail.scrollIntoViewIfNeeded()
    const readable = await detail.evaluate((element, viewportWidth) => {
      const scroll = element.closest(".snippet-scroll")
      const a = element.getBoundingClientRect()
      const b = scroll.getBoundingClientRect()
      return {
        inFlow: getComputedStyle(element).position === "static",
        visible: a.top >= b.top - 1 && a.bottom <= b.bottom + 1 && (viewportWidth >= 390 ? a.left >= b.left - 3 && a.right <= b.right + 3 : scroll.scrollWidth > scroll.clientWidth),
        width: a.width, scroller: b.width,
        box: { left: a.left, right: a.right, top: a.top, bottom: a.bottom },
        view: { left: b.left, right: b.right, top: b.top, bottom: b.bottom, scrollWidth: scroll.scrollWidth, clientWidth: scroll.clientWidth },
      }
    }, width)
    assert.equal(readable.inFlow, true, `${width}px: Info still overlays`)
    assert.equal(readable.visible, true, `${width}px: bottom-row Info cannot be read ${JSON.stringify(readable)}`)
    assert.ok(readable.width > 200, `${width}px: Info is crushed`)
    if (width === 1440 || width === 390) await page.screenshot({ path: join(shots, `specific-info-${width}.png`), fullPage: true })
    const overflow = await page.evaluate(() => ({ viewport: document.documentElement.clientWidth, document: document.documentElement.scrollWidth, body: document.body.scrollWidth }))
    assert.ok(overflow.document <= overflow.viewport + 1 && overflow.body <= overflow.viewport + 1, `${width}px: page overflow ${JSON.stringify(overflow)}`)
    await page.locator(".specific-editor-content").evaluate((element) => { element.scrollTop = 0 })
    await page.locator(".snippet-scroll").evaluate((element) => { element.scrollTop = 0; element.scrollLeft = 0 })
    await page.screenshot({ path: join(shots, `specific-${width}.png`), fullPage: true })
    console.log(`PASS ${width}px: identical tongues; inline bounds; readable, scrollable table and bottom-row Info`)
  }

  await page.setViewportSize({ width: 1440, height: 960 })
  const nextQuery = page.waitForRequest((request) => new URL(request.url()).pathname === "/api/v1/observations/query" && request.method() === "POST")
  await page.getByLabel("Start UTC", { exact: true }).fill("12 Nov 2026, 14:30:12.345 UTC")
  await page.getByLabel("Finish UTC", { exact: true }).fill("13 Nov 2026, 14:30 UTC")
  await page.getByRole("button", { name: "Apply to snippet list", exact: true }).click()
  const applied = (await nextQuery).postDataJSON()
  assert.equal(applied.from_utc, "2026-11-12T14:30:12.345Z")
  assert.equal(applied.to_utc, "2026-11-13T14:30:00.000Z")
  assert.equal(new URL(page.url()).searchParams.get("from_utc"), applied.from_utc)
  await page.getByRole("button", { name: "Composite", exact: true }).click()
  await page.getByRole("button", { name: "Specific", exact: true }).click()
  await page.locator(".sector-members li").first().waitFor()
  await page.locator('input[name="ra"]').fill("57")
  const regionQuery = page.waitForRequest((request) => new URL(request.url()).pathname === "/api/v1/observations/query" && request.method() === "POST")
  await page.getByRole("button", { name: "View", exact: true }).click()
  assert.equal((await regionQuery).postDataJSON().region.ra, 57)
  await page.waitForFunction(() => !document.querySelector(".sector-node"))
  assert.equal(await page.getByText("Region changed. Snippets, sectors and timeline were reset.", { exact: true }).count(), 0)
  assert.deepEqual(errors, [], "browser runtime errors")
  assert.ok(queries.length > 0)
  console.log("PASS UTC milliseconds/API/URL unchanged; sectors survive mode changes; region reset remains without notice; no runtime errors")
} finally {
  await browser.close()
}
