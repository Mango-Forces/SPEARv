"""Trusted embedded array implementation; never parses product FITS bytes."""
import importlib.metadata
import pathlib
import sys

import numpy as np
import astropy
from astropy.wcs import WCS, Sip, NoConvergence
from astropy.utils import iers
import astropy_iers_data


def preflight(package_path):
    expected = {"numpy": "2.5.4", "astropy": "8.0.2",
                "astropy-iers-data": "0.2026.11.2.0.34.55"}
    if tuple(sys.version_info[:3]) != (3, 14, 4):
        raise RuntimeError("CPython version is not the qualified 3.14.4")
    if not sys.flags.isolated or not sys.flags.no_user_site or sys.flags.ignore_environment != 1:
        raise RuntimeError("interpreter is not isolated")
    if not sys._is_gil_enabled():
        raise RuntimeError("free-threaded CPython has not been qualified")
    root = pathlib.Path(package_path).resolve()
    for module in (np, astropy, astropy_iers_data):
        if not pathlib.Path(module.__file__).resolve().is_relative_to(root):
            raise RuntimeError("module loaded outside the configured package root")
    for name, version in expected.items():
        if importlib.metadata.version(name) != version:
            raise RuntimeError("distribution version mismatch: " + name)
    iers.conf.auto_download = False
    iers.conf.auto_max_age = None
    data = pathlib.Path(astropy_iers_data.__file__).parent / "data" / "finals2000A.all"
    with data.open("rb") as stream:
        if not stream.read(1):
            raise RuntimeError("IERS data is empty")
    return f"CPython {sys.version_info[:3]}; NumPy {np.__version__}; Astropy {astropy.__version__}; isolated"


def _array(value, dtype, n):
    if not isinstance(value, np.ndarray) or value.dtype != np.dtype(dtype):
        raise TypeError("array dtype")
    if value.ndim != 1 or value.size != n:
        raise ValueError("array rank/length")
    return value


def _wcs(spec):
    # Metadata comes from native library decode, not astropy.io.fits.Header.
    w = WCS(naxis=2)
    w.wcs.crpix = spec["crpix"]
    w.wcs.crval = spec["crval"]
    w.wcs.cd = spec["cd"]
    w.wcs.ctype = ["RA---TAN-SIP", "DEC--TAN-SIP"]
    w.wcs.cunit = ["deg", "deg"]
    w.wcs.lonpole = 180.0
    w.wcs.radesys = "ICRS"
    coeff = lambda p: np.asarray(p["coeff"], dtype=np.float64).reshape(p["order"] + 1, p["order"] + 1)
    a, b = coeff(spec["a"]), coeff(spec["b"])
    ap = None if spec["ap"] is None else coeff(spec["ap"])
    bp = None if spec["bp"] is None else coeff(spec["bp"])
    w.sip = Sip(a, b, ap, bp, np.asarray(spec["crpix"], dtype=np.float64))
    w.array_shape = (spec["naxis"][1], spec["naxis"][0])
    return w


def nearest_fits(x):
    # np.rint uses even ties; Rust f64::round and the FITS pixel policy do not.
    return np.where(x >= 0, np.floor(x + 0.5), np.ceil(x - 0.5))


# The exact transform is solved on a lattice of target pixels and interpolated
# between its nodes; a pixel whose nearest native pixel the interpolation error
# could change is then solved exactly, so the result is the exact transform's.
# _NODE is the node spacing in target pixels and _AMBIGUOUS how close to a
# rounding boundary (native pixels) counts. The lattice is checked against
# itself at twice the spacing, where the bilinear error of this smooth
# transform is four times larger, so the bound on that check keeps the
# interpolation within half of _AMBIGUOUS.
_NODE = 16
_CHECK = 2 * _NODE
_AMBIGUOUS = 2e-3
_CHECK_ERROR = 4 * (_AMBIGUOUS / 2)


