/* Small qualification fixtures created by the actual library, not a byte oracle.
 * This symbol is not exposed by the Rust library API. */
#include <fitsio.h>
#include <stdint.h>
#include <math.h>
#include <limits.h>
int sv_test_fixture(const char *path) {
    fitsfile *f = NULL; int s = 0; long axes[2] = {3,2};
    fits_create_diskfile(&f, path, &s); fits_create_img(f, BYTE_IMG, 0, NULL, &s);
    int32_t signed_values[6] = {-1, INT32_MIN, 0, 1, INT32_MAX, -2};
    uint32_t unsigned_values[6] = {0, 1, 2147483648U, 4294967295U, 123, 2147483647U};
    fits_create_img(f, LONG_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "SIGNED", NULL, &s);
    fits_write_img(f, TINT, 1, 6, signed_values, &s);
    fits_create_img(f, ULONG_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "UNSIGNED", NULL, &s);
    fits_write_img(f, TUINT, 1, 6, unsigned_values, &s);
    int16_t integers[6] = {1, 2, -32768, -3, 4, 5};
    double scale = 2, zero = 10; long blank = -32768;
    fits_create_img(f, SHORT_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "SCALED", NULL, &s);
    fits_write_img(f, TSHORT, 1, 6, integers, &s);
    fits_update_key(f, TDOUBLE, "BSCALE", &scale, NULL, &s);
    fits_update_key(f, TDOUBLE, "BZERO", &zero, NULL, &s);
    fits_update_key(f, TLONG, "BLANK", &blank, NULL, &s);
    float floats[6] = {0x1p-149f, NAN, INFINITY, -INFINITY, -0.0f, 2};
    fits_create_img(f, FLOAT_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "FLOAT", NULL, &s);
    fits_update_key(f, TSTRING, "BUNIT", "MJy / sr", NULL, &s);
    char longtext[181];
    for (int i = 0; i < 180; i++) longtext[i] = (char)('a' + i % 26);
    longtext[180] = 0;
    fits_write_key_longstr(f, "LONGTEXT", longtext, NULL, &s);
    fits_write_img(f, TFLOAT, 1, 6, floats, &s);
    double doubles[6] = {0x1p-1074, NAN, INFINITY, -INFINITY, -0.0, 2};
    fits_create_img(f, DOUBLE_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "DOUBLE", NULL, &s);
    fits_write_img(f, TDOUBLE, 1, 6, doubles, &s);
    long one[2] = {1,3}; float narrow[3] = {1,2,3};
    fits_create_img(f, FLOAT_IMG, 2, one, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "ONE", NULL, &s);
    fits_write_img(f, TFLOAT, 1, 3, narrow, &s);
    for (int i = 0; i < 2; i++) {
        fits_create_img(f, FLOAT_IMG, 2, axes, &s);
        fits_update_key(f, TSTRING, "EXTNAME", i == 0 ? "CWAVE" : "CBAND", NULL, &s);
        fits_update_key(f, TSTRING, "BUNIT", "fixture-unit", NULL, &s);
        fits_write_img(f, TFLOAT, 1, 6, floats, &s);
    }
    char *names[] = {"VALUES"}, *forms[] = {"6J"}, *units[] = {"retained-unit"};
    int32_t values[6] = {1, 2, -999, 4, 5, 6}; long dims[2] = {2,3}, null = -999;
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, forms, units, "TABLE", &s);
    fits_write_col(f, TINT, 1, 1, 1, 6, values, &s); fits_write_tdim(f, 1, 2, dims, &s);
    fits_update_key(f, TDOUBLE, "TSCAL1", &scale, NULL, &s);
    fits_update_key(f, TDOUBLE, "TZERO1", &zero, NULL, &s);
    fits_update_key(f, TLONG, "TNULL1", &null, NULL, &s);
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, forms, units, "BADTDIM", &s);
    fits_write_col(f, TINT, 1, 1, 1, 6, values, &s);
    fits_update_key(f, TSTRING, "TDIM1", "(2,2)", NULL, &s);
    char *variable_forms[] = {"1PJ"};
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, variable_forms, units, "VARIABLE", &s);
    fits_write_col(f, TINT, 1, 1, 1, 6, values, &s);
    char *real_forms[] = {"2D"}; double reals[2] = {1.7, -2.5};
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, real_forms, units, "REALCOL", &s);
    fits_write_col(f, TDOUBLE, 1, 1, 1, 2, reals, &s);
    double half = 0.5;
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, forms, units, "HALFSCALE", &s);
    fits_write_col(f, TINT, 1, 1, 1, 6, values, &s);
    fits_update_key(f, TDOUBLE, "TSCAL1", &half, NULL, &s);
    char *wide_forms[] = {"1K"}; long long big[1] = {(1LL << 62) + 1}; double one_zero = 1;
    fits_create_tbl(f, BINARY_TBL, 1, 1, names, wide_forms, units, "OFFSET64", &s);
    fits_write_col(f, TLONGLONG, 1, 1, 1, 1, big, &s);
    fits_update_key(f, TDOUBLE, "TZERO1", &one_zero, NULL, &s);
    fits_set_compression_type(f, RICE_1, &s);
    fits_create_img(f, LONG_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "RICEFLAGS", NULL, &s);
    fits_write_img(f, TINT, 1, 6, signed_values, &s);
    long block[2] = {2, 2};
    fits_set_tile_dim(f, 2, block, &s);
    fits_create_img(f, LONG_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "RICEBLOCK", NULL, &s);
    fits_write_img(f, TINT, 1, 6, signed_values, &s);
    long rows[2] = {3, 1};
    fits_set_tile_dim(f, 2, rows, &s);
    float quantized[6] = {1, NAN, 2, 3, 4, 5};
    fits_create_img(f, FLOAT_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "QUANTIZED", NULL, &s);
    fits_write_img(f, TFLOAT, 1, 6, quantized, &s);
    fits_set_compression_type(f, GZIP_1, &s);
    fits_set_quantize_level(f, 0, &s);
    fits_create_img(f, FLOAT_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "LOSSLESS", NULL, &s);
    fits_write_img(f, TFLOAT, 1, 6, floats, &s);
    fits_create_img(f, DOUBLE_IMG, 2, axes, &s);
    fits_update_key(f, TSTRING, "EXTNAME", "LOSSLESS64", NULL, &s);
    fits_write_img(f, TDOUBLE, 1, 6, doubles, &s);
    int close = 0; if (f) fits_close_file(f, &close);
    return s ? s : close;
}
