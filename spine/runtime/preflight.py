"""Check the private runtime without reading FITS, contacting services or writing."""

from __future__ import annotations

import importlib.metadata
import importlib.resources
import json
from pathlib import Path
import platform
import sys
import sysconfig


def main() -> None:
    """Reject missing or incompatible dependencies rather than return no-data."""
    manifest_path = Path(sys.argv[1]) if len(sys.argv) == 2 else Path("/app/manifest.json")
    manifest = json.loads(manifest_path.read_text())
    prefix = Path(manifest["prefix"])
    if Path(sys.prefix) != prefix or sys.version_info[:3] != (3, 14, 4):
        raise RuntimeError("private interpreter version or prefix differs")
    if sysconfig.get_config_var("Py_GIL_DISABLED"):
        raise RuntimeError("free-threaded interpreter is not qualified")
    versions = {
        item["name"]: importlib.metadata.version(item["name"])
        for item in manifest["packages"]
    }
    if versions != {item["name"]: item["version"] for item in manifest["packages"]}:
        raise RuntimeError("private dependency versions differ")
    import numpy as np
    from astropy.utils import iers
    from astropy.wcs import Sip, WCS

    iers.conf.auto_download = False
    if not Path(np.__file__).is_relative_to(prefix):
        raise RuntimeError("NumPy imported outside the private prefix")
    if np.array([1, 2, 3], dtype=np.float64).sum() != 6:
        raise RuntimeError("NumPy native operation failed")
    data = importlib.resources.files("astropy_iers_data").joinpath("data/finals2000A.all")
    with data.open("rb") as stream:
        if not stream.read(64):
            raise RuntimeError("packaged IERS data is empty")
    wcs = WCS(naxis=2)
    wcs.wcs.crpix = [8, 8]
    wcs.wcs.crval = [56.75, 24.12]
    wcs.wcs.cd = [[-0.0017, 0], [0, 0.0017]]
    wcs.wcs.ctype = ["RA---TAN-SIP", "DEC--TAN-SIP"]
    a = np.zeros((3, 3), dtype=np.float64)
    b = a.copy()
    a[2, 0] = 5e-5
    b[0, 2] = -3e-5
    wcs.sip = Sip(a, b, None, None, wcs.wcs.crpix)
    pixels = np.asarray([[8.125, 9.75], [12.5, 3.5]], dtype=np.float64)
    sky = wcs.all_pix2world(pixels, 1)
    recovered = wcs.all_world2pix(
        sky, 1, tolerance=1e-9, maxiter=100,
        adaptive=False, detect_divergence=True, quiet=False,
    )
    error = float(np.max(np.abs(recovered - pixels)))
    if not np.isfinite(error) or error >= 1e-6:
        raise RuntimeError("private WCS roundtrip failed")
    print(json.dumps({
        "python": platform.python_version(),
        "prefix": str(prefix),
        "machine": platform.machine(),
        "libc": platform.libc_ver(),
        "packages": versions,
        "wcs_roundtrip_max_pixels": error,
        "user_site_enabled": False,
        "network_data_downloads": False,
    }, sort_keys=True))


if __name__ == "__main__":
    main()
