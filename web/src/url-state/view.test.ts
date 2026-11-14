// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import { readView, setWorkspace, viewSearch, type View } from "./view.ts"
import { DEFAULT_BOUNDS } from "../features/specific/link.ts"

const BASE: View = {
  ra: 56.75,
  dec: 24.12,
  fovL: 1,
  fovH: 1,
  fovLinked: true,
  mode: "composite",
  dr: "dr1",
  pins: [{ center: 42, width: 5 }],
  gaia: null,
  wise: null,
  mask: true,
  candidates: true,
  movers: false,
  color: false,
}

const BOUNDS = {
  fromUtc: "2024-01-02T03:04:05.000Z",
  toUtc: "2024-02-03T04:05:06.000Z",
  releases: ["qr2", "qr3"],
  bands: [1, 2, 3, 40, 41, 42],
}

const COLOUR = {
  colorBands: {
    r: { pin: 80, width: 7 },
    g: { pin: 30, width: 6 },
    b: { pin: 10, width: 4 },
  },
}

describe("older links stay compatible", () => {
  it("keeps delta pins, visits, signs, opacity and mode", () => {
    const view = readView("?ra=56.75&dec=24.12&fov_l=0.5&fov_h=0.5&mode=delta&dr=qr2&mask=on&pin=10%3A5%3Av0%3A%2B&pin=20%3A5%3Av1%3A-&gaia=0.7&wise=0.3")
    assert.equal(view.mode, "delta")
    assert.equal(view.color, false)
    assert.deepEqual(view.pins, [
      { center: 10, width: 5, visit: 0, sign: "+" },
      { center: 20, width: 5, visit: 1, sign: "-" },
    ])
    assert.equal(view.gaia, 0.7)
    assert.equal(view.wise, 0.3)
    assert.equal(view.bounds, undefined)
    assert.equal(view.colour, undefined)
  })

  it("uses only the rectangular pair and preserves explicit unlinking", () => {
    const single = readView("?fov=2")
    assert.equal(single.fovL, 0.1)
    assert.equal(single.fovH, 0.1)
    assert.match(single.urlErrors?.join("\n") ?? "", /fov is unsupported.*fov_l.*fov_h/)
    const lone = readView("?fov_l=2")
    assert.deepEqual({ fovL: lone.fovL, fovH: lone.fovH, fovLinked: lone.fovLinked }, { fovL: 2, fovH: 2, fovLinked: true })
    const unlinked = readView("?fov_l=2&fov_h=2&fov_linked=0")
    assert.equal(unlinked.fovLinked, false)
    const params = new URLSearchParams(viewSearch(unlinked))
    assert.equal(params.get("fov"), null)
    assert.equal(params.get("fov_l"), "2")
    assert.equal(params.get("fov_h"), "2")
    assert.equal(params.get("fov_linked"), "0")
    assert.equal(readView("?fov_l=2&fov_h=1&fov_linked=1").fovLinked, false)
  })

  it("reads mode=color and color=1 as the colour workspace", () => {
    assert.equal(readView("?mode=color").color, true)
    assert.equal(readView("?color=1").color, true)
    assert.equal(readView("?mode=slider").color, false)
  })

  it("leaves pins=none, mask, candidates and movers alone", () => {
    const view = readView("?pins=none&mask=off&candidates=off&movers=on")
    assert.deepEqual(view.pins, [])
    assert.equal(view.mask, false)
    assert.equal(view.candidates, false)
    assert.equal(view.movers, true)
  })
})

describe("bounds roundtrip", () => {
  it("serializes applied bounds and reads them back", () => {
    const view: View = { ...BASE, mode: "specific", bounds: BOUNDS }
    const search = viewSearch(view)
    const params = new URLSearchParams(search)
    assert.equal(params.get("from_utc"), BOUNDS.fromUtc)
    assert.equal(params.get("to_utc"), BOUNDS.toUtc)
    assert.equal(params.get("releases"), "qr2,qr3")
    assert.ok(params.get("bands"))
    const back = readView(`?${search}`)
    assert.deepEqual(back.bounds, {
      fromUtc: BOUNDS.fromUtc,
      toUtc: BOUNDS.toUtc,
      releases: ["qr2", "qr3"],
      bands: BOUNDS.bands,
    })
  })

  it("omits default bounds and normalizes explicit defaults", () => {
    assert.equal(viewSearch({ ...BASE, bounds: undefined }).includes("from_utc"), false)
    assert.equal(viewSearch({ ...BASE, bounds: DEFAULT_BOUNDS }).includes("bands"), false)
    assert.equal(readView("?releases=dr1%2Cqr2%2Cqr3&bands=1-102").bounds, undefined)
    assert.equal(readView("?mode=specific").bounds, undefined)
  })

  it("keeps open ends and single-release selections", () => {
    const view: View = { ...BASE, mode: "specific", bounds: { fromUtc: null, toUtc: null, releases: ["qr3"], bands: [90, 91, 92] } }
    const back = readView(`?${viewSearch(view)}`)
    assert.deepEqual(back.bounds, view.bounds)
  })
})

