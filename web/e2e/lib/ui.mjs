// Shared UI entry points for the end-to-end scripts. The shell is allowed to
// change labels and markup, so these helpers use accessible roles and the
// drawer's aria state rather than a particular class hierarchy.
import assert from "node:assert/strict"

const escape = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")

/** A visible toolbar button whose accessible name is exactly `name`, with a
 * loose match as a fallback for labels like "Layers panel". Buttons that carry
 * drawer state win over a same-named panel summary. */
export async function findPanelButton(page, name) {
  const exact = page.getByRole("button", { name: new RegExp(`^\\s*${escape(name)}\\s*$`, "i") })
  const explicit = exact.and(page.locator("[aria-expanded], [aria-controls]")).first()
  if (await explicit.count()) return explicit
  if (await exact.count()) return exact.first()
  const loose = page.getByRole("button", { name: new RegExp(`\\b${escape(name)}\\b`, "i") })
  const looseExplicit = loose.and(page.locator("[aria-expanded], [aria-controls]")).first()
  if (await looseExplicit.count()) return looseExplicit
  return loose.first()
}

export async function drawerIsOpen(page, button) {
  const expanded = await button.getAttribute("aria-expanded")
  if (expanded !== null) return expanded === "true"
  const controls = await button.getAttribute("aria-controls")
  if (controls) {
    const panel = page.locator(`[id="${controls.replaceAll('"', '\\"')}"]`)
    return (await panel.count()) > 0 && panel.first().isVisible()
  }
  return false
}

export async function waitForDrawer(page, button, open, timeout = 4000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    if ((await drawerIsOpen(page, button)) === open) return
    await page.waitForTimeout(50)
  }
  assert.equal(await drawerIsOpen(page, button), open, `drawer did not ${open ? "open" : "close"}`)
}

export async function openDrawer(page, name) {
  const button = await findPanelButton(page, name)
  await button.waitFor({ state: "visible" })
  if (await drawerIsOpen(page, button)) return button
  await button.click()
  await waitForDrawer(page, button, true)
  return button
}

export async function closeDrawer(page, name) {
  const button = await findPanelButton(page, name)
  if (!(await drawerIsOpen(page, button))) return
  await button.click()
  try {
    await waitForDrawer(page, button, false, 1000)
  } catch {
    // A drawer with a separate close control also answers Escape. Use it to
    // finish the close, then leave the assertion to the caller.
    await page.keyboard.press("Escape")
    await waitForDrawer(page, button, false)
  }
}

export async function closeAllDrawers(page) {
  for (const name of ["Share", "Layers", "Spectrum"]) {
    const button = await findPanelButton(page, name)
    if ((await button.count()) && (await drawerIsOpen(page, button))) await closeDrawer(page, name)
  }
}

export function modeButton(page, name) {
  const label = name === "Color" ? "colou?r" : escape(name)
  return page.getByRole("button", { name: new RegExp(`^\\s*${label}\\s*$`, "i") }).first()
}

export function layerRow(page, name) {
  return page
    .locator(".layer-row, [data-layer], [data-catalog-layer]")
    .filter({ has: page.getByText(new RegExp(`^\\s*${escape(name)}\\b`, "i")) })
    .first()
}

/** Toggle a catalog row in the visible Layers drawer and return its button. */
export async function toggleLayer(page, name) {
  await openDrawer(page, "Layers")
  const row = layerRow(page, name)
  await row.waitFor({ state: "visible" })
  const button = row.getByRole("button", { name: /^(show|hide|on|off)$/i }).first()
  const target = (await button.count()) ? button : row.getByRole("button").first()
  await target.click()
  return target
}

export function mainCanvas(page) {
  return page.locator(".overlay-frame canvas, canvas[data-export-image]").first()
}

export async function waitForCanvas(page, timeout = 30000) {
  await page.waitForFunction(() => {
    const canvas = document.querySelector(".overlay-frame canvas, canvas[data-export-image]")
    return Boolean(canvas && canvas.width > 0 && canvas.height > 0)
  }, undefined, { timeout })
}

export async function assertNoPageOverflow(page) {
  const widths = await page.evaluate(() => ({
    document: document.documentElement.scrollWidth,
    viewport: document.documentElement.clientWidth,
    body: document.body.scrollWidth,
  }))
  assert.ok(
    widths.document <= widths.viewport + 1 && widths.body <= widths.viewport + 1,
    `page overflows horizontally: ${JSON.stringify(widths)}`,
  )
}
