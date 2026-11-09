// Thin wrapper over LibRaw: LibRaw's settings live in a large struct, whose
// layout is better left to its own header than mirrored by hand in Rust.

#include <libraw/libraw.h>
#include <stddef.h>

// Develops a RAW file to 8-bit sRGB like a good "as shot" conversion: the
// camera's own white balance and color matrix, high-quality demosaicing (DHT),
// highlight blending, and LibRaw's usual tone curve and auto brightness. The
// result is upright (the file's orientation applied). Returns NULL on failure
// (with LibRaw's error code in *error); free the result with fv_raw_free.
libraw_processed_image_t *fv_raw_decode(const void *data, size_t len, int *error) {
    libraw_data_t *raw = libraw_init(0);
    if (!raw) {
        *error = LIBRAW_UNSPECIFIED_ERROR;
        return NULL;
    }
    raw->params.use_camera_wb = 1;
    raw->params.use_camera_matrix = 1;
    raw->params.output_color = 1; // sRGB
    raw->params.output_bps = 8;
    raw->params.user_qual = 11; // DHT
    raw->params.highlight = 2;  // blend
    libraw_processed_image_t *image = NULL;
    int rc = libraw_open_buffer(raw, data, len);
    if (rc == LIBRAW_SUCCESS) rc = libraw_unpack(raw);
    if (rc == LIBRAW_SUCCESS) rc = libraw_dcraw_process(raw);
    if (rc == LIBRAW_SUCCESS) image = libraw_dcraw_make_mem_image(raw, &rc);
    *error = rc;
    libraw_close(raw);
    return image;
}

int fv_raw_width(const libraw_processed_image_t *image) { return image->width; }
int fv_raw_height(const libraw_processed_image_t *image) { return image->height; }
int fv_raw_colors(const libraw_processed_image_t *image) { return image->colors; }
int fv_raw_bits(const libraw_processed_image_t *image) { return image->bits; }
const unsigned char *fv_raw_pixels(const libraw_processed_image_t *image) { return image->data; }
size_t fv_raw_size(const libraw_processed_image_t *image) { return image->data_size; }
void fv_raw_free(libraw_processed_image_t *image) { libraw_dcraw_clear_mem(image); }
const char *fv_raw_error(int code) { return libraw_strerror(code); }

typedef struct {
    int width, height, flip, dng;
    long long taken;
} FvProbe;

// Opens a RAW file just far enough to read its details. Returns 0 on
// success; fails for anything that isn't a RAW file with sensor data
// (LibRaw also opens some plain TIFFs, which have no color filter array).
int fv_raw_probe(const void *data, size_t len, FvProbe *out) {
    libraw_data_t *raw = libraw_init(0);
    if (!raw) return -1;
    int rc = libraw_open_buffer(raw, data, len);
    if (rc == LIBRAW_SUCCESS) {
        int sensor = raw->idata.filters != 0 || raw->idata.is_foveon || raw->idata.dng_version != 0;
        if (!sensor) {
            rc = -2;
        } else {
            out->width = raw->sizes.width;
            out->height = raw->sizes.height;
            out->flip = raw->sizes.flip;
            out->dng = raw->idata.dng_version != 0;
            out->taken = (long long)raw->other.timestamp;
        }
    }
    libraw_close(raw);
    return rc;
}

typedef struct {
    char make[64], model[64], lens[128], software[64], artist[64], serial[64];
    float iso, shutter, aperture, focal, flash;
    int focal35, raw_width, raw_height;
    long long taken;
    float latitude[3], longitude[3], altitude;
    char latref, longref, gps;
} FvDetails;

static void copy(char *to, const char *from, size_t size) {
    size_t i = 0;
    for (; i + 1 < size && from[i]; i++) to[i] = from[i];
    to[i] = 0;
}

// Shooting details LibRaw reads from a RAW file's headers (reading only what
// it needs from the file), for files whose EXIF the EXIF reader can't find
// (CR3 keeps it in its own boxes). Returns 0 on success.
int fv_raw_details(const char *path, FvDetails *out) {
    libraw_data_t *raw = libraw_init(0);
    if (!raw) return -1;
    int rc = libraw_open_file(raw, path);
    if (rc == LIBRAW_SUCCESS) {
        copy(out->make, raw->idata.make, sizeof out->make);
        copy(out->model, raw->idata.model, sizeof out->model);
        copy(out->lens, raw->lens.Lens[0] ? raw->lens.Lens : raw->lens.makernotes.Lens,
             sizeof out->lens);
        copy(out->software, raw->idata.software, sizeof out->software);
        copy(out->artist, raw->other.artist, sizeof out->artist);
        copy(out->serial, raw->shootinginfo.BodySerial, sizeof out->serial);
        out->iso = raw->other.iso_speed;
        out->shutter = raw->other.shutter;
        out->aperture = raw->other.aperture;
        out->focal = raw->other.focal_len;
        out->flash = raw->color.flash_used;
        out->focal35 = raw->lens.FocalLengthIn35mmFormat;
        out->raw_width = raw->sizes.raw_width;
        out->raw_height = raw->sizes.raw_height;
        out->taken = (long long)raw->other.timestamp;
        for (int i = 0; i < 3; i++) {
            out->latitude[i] = raw->other.parsed_gps.latitude[i];
            out->longitude[i] = raw->other.parsed_gps.longitude[i];
        }
        out->altitude = raw->other.parsed_gps.altitude;
        out->latref = raw->other.parsed_gps.latref;
        out->longref = raw->other.parsed_gps.longref;
        out->gps = raw->other.parsed_gps.gpsparsed;
    }
    libraw_close(raw);
    return rc;
}
