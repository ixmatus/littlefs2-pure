// verify_image: load a LittleFS image produced by littlefs2-pure and
// validate its contents through the C reference. The exit status
// signals the verification result (0 = pass, non-zero = mismatch).
//
// Usage:
//   verify_image <image_path> <scenario>
//
// Scenarios:
//   inline   - expect `/cfg` with body == "hello, rust"
//   ctz      - expect `/payload.bin` with 500 bytes of i & 0xff
//   nested   - expect `/audit/log` with body == "entry-0001;"
//
// Geometry mirrors `tests/common::MemStorage`:
//   block_size  = 256
//   block_count = 8
//   read/prog   = 16
//   cache_size  = 64
//   lookahead   = 8

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../gen_vectors/littlefs/lfs.h"

#define BLOCK_SIZE   256
#define BLOCK_COUNT  8
#define PROG_SIZE    16
#define READ_SIZE    16
#define CACHE_SIZE   64
#define LOOKAHEAD    8
#define IMAGE_BYTES  (BLOCK_SIZE * BLOCK_COUNT)

static uint8_t image[IMAGE_BYTES];

static int ram_read(const struct lfs_config *c, lfs_block_t block,
                    lfs_off_t off, void *buffer, lfs_size_t size) {
    (void)c;
    if ((size_t)block * BLOCK_SIZE + off + size > IMAGE_BYTES) return LFS_ERR_IO;
    memcpy(buffer, &image[block * BLOCK_SIZE + off], size);
    return 0;
}

// verify_image is read-only; program/erase should never fire during a
// pure mount + read workload. Return LFS_ERR_IO if they do, which
// the caller treats as a verification failure.
static int ram_prog(const struct lfs_config *c, lfs_block_t b,
                    lfs_off_t off, const void *buf, lfs_size_t sz) {
    (void)c; (void)b; (void)off; (void)buf; (void)sz;
    fprintf(stderr, "verify_image: unexpected program call during mount/read\n");
    return LFS_ERR_IO;
}

static int ram_erase(const struct lfs_config *c, lfs_block_t b) {
    (void)c; (void)b;
    fprintf(stderr, "verify_image: unexpected erase call during mount/read\n");
    return LFS_ERR_IO;
}

static int ram_sync(const struct lfs_config *c) {
    (void)c;
    return 0;
}

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

    .name_max = 0,
    .file_max = 0,
    .attr_max = 0,
};

// Verify a file's content matches `expected`. Returns 0 on success.
static int verify_file(lfs_t *lfs, const char *path,
                       const uint8_t *expected, size_t exp_len) {
    lfs_file_t f;
    int err = lfs_file_open(lfs, &f, path, LFS_O_RDONLY);
    if (err < 0) {
        fprintf(stderr, "verify_image: open %s failed: %d\n", path, err);
        return 1;
    }
    uint8_t buf[1024];
    if (exp_len > sizeof buf) {
        fprintf(stderr, "verify_image: expected length %zu exceeds buffer\n", exp_len);
        return 1;
    }
    lfs_ssize_t n = lfs_file_read(lfs, &f, buf, sizeof buf);
    if (n < 0) {
        fprintf(stderr, "verify_image: read %s failed: %d\n", path, (int)n);
        return 1;
    }
    if ((size_t)n != exp_len) {
        fprintf(stderr, "verify_image: %s length mismatch: got %d expected %zu\n",
                path, (int)n, exp_len);
        return 1;
    }
    if (memcmp(buf, expected, exp_len) != 0) {
        fprintf(stderr, "verify_image: %s content mismatch\n", path);
        return 1;
    }
    lfs_file_close(lfs, &f);
    return 0;
}

static int load_image(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) { perror(path); return 1; }
    size_t n = fread(image, 1, IMAGE_BYTES, f);
    fclose(f);
    if (n != IMAGE_BYTES) {
        fprintf(stderr, "verify_image: expected %d bytes, got %zu\n", IMAGE_BYTES, n);
        return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <image_path> <scenario>\n", argv[0]);
        return 2;
    }
    if (load_image(argv[1]) != 0) return 2;

    lfs_t lfs;
    int err = lfs_mount(&lfs, &cfg);
    if (err < 0) {
        fprintf(stderr, "verify_image: mount failed: %d\n", err);
        return 1;
    }

    int rc = 0;
    if (strcmp(argv[2], "inline") == 0) {
        rc = verify_file(&lfs, "/cfg", (const uint8_t *)"hello, rust", 11);
    } else if (strcmp(argv[2], "ctz") == 0) {
        uint8_t expected[500];
        for (size_t i = 0; i < sizeof expected; i++) expected[i] = (uint8_t)(i & 0xff);
        rc = verify_file(&lfs, "/payload.bin", expected, sizeof expected);
    } else if (strcmp(argv[2], "nested") == 0) {
        rc = verify_file(&lfs, "/audit/log", (const uint8_t *)"entry-0001;", 11);
    } else {
        fprintf(stderr, "verify_image: unknown scenario %s\n", argv[2]);
        rc = 2;
    }

    lfs_unmount(&lfs);
    return rc;
}
