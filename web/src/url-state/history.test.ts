// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"

const calls: Array<[string, string, string]> = []
globalThis.window = {
  location: { pathname: "/", search: "?ra=56.75&dec=24.12&fov_l=0.1&fov_h=0.1&mode=color" },
  history: {
    replaceState: (_state, _title, next) => calls.push(["replace", next, ""]),
    pushState: (_state, _title, next) => calls.push(["push", next, ""]),
  },
  addEventListener: () => {},
  removeEventListener: () => {},
  dispatchEvent: () => {},
}
globalThis.PopStateEvent = class { constructor(type) { this.type = type } }

const { commit, readView } = await import("./view.ts")
const view = readView(window.location.search)

describe("committed navigation", () => {
  it("pushes View and mode changes onto history", () => {
    calls.length = 0
    commit({ ...view, ra: 133.795129 })
    commit({ ...view, mode: "specific" })
    assert.deepEqual(calls.map(([kind]) => kind), ["push", "push"])
    assert.match(calls[0][1], /ra=133\.795129/)
    assert.match(calls[1][1], /mode=specific/)
  })

  it("replaces continuous tweaks", () => {
    calls.length = 0
    commit({ ...view, mode: "composite" }, "replace")
    assert.deepEqual(calls.map(([kind]) => kind), ["replace"])
  })

  it("ignores a commit that would not change the URL", () => {
    calls.length = 0
    commit({ ...view, mode: "composite" })
    assert.deepEqual(calls, [])
  })
})
