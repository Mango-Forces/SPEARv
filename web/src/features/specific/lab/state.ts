export type Region = { ra: number; dec: number; fovL: number; fovH: number }

/** Engine region payload. `fov` keeps older readers working; the axes are
 * authoritative whenever the server supports them. */
export function apiRegion(region: Region) {
  return {
    ra: region.ra,
    dec: region.dec,
    fov: Math.max(region.fovL, region.fovH),
    fov_l: region.fovL,
    fov_h: region.fovH,
  }
}

/** IDs and refs come from exact file/content/band discovery, never table offsets or dataset IDs. */
export type Snippet = {
  id: string
  ref: string
  band: number
  mjd: number
  utc: string
  release: string
  detector: number
  obsid: string
  version: string
  geometricSamples: number
  wl_min_um: number
  wl_max_um: number
  /** Discovery-time hint that the detector footprint reaches the field centre. */
  targetCovered: boolean
  /** Raw header quality; null or empty means unknown, never clean. */
  quality: unknown
}

export type Sector = { id: string; name: string; revision: number }

export type SectorState = {
  region: Region
  sectors: Map<string, Sector>
  snippets: Map<string, Snippet>
  ownerBySnippet: Map<string, string>
  order: string[]
  activeSectorId: string | null
  nextSector: number
}

export type SectorAction =
  | { type: "region"; region: Region }
  | { type: "register"; snippets: Snippet[] }
  | { type: "create" }
  | { type: "select"; sectorId: string }
  | { type: "join"; snippetId: string }
  | { type: "remove"; snippetId: string }
  | { type: "delete"; sectorId: string }
  | { type: "reorder"; sectorId: string; direction: -1 | 1 }
  | { type: "chronological" }
  | { type: "compose"; groups: Snippet[][] }

export function initialSectorState(region: Region): SectorState {
  return { region, sectors: new Map(), snippets: new Map(), ownerBySnippet: new Map(), order: [], activeSectorId: null, nextSector: 1 }
}

export function sectorMembers(state: SectorState, sectorId: string): Snippet[] {
  return [...state.ownerBySnippet].flatMap(([snippetId, owner]) => {
    const snippet = state.snippets.get(snippetId)
    return owner === sectorId && snippet ? [snippet] : []
  })
}

export function sectorReducer(state: SectorState, action: SectorAction): SectorState {
  if (action.type === "region") {
    if (state.region.ra === action.region.ra && state.region.dec === action.region.dec && state.region.fovL === action.region.fovL && state.region.fovH === action.region.fovH) return state
    return initialSectorState(action.region)
  }
  if (action.type === "register") {
    // Keep owned recipes plus the visible page, not every row ever browsed.
    const snippets = new Map([...state.snippets].filter(([id]) => state.ownerBySnippet.has(id)))
    action.snippets.forEach((snippet) => {
      // Assigned recipes retain their original content and metadata across browse snapshots.
      if (!state.ownerBySnippet.has(snippet.id)) snippets.set(snippet.id, snippet)
    })
    return { ...state, snippets }
  }
  if (action.type === "create") {
    const id = `sector-${state.nextSector}`
    return {
      ...state,
      sectors: new Map(state.sectors).set(id, { id, name: `Sector ${state.nextSector}`, revision: 0 }),
      order: [...state.order, id], activeSectorId: id, nextSector: state.nextSector + 1,
    }
  }
  if (action.type === "select") {
    return state.sectors.has(action.sectorId) ? { ...state, activeSectorId: action.sectorId } : state
  }
  if (action.type === "join" || action.type === "remove") {
    const owner = state.ownerBySnippet.get(action.snippetId)
    const sectorId = action.type === "join" ? state.activeSectorId : owner
    if (!sectorId || !state.sectors.has(sectorId) || !state.snippets.has(action.snippetId)) return state
    if (action.type === "join" && owner) return state
    const sector = state.sectors.get(sectorId)!
    const ownerBySnippet = new Map(state.ownerBySnippet)
    if (action.type === "join") ownerBySnippet.set(action.snippetId, sectorId)
    if (action.type === "remove") ownerBySnippet.delete(action.snippetId)
    return { ...state, ownerBySnippet, sectors: new Map(state.sectors).set(sectorId, { ...sector, revision: sector.revision + 1 }) }
  }
  if (action.type === "delete") {
    if (!state.sectors.has(action.sectorId)) return state
    const sectors = new Map(state.sectors)
    sectors.delete(action.sectorId)
    const order = state.order.filter((id) => id !== action.sectorId)
    return {
      ...state, sectors, order,
      ownerBySnippet: new Map([...state.ownerBySnippet].filter(([, owner]) => owner !== action.sectorId)),
      activeSectorId: state.activeSectorId === action.sectorId ? order[0] ?? null : state.activeSectorId,
    }
  }
  if (action.type === "compose") {
    const sectors = new Map<string, Sector>()
    const snippets = new Map<string, Snippet>()
    const ownerBySnippet = new Map<string, string>()
    const order: string[] = []
    action.groups.forEach((group) => {
      if (group.length === 0) return
      const id = `sector-${sectors.size + 1}`
      sectors.set(id, { id, name: `Sector ${sectors.size + 1}`, revision: group.length })
      for (const snippet of group) {
        snippets.set(snippet.id, snippet)
        ownerBySnippet.set(snippet.id, id)
      }
      order.push(id)
    })
    return {
      ...state,
      sectors,
      snippets,
      ownerBySnippet,
      order,
      activeSectorId: order[0] ?? null,
      nextSector: order.length + 1,
    }
  }
  if (action.type === "reorder") {
    const index = state.order.indexOf(action.sectorId)
    const destination = index + action.direction
    if (index < 0 || destination < 0 || destination >= state.order.length) return state
    const order = [...state.order]
    ;[order[index], order[destination]] = [order[destination], order[index]]
    return { ...state, order }
  }
  const times = new Map(state.order.map((id) => [id, sectorMembers(state, id).reduce((earliest, snippet) => Math.min(earliest, snippet.mjd), Infinity)]))
  return { ...state, order: [...state.order].sort((a, b) => {
    const first = times.get(a)!
    const second = times.get(b)!
    return first === second ? 0 : first < second ? -1 : 1
  }) }
}
