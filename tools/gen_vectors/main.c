// gen_vectors: emit on-disk LittleFS v2 images using the C reference,
// for the Rust crate to mount and verify.
//
// Geometry matches `tests/common::MemStorage` exactly so the resulting
// images can be loaded into MemStorage::data without translation:
//   read_size = 16
//   prog_size = 16
//   block_size = 256
//   block_count = 8
//   cache_size = 64
//   lookahead_size = 8
//
// Each scenario is named after the file it writes under tests/vectors/.
// The driver runs every scenario sequentially against a fresh image and
// writes the raw 2 KiB storage buffer to the named output.

#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <assert.h>

#include "littlefs/lfs.h"

#define BLOCK_SIZE   256
#define BLOCK_COUNT  8
#define PROG_SIZE    16
#define READ_SIZE    16
#define CACHE_SIZE   64
#define LOOKAHEAD    8
#define IMAGE_BYTES  (BLOCK_SIZE * BLOCK_COUNT)

// RAM-backed storage: one byte buffer the size of the whole device.
// Fresh state is all 0xFF (post-erase).
static uint8_t image[IMAGE_BYTES];

static int ram_read(const struct lfs_config *c, lfs_block_t block,
                    lfs_off_t off, void *buffer, lfs_size_t size) {
    (void)c;
    if ((size_t)block * BLOCK_SIZE + off + size > IMAGE_BYTES) return LFS_ERR_IO;
    memcpy(buffer, &image[block * BLOCK_SIZE + off], size);
    return 0;
}

static int ram_prog(const struct lfs_config *c, lfs_block_t block,
                    lfs_off_t off, const void *buffer, lfs_size_t size) {
    (void)c;
    if ((size_t)block * BLOCK_SIZE + off + size > IMAGE_BYTES) return LFS_ERR_IO;
    // NOR semantics: only 1 -> 0 transitions allowed. Enforce so the C
    // ref's commit sequencing is exercised honestly.
    const uint8_t *src = buffer;
    uint8_t *dst = &image[block * BLOCK_SIZE + off];
    for (size_t i = 0; i < size; i++) {
        if ((dst[i] & src[i]) != src[i]) {
            fprintf(stderr, "ram_prog: 0->1 bit flip at block %u off %u byte %zu (dst=%02x src=%02x)\n",
                    block, off, i, dst[i], src[i]);
            return LFS_ERR_IO;
        }
        dst[i] = src[i];
    }
    return 0;
}

static int ram_erase(const struct lfs_config *c, lfs_block_t block) {
    (void)c;
    if (block >= BLOCK_COUNT) return LFS_ERR_IO;
    memset(&image[block * BLOCK_SIZE], 0xff, BLOCK_SIZE);
    return 0;
}

static int ram_sync(const struct lfs_config *c) {
    (void)c;
    return 0;
}

// Static buffers sized per the geometry. Re-zeroed at the start of
// each scenario via reset().
static uint8_t read_buf[CACHE_SIZE];
static uint8_t prog_buf[CACHE_SIZE];
static uint8_t lookahead_buf[LOOKAHEAD];

static const struct lfs_config cfg = {
    .context = NULL,
    .read  = ram_read,
    .prog  = ram_prog,
    .erase = ram_erase,
    .sync  = ram_sync,

    .read_size      = READ_SIZE,
    .prog_size      = PROG_SIZE,
    .block_size     = BLOCK_SIZE,
    .block_count    = BLOCK_COUNT,
    .block_cycles   = 500,
    .cache_size     = CACHE_SIZE,
    .lookahead_size = LOOKAHEAD,

    .read_buffer      = read_buf,
    .prog_buffer      = prog_buf,
    .lookahead_buffer = lookahead_buf,

    // Defaults.
    .name_max = 0,
    .file_max = 0,
    .attr_max = 0,
};

static void reset(void) {
    memset(image, 0xff, IMAGE_BYTES);
}

static void dump(const char *path) {
    FILE *f = fopen(path, "wb");
    if (!f) { perror(path); exit(1); }
    if (fwrite(image, 1, IMAGE_BYTES, f) != IMAGE_BYTES) { perror(path); exit(1); }
    fclose(f);
    fprintf(stderr, "wrote %s\n", path);
}

static void must(int err, const char *what) {
    if (err < 0) { fprintf(stderr, "%s: %d\n", what, err); exit(1); }
}

// ---- Scenarios ----

// 01_empty_format: format only, no entries.
static void scenario_empty_format(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    dump(out_path);
}

// 02_single_inline: one tiny file at root, content fits inline by every
// metric (16 bytes, well below the inline threshold in either kernel).
static void scenario_single_inline(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/cfg", LFS_O_WRONLY | LFS_O_CREAT), "open");
    static const char body[] = "hello, littlefs";  // 15 bytes + NUL but we write only 15
    must(lfs_file_write(&lfs, &f, body, 15), "write");
    must(lfs_file_close(&lfs, &f), "close");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 03_single_ctz: one larger file at root that lands as a CTZ skip list
// in either kernel (500 bytes well exceeds any sane inline threshold).
static void scenario_single_ctz(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/payload.bin", LFS_O_WRONLY | LFS_O_CREAT), "open");
    uint8_t body[500];
    for (size_t i = 0; i < sizeof body; i++) body[i] = (uint8_t)(i & 0xff);
    must(lfs_file_write(&lfs, &f, body, sizeof body), "write");
    must(lfs_file_close(&lfs, &f), "close");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 04_nested_dir: /audit/ directory containing one inline file.
static void scenario_nested_dir(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    must(lfs_mkdir(&lfs, "/audit"), "mkdir");
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/audit/log", LFS_O_WRONLY | LFS_O_CREAT), "open");
    static const char body[] = "entry-0001;";  // 11 bytes
    must(lfs_file_write(&lfs, &f, body, 11), "write");
    must(lfs_file_close(&lfs, &f), "close");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

int main(int argc, char **argv) {
    const char *outdir = argc > 1 ? argv[1] : ".";
    char path[512];

    snprintf(path, sizeof path, "%s/01_empty_format.bin", outdir);
    scenario_empty_format(path);

    snprintf(path, sizeof path, "%s/02_single_inline.bin", outdir);
    scenario_single_inline(path);

    snprintf(path, sizeof path, "%s/03_single_ctz.bin", outdir);
    scenario_single_ctz(path);

    snprintf(path, sizeof path, "%s/04_nested_dir.bin", outdir);
    scenario_nested_dir(path);

    return 0;
}
