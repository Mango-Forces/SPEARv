/* Matching-header, opaque ABI. No fitsfile layout or FITS byte parsing crosses it. */
#include <fitsio2.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <limits.h>

_Static_assert(sizeof(LONGLONG) == 8, "CFITSIO LONGLONG must be 64 bits");
_Static_assert(sizeof(int) == 4, "CFITSIO TINT must be 32 bits");
_Static_assert(sizeof(float) == 4 && sizeof(double) == 8, "IEEE widths required");
_Static_assert(CFITSIO_MAJOR == 4 && CFITSIO_MINOR == 7 && CFITSIO_MICRO == 0,
    "shim requires pinned CFITSIO 4.7.0 headers");

extern int sv_rs_open(char *, int, int *);
extern int sv_rs_close(int);
extern int sv_rs_size(int, int64_t *);
extern int sv_rs_seek(int, int64_t);
extern int sv_rs_read(int, void *, long);
static int sv_size(int h, LONGLONG *size) {
    int64_t value = 0; int s = sv_rs_size(h, &value); *size = (LONGLONG)value; return s;
}
static int sv_seek(int h, LONGLONG pos) { return sv_rs_seek(h, (int64_t)pos); }
static int no_write(int h, void *p, long n) {
    (void)h; (void)p; (void)n; return READONLY_FILE;
}
int sv_init(void) {
    float version = 0;
    fits_get_version(&version);
    if (version != 4.0f + 0.07f) return BAD_OPTION;
    if (!fits_is_reentrant()) return BAD_OPTION;
    int s = fits_init_cfitsio();
    if (s) return s;
    return fits_register_driver("spearvfits://", NULL, NULL, NULL, NULL, NULL, NULL,
        sv_rs_open, NULL, NULL, sv_rs_close, NULL, sv_size, NULL, sv_seek, sv_rs_read, no_write);
}
int sv_reentrant(void) { return fits_is_reentrant(); }
void sv_status(int status, char *out) { fits_get_errstatus(status, out); }

typedef struct { fitsfile *file; void *memory; size_t size; } SvFile;
int sv_open(const char *name, void *bytes, size_t len, int mode, void **out) {
    SvFile *p = calloc(1, sizeof(*p));
    if (!p) return MEMORY_ALLOCATION;
    int s = 0;
    if (mode == 0) fits_open_diskfile(&p->file, name, READONLY, &s);
    if (mode == 1) fits_open_file(&p->file, name, READONLY, &s);
    if (mode == 2) {
        p->memory = bytes; p->size = len;
        fits_open_memfile(&p->file, "owned-memory", READONLY, &p->memory, &p->size, 0, NULL, &s);
    }
    if (s) {
        int close = 0;
        if (p->file) fits_close_file(p->file, &close);
        free(p); *out = NULL; return s;
    }
    *out = p; return 0;
}
int sv_close(void *opaque) {
    SvFile *p = opaque; int s = 0;
    if (!p) return 0;
    if (p->file) fits_close_file(p->file, &s);
    free(p); return s;
}
int sv_move(void *p, int hdu, int *kind) {
    int s = 0; fits_movabs_hdu(((SvFile *)p)->file, hdu, kind, &s); return s;
}
int sv_named(void *p, const char *name, int version) {
    int s = 0; fits_movnam_hdu(((SvFile *)p)->file, ANY_HDU, (char *)name, version, &s); return s;
}
int sv_key_string(void *p, const char *key, char *out, size_t capacity) {
    int s = 0; char *value = NULL;
    fits_read_key_longstr(((SvFile *)p)->file, key, &value, NULL, &s);
    if (!s) {
        size_t length = strlen(value);
        if (length >= capacity) s = BAD_DIMEN;
        else memcpy(out, value, length + 1);
    }
    if (value) { int cleanup = 0; fits_free_memory(value, &cleanup); }
    return s;
}
int sv_key_double(void *p, const char *key, double *out) {
    int s = 0; fits_read_key(((SvFile *)p)->file, TDOUBLE, key, out, NULL, &s); return s;
}
int sv_key_int(void *p, const char *key, int64_t *out) {
    int s = 0; LONGLONG value = 0;
    fits_read_key(((SvFile *)p)->file, TLONGLONG, key, &value, NULL, &s);
    *out = (int64_t)value; return s;
}
int sv_cards_count(void *p, int *count) {
    int s = 0, more = 0; fits_get_hdrspace(((SvFile *)p)->file, count, &more, &s); return s;
}
int sv_card(void *p, int index, char *name, char *value, char *comment, char *raw) {
    int s = 0; fitsfile *f = ((SvFile *)p)->file;
    fits_read_keyn(f, index, name, value, comment, &s);
    fits_read_record(f, index, raw, &s); return s;
}
int sv_image_rank(void *p, int *rank) {
    int s = 0; fits_get_img_dim(((SvFile *)p)->file, rank, &s); return s;
}
int sv_image(void *p, int rank, int64_t *axes, int *bitpix, int *compressed,
             int *quantized, double *scale, double *zero) {
    fitsfile *f = ((SvFile *)p)->file; int s = 0;
    LONGLONG *dims = calloc((size_t)rank + 1, sizeof(*dims));
    if (!dims) return MEMORY_ALLOCATION;
    fits_get_img_sizell(f, rank, dims, &s);
    fits_get_img_type(f, bitpix, &s);
    *compressed = fits_is_compressed_image(f, &s);
    *quantized = 0;
    for (int i = 0; i < rank; i++) axes[i] = (int64_t)dims[i];
    free(dims);
    if (!s && *compressed && *bitpix < 0) {
        /* Native column/key metadata, same classification as pinned CFITSIO;
         * no compression descriptors, raster bytes or cache layout accessed. */
        int a = 0, b = 0, column = 0;
        fits_get_colnum(f, CASEINSEN, "ZSCALE", &column, &a);
        fits_get_colnum(f, CASEINSEN, "ZZERO", &column, &b);
        if (a != 0 && a != COL_NOT_FOUND) return a;
        if (b != 0 && b != COL_NOT_FOUND) return b;
        if (a == 0 || b == 0) {
            char method[FLEN_VALUE] = {0}; int key = 0;
            fits_read_key(f, TSTRING, "ZQUANTIZ", method, NULL, &key);
            if (key != 0 && key != KEY_NO_EXIST) return key;
            *quantized = key == KEY_NO_EXIST || strcmp(method, "NONE") != 0;
        }
    }
    *scale = 1; *zero = 0;
    if (!s) {
        int key = 0; fits_read_key(f, TDOUBLE, "BSCALE", scale, NULL, &key);
        if (key != 0 && key != KEY_NO_EXIST) return key;
        key = 0; fits_read_key(f, TDOUBLE, "BZERO", zero, NULL, &key);
        if (key != 0 && key != KEY_NO_EXIST) return key;
    }
    return s;
}
/* Row-wise reads keep rectangular requests bounded, including one-pixel axes.
 * mode 0 preserves unscaled IEEE with zero NULLVAL; mode 1 supplies null flags.
 * modes 2/3 are signed-bit FLAGS / physical unsigned-BZERO FLAGS. */