describe("colour roundtrip", () => {
  it("serializes custom API-numbered channels and reads them back", () => {
    const view: View = { ...BASE, color: true, colour: COLOUR }
    const search = viewSearch(view)
    const params = new URLSearchParams(search)
    assert.equal(params.get("color_r"), "80")
    assert.equal(params.get("color_r_width"), "7")
    assert.equal(params.get("color_g"), "30")
    assert.equal(params.get("color_g_width"), "6")
    assert.equal(params.get("color_b"), "10")
    assert.equal(params.get("color_b_width"), "4")
    // UI shows 1..102; the URL and mosaic request stay 0-based.
    assert.equal(params.get("mode"), "color")
    const back = readView(`?${search}`)
    assert.deepEqual(back.colour, COLOUR)
    assert.equal(back.color, true)
  })

  it("omits default colour and normalizes explicit defaults", () => {
    assert.equal(viewSearch({ ...BASE, colour: undefined }).includes("color_r"), false)
    const defaults: View = {
      ...BASE,
      colour: { colorBands: { r: { pin: 90, width: 5 }, g: { pin: 40, width: 5 }, b: { pin: 5, width: 5 } } },
    }
    assert.equal(viewSearch(defaults).includes("color_r"), false)
    assert.equal(readView("?mode=color&color_r=90&color_r_width=5&color_g=40&color_g_width=5&color_b=5&color_b_width=5").colour, undefined)
  })

  it("fills missing channels from the defaults", () => {
    const back = readView("?mode=color&color_r=80")
    assert.deepEqual(back.colour, {
      colorBands: {
        r: { pin: 80, width: 5 },
        g: { pin: 40, width: 5 },
        b: { pin: 5, width: 5 },
      },
    })
  })
})

describe("invalid links surface every bad parameter", () => {
  it("names bad bounds without substituting a queryable view", () => {
    for (const bad of [
      "bands=0",
      "bands=103",
      "bands=nope",
      "releases=rd1",
      "releases=nope",
      "releases=",
      "from_utc=not-a-date",
      "from_utc=2024-02-03T04%3A05%3A06.000Z&to_utc=2024-01-02T03%3A04%3A05.000Z",
      "from_utc=2024-13-99T99%3A99%3A99.000Z",
    ]) {
      const view = readView(`?ra=10&mode=specific&${bad}`)
      assert.equal(view.bounds, undefined, bad)
      assert.ok((view.urlErrors?.length ?? 0) > 0, bad)
      assert.equal(view.ra, 10)
      assert.equal(view.mode, "specific")
    }
  })

  it("names bad numeric link fields instead of defaulting them", () => {
    for (const [search, name] of [
      ["?ra=banana&dec=24.12", "ra"],
      ["?ra=56.75&dec=oops", "dec"],
      ["?fov_l=banana&fov_h=0.1", "fov_l"],
      ["?fov_l=0.1&fov_h=31", "fov_h"],
      ["?ra=361", "ra"],
      ["?dec=-91", "dec"],
    ] as const) {
      const view = readView(search)
      assert.ok(view.urlErrors?.some((error) => error.startsWith(`${name}:`)), `${search}: ${view.urlErrors?.join("; ")}`)
    }
  })

  it("keeps a valid link without an error", () => {
    assert.equal(readView("?ra=56.75&dec=24.12&fov_l=0.1&fov_h=0.1&mode=color").urlErrors, undefined)
  })

  it("names bad colour channels but keeps the rest of the view", () => {
    for (const bad of [
      "color_r=102",
      "color_r=-1",
      "color_r=1.5",
      "color_r=nope",
      "color_r_width=0",
      "color_r_width=103",
      "color_g_width=nope",
      "color_b=200",
    ]) {
      const view = readView(`?mode=color&${bad}`)
      assert.equal(view.colour, undefined, bad)
      assert.ok(view.urlErrors?.some((error) => error.startsWith(bad.split("=")[0] + ":")), bad)
      assert.equal(view.color, true)
    }
  })
})

describe("settings survive Show, mode and layer commits", () => {
  const stocked: View = { ...BASE, mode: "specific", color: false, gaia: 0.5, bounds: BOUNDS, colour: COLOUR }

  it("Show (position commit) keeps bounds and colour", () => {
    const next: View = { ...stocked, ra: 10, dec: 20, fovL: 2, fovH: 2, fovLinked: true }
    const back = readView(`?${viewSearch(next)}`)
    assert.deepEqual(back.bounds, stocked.bounds)
    assert.deepEqual(back.colour, stocked.colour)
    assert.equal(back.ra, 10)
  })

  it("switching workspaces keeps both settings in the URL", () => {
    for (const workspace of ["color", "composite", "specific"] as const) {
      const next = setWorkspace(stocked, workspace)
      const back = readView(`?${viewSearch(next)}`)
      assert.deepEqual(back.bounds, BOUNDS, workspace)
      assert.deepEqual(back.colour, COLOUR, workspace)
    }
  })

  it("opacity, release and pin edits keep both settings", () => {
    const opacity: View = { ...stocked, gaia: 0.2, wise: 0.9 }
    assert.deepEqual(readView(`?${viewSearch(opacity)}`).bounds, BOUNDS)
    const release: View = { ...stocked, dr: "qr2", pins: stocked.pins.map((pin, index) => ({ ...pin, visit: index })) }
    const back = readView(`?${viewSearch(release)}`)
    assert.deepEqual(back.bounds, BOUNDS)
    assert.deepEqual(back.colour, COLOUR)
    assert.equal(back.dr, "qr2")
  })
})
