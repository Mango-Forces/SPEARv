"""Bounded tests of the actual embedded backend, not copied reducer oracles."""
import importlib.util
import pathlib
import unittest
from unittest.mock import patch

import numpy as np
from astropy.wcs import NoConvergence

spec = importlib.util.spec_from_file_location("spearv_math", pathlib.Path(__file__).parents[1] / "python" / "spearv_math.py")
math = importlib.util.module_from_spec(spec)
spec.loader.exec_module(math)
BITS = dict(source=21, fullsample=12, transient=0, overflow=1, sur_error=2, outlier=19)


class LibraryEdges(unittest.TestCase):
    def test_half_ties_are_away_from_zero_not_even(self):
        np.testing.assert_array_equal(math.nearest_fits(np.array([-2.5, -1.5, -.5, .5, 1.5, 2.5])), [-3, -2, -1, 1, 2, 3])

    def test_shapes_dtype_rank_and_output_nonalias(self):
        state = (np.zeros(2), np.zeros(2), np.zeros(2, dtype=np.uint16))
        args = [np.array([3, 4], dtype=np.float32), np.ones(2, dtype=np.float32),
                np.zeros(2, dtype=np.float32), np.zeros(2, dtype=np.uint32),
                np.zeros(2, dtype=np.uint16), np.ones(2, dtype=bool), np.arange(2, dtype=np.int64), BITS, "none", []]
        for index, wrong in [(0, args[0].astype(np.float64)), (1, args[1].reshape(1, 2)), (2, args[2][:1])]:
            copy = args.copy()
            copy[index] = wrong
            with self.assertRaises((ValueError, TypeError)):
                math.accumulate("science", state, *copy)
        with self.assertRaises(TypeError):
            math.accumulate("science", (state[0], state[1], state[2].astype(np.uint64)), *args)
        math.accumulate("science", state, *args)
        value, variance, count = math.output("science", state, 0)
        value[:] = 99
        variance[:] = 99
        count[:] = 99
        np.testing.assert_array_equal(math.output("science", state, 0)[0], [3, 4])

    def test_u16_saturation_u64_large_counts_and_overflow(self):
        args = [np.array([3], dtype=np.float32), np.ones(1, dtype=np.float32),
                np.zeros(1, dtype=np.float32), np.zeros(1, dtype=np.uint32),
                np.zeros(1, dtype=np.uint16), np.ones(1, dtype=bool), np.zeros(1, dtype=np.int64), BITS, "spectrum", [0]]
        science = (np.zeros(1), np.zeros(1), np.array([65534], dtype=np.uint16))
        math.accumulate("science", science, *args)
        math.accumulate("science", science, *args)
        self.assertEqual(math.output("science", science, 0)[2][0], 65535)
        depth = (np.zeros(1), np.zeros(1), np.array([2**32], dtype=np.uint64))
        math.accumulate("depth", depth, *args)
        self.assertEqual(math.output("depth", depth, 0)[2][0], 2**32 + 1)
        depth[2][0] = 2**64 - 1
        old = depth[0].copy()
        with self.assertRaises(OverflowError):
            math.accumulate("depth", depth, *args)
        np.testing.assert_array_equal(depth[0], old)

    def test_depth_not_rounded_before_complete_band_mean(self):
        sector = (np.zeros(1), np.zeros(1), np.zeros(1, dtype=np.uint16))
        math.add_band(sector, np.array([16777217.], dtype=np.float64),
                      np.ones(1, dtype=np.float64), np.ones(1, dtype=np.uint64))
        math.add_band(sector, np.array([-16777216.], dtype=np.float64),
                      np.ones(1, dtype=np.float64), np.ones(1, dtype=np.uint64))
        self.assertEqual(math.output("complete", sector, 2)[0][0], .5)

    def test_empty_values_output_is_nodata_f32_without_count(self):
        state = (np.zeros(256 * 153), np.zeros(256 * 153), None)
        value, variance, count = math.output("values", state, 0)
        self.assertEqual(value.dtype, np.float32)
        self.assertTrue(np.all(np.isnan(value)))
        self.assertIsNone(variance)
        self.assertIsNone(count)

    def test_finite_best_solution_is_not_transform_success(self):
        native = {
            "crpix": [1., 1.], "crval": [10., 20.], "cd": [[-.001, 0.], [0., .001]], "naxis": [1, 1],
            "a": {"order": 0, "coeff": [0.]}, "b": {"order": 0, "coeff": [0.]}, "ap": None, "bp": None,
        }
        error = NoConvergence("deliberate non-convergence", best_solution=np.array([[1., 1.]]),
                              accuracy=np.array([[.1, .1]]), niter=50, divergent=None, slow_conv=np.array([0]))
        with patch("astropy.wcs.WCS.all_world2pix", side_effect=error):
            with self.assertRaises(NoConvergence):
                math.geometry(native, dict(x=1, y=1, width=1, height=1),
                              dict(ra=10., dec=20., width=1, height=1, scale=.001),
                              dict(x=0, y=0, width=1, height=1, png=False))


if __name__ == "__main__":
    unittest.main()
