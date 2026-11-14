// Focus/blur regression for the shared ToneControls, mounted through Vite.
//
// The fixture imports the production CompositeDisplayControls/ToneControls
// module and the production display URL helpers. It never boots the app, so no
// mosaic, engine, or API request is made.
//
// Run from the repository root or web directory, for example:
//   PLAYWRIGHT_PATH=/path/to/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/path/to/ms-playwright \
//   node web/e2e/color-tone-controls.mjs
//
// PLAYWRIGHT_PATH is optional when "playwright" resolves normally. The browser
// cache is provided by PLAYWRIGHT_BROWSERS_PATH. Vite is started as this
// process's child on 127.0.0.1:8244 and stopped in the finally block.

import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import { fileURLToPath } from "node:url"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const webRoot = fileURLToPath(new URL("..", import.meta.url))
const viteBin = fileURLToPath(new URL("../node_modules/vite/bin/vite.js", import.meta.url))
const host = "127.0.0.1"
const port = 8244
const fixtureUrl = `http://${host}:${port}/e2e/color-tone-controls.fixture.html`
const autoLabel = "Composite black point, MJy/sr"
const whiteLabel = "Composite white point, MJy/sr"
const autoButtonName = "Auto stretch"
const redBlackLabel = "Red black point, MJy/sr"
const redWhiteLabel = "Red white point, MJy/sr"

let viteLog = ""
const vite = spawn(process.execPath, [viteBin, "--host", host, "--port", String(port), "--strictPort"], {
  cwd: webRoot,
  env: { ...process.env, BROWSER: "none" },
  stdio: ["ignore", "pipe", "pipe"],
})
vite.stdout.setEncoding("utf8")
vite.stderr.setEncoding("utf8")
vite.stdout.on("data", (chunk) => (viteLog += chunk))
vite.stderr.on("data", (chunk) => (viteLog += chunk))
const viteExited = new Promise((resolve) => vite.once("exit", resolve))