int sv_pixels(void *p, int64_t x, int64_t y, int64_t w, int64_t h,
              int mode, int precision, void *out, unsigned char *valid) {
    fitsfile *f = ((SvFile *)p)->file; int s = 0, any = 0;
    for (int64_t row = 0; row < h && !s; row++) {
        LONGLONG first[2] = {(LONGLONG)x + 1, (LONGLONG)y + row + 1};
        size_t offset = (size_t)(row * w);
        if (mode == 0) {
            if (precision == 32) {
                float zero = 0;
                fits_read_pixll(f, TFLOAT, first, w, &zero, (float *)out + offset, &any, &s);
            } else {
                double zero = 0;
                fits_read_pixll(f, TDOUBLE, first, w, &zero, (double *)out + offset, &any, &s);
            }
        } else {
            int type = mode == 2 ? TINT : mode == 3 ? TUINT : precision == 32 ? TFLOAT : TDOUBLE;
            size_t width = mode >= 2 || precision == 32 ? 4 : 8;
            fits_read_pixnullll(f, type, first, w, (char *)out + offset * width,
                (char *)valid + offset, &any, &s);
        }
    }
    if (s || mode == 0) return s;
    for (int64_t i = 0; i < w * h; i++) {
        if (valid[i]) {
            if (mode >= 2) ((uint32_t *)out)[i] = 0;
            else if (precision == 32) ((float *)out)[i] = NAN;
            else ((double *)out)[i] = NAN;
        } else if (mode == 2) {
            /* memcpy avoids C aliasing UB while preserving stored signed bits. */
            int32_t signed_value;
            memcpy(&signed_value, (char *)out + (size_t)i * 4, 4);
            ((uint32_t *)out)[i] = (uint32_t)signed_value;
        }
        valid[i] = valid[i] ? 0 : 1;
    }
    return 0;
}
int sv_table(void *p, int *cols, int64_t *rows) {
    int s = 0; LONGLONG n = 0; fitsfile *f = ((SvFile *)p)->file;
    fits_get_num_cols(f, cols, &s); fits_get_num_rowsll(f, &n, &s); *rows = (int64_t)n; return s;
}
int sv_column(void *p, int col, int capacity, int *type, int64_t *repeat,
              int64_t *width, int *rank, int64_t *dims) {
    int s = 0; LONGLONG r = 0, w = 0; fitsfile *f = ((SvFile *)p)->file;
    fits_get_coltypell(f, col, type, &r, &w, &s);
    *repeat = (int64_t)r; *width = (int64_t)w;
    if (s || *type < 0) { *rank = 0; return s; }
    LONGLONG *shape = calloc((size_t)capacity + 1, sizeof(*shape));
    if (!shape) return MEMORY_ALLOCATION;
    fits_read_tdimll(f, col, capacity, rank, shape, &s);
    for (int i = 0; i < capacity; i++) dims[i] = (int64_t)shape[i];
    free(shape); return s;
}
int sv_descriptor(void *p, int col, int64_t row, int64_t *count) {
    int s = 0; LONGLONG n = 0, offset = 0;
    fits_read_descriptll(((SvFile *)p)->file, col, row, &n, &offset, &s);
    *count = (int64_t)n; return s;
}
int sv_col_read(void *p, int col, int64_t row, int64_t first, int64_t count,
                int integer, void *out, unsigned char *valid) {
    int s = 0, any = 0;
    if (integer) {
        /* Native LONGLONG may differ from int64_t typedef (ARM). */
        LONGLONG *values = calloc((size_t)count, sizeof(*values));
        if (!values && count) return MEMORY_ALLOCATION;
        fits_read_colnull(((SvFile *)p)->file, TLONGLONG, col, row, first, count,
            values, (char *)valid, &any, &s);
        for (int64_t i = 0; i < count; i++) ((int64_t *)out)[i] = valid[i] ? 0 : (int64_t)values[i];
        free(values);
    } else {
        fits_read_colnull(((SvFile *)p)->file, TDOUBLE, col, row, first, count, out,
            (char *)valid, &any, &s);
        for (int64_t i = 0; i < count; i++) if (valid[i]) ((double *)out)[i] = NAN;
    }
    for (int64_t i = 0; i < count; i++) valid[i] = valid[i] ? 0 : 1;
    return s;
}