def _native(out, w, x, y):
    ra, dec = out.all_pix2world(x, y, 1)
    # NoConvergence is an error even when best_solution is finite.
    px, py = w.all_world2pix(ra, dec, 1, tolerance=1e-12, maxiter=50,
                             adaptive=False, detect_divergence=True, quiet=False)
    if not (np.all(np.isfinite(px)) and np.all(np.isfinite(py))):
        raise ValueError("non-finite coordinate transform")
    return px, py


def _bilinear(nodes, step, kx, ky):
    # nodes is (2, rows, cols): both native axes are interpolated in one pass.
    fx, fy = kx / step, ky / step
    i = np.minimum(fx.astype(np.intp), nodes.shape[2] - 2)
    j = np.minimum(fy.astype(np.intp), nodes.shape[1] - 2)
    tx, ty = fx - i, (fy - j)[:, None]
    rows = nodes[:, :, i] * (1 - tx)
    rows += nodes[:, :, i + 1] * tx
    grid = rows[:, j] * (1 - ty)
    grid += rows[:, j + 1] * ty
    return grid


def _lattice(out, w, xs, ys):
    # The exact transform solved on a lattice and interpolated between its
    # nodes, or None when the tile is too small for one or the lattice misses
    # its error bound.
    width, height = xs.size, ys.size
    if width <= _CHECK or height <= _CHECK:
        return None
    nx = np.arange(2 * ((width - 2) // _CHECK + 1) + 1, dtype=np.float64) * _NODE
    ny = np.arange(2 * ((height - 2) // _CHECK + 1) + 1, dtype=np.float64) * _NODE
    dx, dy = xs[1] - xs[0], ys[1] - ys[0]
    nodes = np.stack(_native(out, w, *np.meshgrid(xs[0] + nx * dx, ys[0] + ny * dy)))
    if np.max(np.abs(_bilinear(nodes[:, ::2, ::2], _CHECK, nx, ny) - nodes)) > _CHECK_ERROR:
        return None
    return _bilinear(nodes, _NODE, np.arange(width, dtype=np.float64), np.arange(height, dtype=np.float64))


def geometry(spec, window, target, tile):
    w = _wcs(spec)  # Independent mutable WCS for this call/session.
    out = WCS(naxis=2)
    out.wcs.crpix = [(target["width"] + 1) / 2, (target["height"] + 1) / 2]
    out.wcs.crval = [target["ra"], target["dec"]]
    out.wcs.cd = [[-target["scale"], 0], [0, target["scale"]]]
    out.wcs.ctype = ["RA---TAN", "DEC--TAN"]
    out.wcs.cunit = ["deg", "deg"]
    out.wcs.lonpole = 180.0
    out.wcs.radesys = "ICRS"
    xs = np.arange(tile["width"], dtype=np.float64) + tile["x"] + 1
    ys = np.arange(tile["height"], dtype=np.float64) + tile["y"] + 1
    if tile["png"]:
        ys = target["height"] + 1 - ys
    # The native pixels this window can select.
    first = (max(window["x"], 1), max(window["y"], 1))
    last = (min(window["x"] + window["width"] - 1, spec["naxis"][0]),
            min(window["y"] + window["height"] - 1, spec["naxis"][1]))
    positions = _lattice(out, w, xs, ys)
    interpolated = positions is not None
    if not interpolated:
        positions = np.stack(_native(out, w, *np.meshgrid(xs, ys)))
    # floor(x + 0.5) is nearest_fits for every position a window can select.
    positions += 0.5
    nearest = np.floor(positions)
    if interpolated:
        # A position this close to a rounding boundary may round the other way
        # under the exact transform, so it is solved exactly. One native pixel
        # of margin keeps the pixels such a solve could bring into the window.
        positions -= nearest
        positions -= 0.5
        np.abs(positions, out=positions)
        near = positions[0] > 0.5 - _AMBIGUOUS
        near |= positions[1] > 0.5 - _AMBIGUOUS
        near &= ((nearest[0] >= first[0] - 1) & (nearest[0] <= last[0] + 1)
                 & (nearest[1] >= first[1] - 1) & (nearest[1] <= last[1] + 1))
        rows, cols = np.divmod(np.flatnonzero(near.reshape(-1)), xs.size)
        if rows.size:
            exact = _native(out, w, xs[cols], ys[rows])
            nearest[0, rows, cols], nearest[1, rows, cols] = nearest_fits(exact[0]), nearest_fits(exact[1])
    ix, iy = nearest[0].reshape(-1), nearest[1].reshape(-1)
    valid = (ix >= first[0]) & (ix <= last[0]) & (iy >= first[1]) & (iy <= last[1])
    iy -= window["y"]
    iy *= window["width"]
    iy += ix
    iy -= window["x"]
    return np.where(valid, iy, -1).astype(np.int64)


def _bad(flags, bits, policy):
    if policy == "none":
        return np.zeros(flags.shape, dtype=bool)
    bit = lambda i: np.uint32(1 << i) if i < 32 else np.uint32(0)
    ignore = bit(bits["source"]) | bit(bits["fullsample"])
    defects = np.full(flags.shape, ~ignore, dtype=np.uint32)
    if policy in ("composite", "delta"):
        source = (flags & bit(bits["source"])) != 0
        ramp = bit(bits["transient"]) | bit(bits["overflow"]) | bit(bits["sur_error"])
        defects[source] &= ~ramp
    if policy == "delta":
        defects &= ~bit(bits["outlier"])
    return (flags & defects) != 0


# Running sums are owned and charged by Rust between calls; each call works on
# private copies and Rust keeps them only when the whole call succeeds.
_COUNT = {"values": None, "science": "uint16", "depth": "uint64", "complete": "uint16"}


def _state(kind, sum_f, sum_w, count):
    n = sum_f.size
    _array(sum_f, "float64", n)
    _array(sum_w, "float64", n)
    if _COUNT[kind] is None:
        if count is not None:
            raise TypeError("unexpected count state")
    else:
        _array(count, _COUNT[kind], n)
    return n


def accumulate(kind, state, image, variance, zodi, flags, native_bands, validity, indices, bits, policy, bands):
    if kind == "complete":
        raise ValueError("complete-band accumulator needs finished depth bands")
    sum_f, sum_w, count = state
    tile = _state(kind, sum_f, sum_w, count)
    n = image.size
    _array(image, "float32", n)
    _array(variance, "float32", n)
    _array(zodi, "float32", n)
    _array(flags, "uint32", n)
    _array(native_bands, "uint16", n)
    _array(validity, "bool", n)
    _array(indices, "int64", tile)
    if np.any(indices < -1) or np.any(indices >= n):
        raise ValueError("native index outside admitted input")
    support = indices >= 0
    positions = np.flatnonzero(support)
    gather = indices[support]
    # Library owns gathering, flag masks, selection, and all image arithmetic.
    flux = np.subtract(image[gather], zodi[gather], dtype=np.float32)
    var = variance[gather]
    accepted = validity[gather] & np.isfinite(flux) & np.isfinite(var) & (var > 0) & ~_bad(flags[gather], bits, policy)
    if bands:
        accepted &= np.isin(native_bands[gather], np.asarray(bands, dtype=np.uint16))
    positions = positions[accepted]
    flux = flux[accepted].astype(np.float64)
    weight = np.reciprocal(var[accepted].astype(np.float64))
    if kind == "depth" and np.any(count[positions] == np.iinfo(np.uint64).max):
        raise OverflowError("Specific u64 count")
    # One exposure per call, in admitted order. No exposure cube / np.mean.
    sum_f[positions] += np.multiply(flux, weight, dtype=np.float64)
    sum_w[positions] += weight
    if kind == "science":
        count[positions] = np.minimum(count[positions].astype(np.uint32) + 1, 65535).astype(np.uint16)
    if kind == "depth":
        count[positions] += np.uint64(1)


def add_band(state, value, variance, counts):
    sum_f, sum_w, count = state
    n = _state("complete", sum_f, sum_w, count)
    _array(value, "float64", n)
    _array(variance, "float64", n)
    _array(counts, "uint64", n)
    valid = (counts > 0) & np.isfinite(value) & np.isfinite(variance) & (variance > 0)
    sum_f[valid] += value[valid].astype(np.float64)
    sum_w[valid] += variance[valid].astype(np.float64)
    count[valid] += np.uint16(1)


def output(kind, state, nbands):
    sum_f, sum_w, count = state
    n = _state(kind, sum_f, sum_w, count)
    dtype = np.float64 if kind == "depth" else np.float32
    value = np.full(n, np.nan, dtype=dtype)
    if kind == "complete":
        valid = count == nbands
        value[valid] = sum_f[valid] / nbands
        variance = np.full(n, np.nan, dtype=np.float32)
        variance[valid] = sum_w[valid] / (nbands ** 2)
        return value, variance, None
    valid = sum_w > 0
    value[valid] = sum_f[valid] / sum_w[valid]
    if kind == "values":
        return value, None, None
    variance = np.full(n, np.nan, dtype=dtype)
    variance[valid] = np.reciprocal(sum_w[valid])
    return value, variance, count.copy()


def difference(plus, minus, pv, mv, pc, mc):
    n = plus.size
    _array(plus, "float32", n)
    _array(minus, "float32", n)
    valid = np.isfinite(plus) & np.isfinite(minus)
    value = np.full(n, np.nan, dtype=np.float32)
    value[valid] = np.subtract(plus[valid], minus[valid], dtype=np.float32)
    if pv is None:
        return value, None, None
    for v in (pv, mv):
        _array(v, "float32", n)
    for c in (pc, mc):
        _array(c, "uint16", n)
    variance = np.full(n, np.nan, dtype=np.float32)
    variance[valid] = np.add(pv[valid], mv[valid], dtype=np.float32)
    counts = np.zeros(n, dtype=np.uint16)
    counts[valid] = np.minimum(pc[valid].astype(np.uint32) + mc[valid].astype(np.uint32), 65535).astype(np.uint16)
    return value, variance, counts


def spectrum(flux, variance, band, rejected):
    n = flux.size
    _array(flux, "float64", n)
    _array(variance, "float64", n)
    _array(band, "uint16", n)
    _array(rejected, "bool", n)
    accepted = ~rejected & np.isfinite(flux) & np.isfinite(variance) & (variance > 0)
    sum_f, sum_w = np.zeros(102), np.zeros(102)
    counts, invalid = np.zeros(102, dtype=np.uint64), np.zeros(102, dtype=np.uint64)
    last, first = np.full(102, -1, dtype=np.int64), np.full(102, -1, dtype=np.int64)
    for i in range(n):  # Explicit sample order, no unqualified parallel reduction.
        b = int(band[i])
        if b == 65535:
            continue
        if b >= 102:
            raise ValueError("spectrum band")
        if accepted[i]:
            weight = np.reciprocal(variance[i])
            sum_f[b] = np.add(sum_f[b], np.multiply(weight, flux[i]))
            sum_w[b] = np.add(sum_w[b], weight)
            counts[b] += np.uint64(1)
            last[b] = i
        else:
            invalid[b] += np.uint64(1)
            if first[b] < 0:
                first[b] = i
    value, error = np.full(102, np.nan), np.full(102, np.nan)
    valid = sum_w > 0
    value[valid] = np.divide(sum_f[valid], sum_w[valid])
    error[valid] = np.reciprocal(np.sqrt(sum_w[valid]))
    return value, error, counts, invalid, last, first, accepted
