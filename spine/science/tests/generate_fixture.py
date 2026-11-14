"""Independent Astropy Header oracle for retained, already-decoded QR2/QR3 metadata.

This is test-only Header construction, not a second product FITS decoder.
Only a 6 by 3 coordinate tile is evaluated; no native detector planes load.
"""
import hashlib
import json
import pathlib

import numpy as np
from astropy.io.fits import Header
from astropy.wcs import WCS

source = pathlib.Path("/tmp/opencode/spearv-ab-20261113/geometry-stack/data/exposures.json")
document = json.loads(source.read_text())
cases = []
for release in ("qr2", "qr3"):
    exposure = next(item for item in document["exposures"] if item["release"] == release)
    spec = exposure["wcs"]
    header = Header()
    for axis in (1, 2):
        header[f"CTYPE{axis}"] = "RA---TAN-SIP" if axis == 1 else "DEC--TAN-SIP"
        header[f"CUNIT{axis}"] = "deg"
        header[f"CRPIX{axis}"] = spec["crpix"][axis - 1]
        header[f"CRVAL{axis}"] = spec["crval"][axis - 1]
        for other in (1, 2):
            header[f"CD{axis}_{other}"] = spec["cd"][axis - 1][other - 1]
    for name in ("a", "b", "ap", "bp"):
        poly = spec["sip"][name]
        if poly is None:
            continue
        n = poly["order"] + 1
        header[name.upper() + "_ORDER"] = n - 1
        for i in range(n):
            for j in range(n - i):
                header[f"{name.upper()}_{i}_{j}"] = poly["coeff"][i * n + j]
    header["LONPOLE"] = 180.0
    header["RADESYS"] = "ICRS"
    native = WCS(header)
    out = WCS(naxis=2)
    out.wcs.crpix = [3.5, 2.]
    out.wcs.crval = spec["crval"]
    out.wcs.cdelt = [-6.15 / 3600, 6.15 / 3600]
    out.wcs.ctype = ["RA---TAN", "DEC--TAN"]
    x, y = np.meshgrid(np.arange(6), np.arange(3))
    # Independent high-level zero-origin API; no helper from the module under test.
    world = out.pixel_to_world(x, y)
    px, py = native.world_to_pixel(world)
    if not (np.all(np.isfinite(px)) and np.all(np.isfinite(py))):
        raise RuntimeError("oracle non-finite")
    col, row = np.floor(px + 1.5).astype(np.int64) - 1, np.floor(py + 1.5).astype(np.int64) - 1
    cases.append(dict(release=release, key=exposure["key"], wcs=spec,
                      expected=(row * 2040 + col).reshape(-1).tolist()))
print(json.dumps(dict(source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(), cases=cases), indent=2))