async function waitForVite() {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    if (vite.exitCode !== null) throw new Error(`Vite exited with ${vite.exitCode}\n${viteLog}`)
    try {
      const response = await fetch(fixtureUrl)
      if (response.ok) return
    } catch {
      // Vite is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
  throw new Error(`Vite did not serve ${fixtureUrl}\n${viteLog}`)
}

async function stopVite() {
  if (vite.exitCode !== null) return
  vite.kill("SIGTERM")
  await Promise.race([viteExited, new Promise((resolve) => setTimeout(resolve, 5_000))])
  if (vite.exitCode === null) vite.kill("SIGKILL")
}

async function main() {
  await waitForVite()
  const browser = await chromium.launch()
  const context = await browser.newContext({ viewport: { width: 1200, height: 900 } })
  const page = await context.newPage()
  const pageErrors = []
  const apiRequests = []
  page.on("pageerror", (error) => pageErrors.push(error.message))
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/api/")) apiRequests.push(request.url())
  })

  const black = () => page.getByLabel(autoLabel)
  const white = () => page.getByLabel(whiteLabel)
  const compositeTone = () => page.locator('[data-testid="composite-tone"]')
  const colorTone = () => page.locator('[data-testid="color-tone"]')
  const autoButton = () => compositeTone().getByRole("button", { name: autoButtonName })
  const redBlack = () => page.getByLabel(redBlackLabel)
  const redWhite = () => page.getByLabel(redWhiteLabel)
  const compositeAlert = () => compositeTone().getByRole("alert")
  const colorAlert = () => colorTone().getByRole("alert")
  const text = async (id) => (await page.getByTestId(id).textContent()) ?? ""
  const waitText = (id, expected) =>
    page.waitForFunction(
      ({ id, expected }) => document.querySelector(`[data-testid="${id}"]`)?.textContent === expected,
      { id, expected },
    )
  const waitValue = (label, value) => page.waitForFunction(
    ({ label, value }) => document.querySelector(`input[aria-label="${label}"]`)?.value === value,
    { label, value },
  )
  const blur = (locator) => locator.evaluate((element) => element.blur())
  const reload = async () => {
    await page.goto(fixtureUrl)
    await waitText("mode", "auto")
    await waitText("commits", "0")
    await waitText("mode-color", "auto")
    await waitText("commits-color", "0")
    assert.equal(await text("bw"), "")
    assert.equal(await text("bw-color"), "")
  }

  const failures = []
  const scenario = async (name, run) => {
    try {
      await run()
      console.log(`PASS ${name}`)
    } catch (error) {
      failures.push(error)
      console.error(`FAIL ${name}\n${error.stack || error}`)
    }
  }

  try {
    await scenario("untouched Auto focus/blur stays Auto with no parent update", async () => {
      await reload()
      assert.equal(await black().inputValue(), "100")
      assert.equal(await white().inputValue(), "1.000e+3")
      await black().focus()
      await blur(black())
      await waitText("mode", "auto")
      assert.equal(await text("commits"), "0")
      assert.equal(await text("bw"), "")
      assert.equal(await black().inputValue(), "100")
      assert.equal(await white().inputValue(), "1.000e+3")
    })

    await scenario("one-point edit and blur commits Manual; unchanged Manual blur is a no-op", async () => {
      await reload()
      await black().fill("200")
      await blur(black())
      await waitText("mode", "manual")
      await waitText("commits", "1")
      await waitText("bw", "200,1000")
      assert.equal(await black().inputValue(), "200")
      assert.equal(await white().inputValue(), "1.000e+3")
      await black().focus()
      await blur(black())
      await waitText("mode", "manual")
      assert.equal(await text("commits"), "1")
      assert.equal(await text("bw"), "200,1000")
    })

    await scenario("invalid point does not commit; corrected blur commits Manual", async () => {
      await reload()
      await white().fill("50")
      await blur(white())
      await compositeAlert().waitFor()
      assert.match(await compositeAlert().textContent(), /finite points/)
      assert.equal(await text("mode"), "auto")
      assert.equal(await text("commits"), "0")
      assert.equal(await text("bw"), "")
      await white().fill("900")
      await blur(white())
      await waitText("mode", "manual")
      await waitText("commits", "1")
      await waitText("bw", "100,900")
      assert.equal(await compositeAlert().count(), 0)
      assert.equal(await white().inputValue(), "900")
    })

    await scenario("Auto click after a dirty edit ends Auto with no manual URL state", async () => {
      await reload()
      await black().fill("200")
      await autoButton().click()
      await waitValue(autoLabel, "100")
      await waitText("mode", "auto")
      await waitText("bw", "")
      assert.ok(Number(await text("commits")) >= 1)
      assert.equal(await white().inputValue(), "1.000e+3")
    })

    await scenario("manual settings persist across changed metadata; Auto uses the new metadata", async () => {
      await reload()
      await black().fill("200")
      await blur(black())
      await waitText("mode", "manual")
      await waitText("bw", "200,1000")
      await page.locator("#new-metadata").click()
      await waitText("auto-points", "300:400")
      assert.equal(await text("mode"), "manual")
      assert.equal(await text("commits"), "1")
      assert.equal(await text("bw"), "200,1000")
      assert.equal(await black().inputValue(), "200")
      assert.equal(await white().inputValue(), "1.000e+3")
      assert.equal(await text("resolved"), "200:800")
      await autoButton().click()
      await waitText("mode", "auto")
      await waitText("bw", "")
      await waitText("resolved", "300:400")
      await waitValue(autoLabel, "300")
      await waitValue(whiteLabel, "700")
      assert.equal(await black().inputValue(), "300")
      assert.equal(await white().inputValue(), "700")
    })

    await scenario("new metadata discards an uncommitted dirty draft", async () => {
      await reload()
      await black().fill("150")
      await page.evaluate(() => window.__applyNewMetadata())
      await waitText("auto-points", "300:400")
      await waitValue(autoLabel, "300")
      await blur(black())
      await waitText("mode", "auto")
      assert.equal(await text("commits"), "0")
      assert.equal(await text("bw"), "")
      assert.equal(await black().inputValue(), "300")
      assert.equal(await white().inputValue(), "700")
    })

    await scenario("an oversized Color range stays a draft with a field error and no commit", async () => {
      await reload()
      await redBlack().fill("1e100")
      await redWhite().fill("1.1e100")
      await blur(redWhite())
      await colorAlert().waitFor()
      assert.match(await colorAlert().textContent(), /32-bit/)
      assert.equal(await text("mode-color"), "auto")
      assert.equal(await text("commits-color"), "0")
      assert.equal(await text("bw-color"), "")
      assert.equal(await redBlack().inputValue(), "1e100")
      assert.equal(await redWhite().inputValue(), "1.1e100")
    })

    await scenario("a valid Color range still commits and persists", async () => {
      await reload()
      await redBlack().fill("200")
      await redWhite().fill("1000")
      await blur(redWhite())
      await waitText("mode-color", "manual")
      await waitText("commits-color", "1")
      await waitText("bw-color", "200,1000")
      assert.equal(await colorAlert().count(), 0)
      assert.equal(await redBlack().inputValue(), "200")
      assert.equal(await redWhite().inputValue(), "1.000e+3")
    })

    await scenario("stored manual Color points show their values and error without metadata", async () => {
      await reload()
      await page.evaluate(() => window.__applyColorManual(1e100, 1.1e100))
      await waitValue(redBlackLabel, "1.000e+100")
      await waitValue(redWhiteLabel, "1.100e+100")
      await colorAlert().waitFor()
      assert.match(await colorAlert().textContent(), /32-bit/)
      assert.equal(await text("mode-color"), "manual")
      assert.equal(await text("bw-color"), `${1e100},${1.1e100}`)
      assert.equal(await text("commits-color"), "0")
    })

    assert.deepEqual(pageErrors, [], `page errors: ${pageErrors.join("; ")}`)
    assert.deepEqual(apiRequests, [], `unexpected API requests: ${apiRequests.join(", ")}`)
    if (failures.length) throw new Error(`${failures.length} tone-controls scenario(s) failed`)
  } finally {
    await context.close()
    await browser.close()
  }
}

let failed = false
try {
  await main()
} catch (error) {
  failed = true
  console.error(error.stack || error)
} finally {
  await stopVite()
}
process.exitCode = failed ? 1 : 0
