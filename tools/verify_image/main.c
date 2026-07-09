// verify_image: load a LittleFS image produced by littlefs2-pure and
// validate its contents through the C reference. The exit status
// signals the verification result (0 = pass, non-zero = mismatch).
//
// Usage:
//   verify_image <image_path> <scenario>
//
// Scenarios:
//   inline    - expect `/cfg` with body == "hello, rust"
//   ctz       - expect `/payload.bin` with 500 bytes of i & 0xff
//   nested    - expect `/audit/log` with body == "entry-0001;"
//   split_dir - expect `/d/f00`..`/d/f13`, each body == "x" (a directory
//               this crate split across a HardTail continuation; the C
//               reference must chase the chain to find every entry)
//   split_root- expect `/f00`..`/f11`, each body == "v" (the ROOT pair
//               {0,1} split across a HardTail continuation; {0,1} stays the
//               superblock anchor and the C reference chases its tail)
//   mutate    - MOUNT a Rust image, WRITE into it (a small inline file and
//               a CTZ-backed file), then dump the mutated image to a third
//               argument path for Rust to remount and verify. Proves the
//               FCRC / erased-window handshake in the C-writes-into-Rust
//               direction (review M11); usage takes a third path argument.
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

// Read scenarios are pure mount + read and must never mutate the image;
// the `mutate` scenario sets this so C can write into a Rust-formatted
// image (review M11). Guarding on it keeps the read scenarios' "no writes
// happened" assertion while letting the write direction through.
static int g_writable = 0;

static int ram_read(const struct lfs_config *c, lfs_block_t block,
                    lfs_off_t off, void *buffer, lfs_size_t size) {
    (void)c;
    if ((size_t)block * BLOCK_SIZE + off + size > IMAGE_BYTES) return LFS_ERR_IO;
    memcpy(buffer, &image[block * BLOCK_SIZE + off], size);
    return 0;
}

// In read scenarios, program/erase should never fire during a pure mount +
// read workload; return LFS_ERR_IO if they do, which the caller treats as
// a verification failure. In the `mutate` scenario, honor them with NOR
// semantics (only 1 -> 0 transitions) exactly as gen_vectors does, so C's
// commit sequencing into the Rust image is exercised honestly.
static int ram_prog(const struct lfs_config *c, lfs_block_t block,
                    lfs_off_t off, const void *buffer, lfs_size_t size) {
    (void)c;
    if (!g_writable) {
        fprintf(stderr, "verify_image: unexpected program call during mount/read\n");
        return LFS_ERR_IO;
    }
    if ((size_t)block * BLOCK_SIZE + off + size > IMAGE_BYTES) return LFS_ERR_IO;
    const uint8_t *src = buffer;
    uint8_t *dst = &image[block * BLOCK_SIZE + off];
    for (size_t i = 0; i < size; i++) {
        if ((dst[i] & src[i]) != src[i]) {
            fprintf(stderr, "verify_image: 0->1 bit flip at block %u off %u byte %zu\n",
                    block, off, i);
            return LFS_ERR_IO;
        }
        dst[i] = src[i];
    }
    return 0;
}

static int ram_erase(const struct lfs_config *c, lfs_block_t b) {
    (void)c;
    if (!g_writable) {
        fprintf(stderr, "verify_image: unexpected erase call during mount/read\n");
        return LFS_ERR_IO;
    }
    if (b >= BLOCK_COUNT) return LFS_ERR_IO;
    memset(&image[b * BLOCK_SIZE], 0xff, BLOCK_SIZE);
    return 0;
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

static int dump_image(const char *path) {
    FILE *f = fopen(path, "wb");
    if (!f) { perror(path); return 1; }
    size_t n = fwrite(image, 1, IMAGE_BYTES, f);
    fclose(f);
    if (n != IMAGE_BYTES) {
        fprintf(stderr, "verify_image: wrote %zu of %d bytes\n", n, IMAGE_BYTES);
        return 1;
    }
    return 0;
}

// The `mutate` scenario: C writes two files into a Rust-formatted image.
// The inline file appends a commit into the root pair's erased region that
// Rust left an FCRC over; the CTZ file forces C to allocate data blocks in
// a Rust image and thread them into a new commit. Rust remounts the result
// and verifies both, closing the read-only gap in the roundtrip gate
// (review M11). Returns 0 on success.
static int mutate_image(lfs_t *lfs) {
    lfs_file_t f;
    int err = lfs_file_open(lfs, &f, "/c_small", LFS_O_WRONLY | LFS_O_CREAT);
    if (err < 0) { fprintf(stderr, "verify_image: mutate open c_small: %d\n", err); return 1; }
    if (lfs_file_write(lfs, &f, "hi", 2) < 0) { fprintf(stderr, "verify_image: mutate write c_small\n"); return 1; }
    if (lfs_file_close(lfs, &f) < 0) { fprintf(stderr, "verify_image: mutate close c_small\n"); return 1; }

    err = lfs_file_open(lfs, &f, "/c_big.bin", LFS_O_WRONLY | LFS_O_CREAT);
    if (err < 0) { fprintf(stderr, "verify_image: mutate open c_big: %d\n", err); return 1; }
    uint8_t body[400];
    for (size_t i = 0; i < sizeof body; i++) body[i] = (uint8_t)(i & 0xff);
    if (lfs_file_write(lfs, &f, body, sizeof body) < 0) { fprintf(stderr, "verify_image: mutate write c_big\n"); return 1; }
    if (lfs_file_close(lfs, &f) < 0) { fprintf(stderr, "verify_image: mutate close c_big\n"); return 1; }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3 && argc != 4) {
        fprintf(stderr, "usage: %s <image_path> <scenario> [<out_path>]\n", argv[0]);
        return 2;
    }
    const char *scenario = argv[2];
    int is_mutate = strcmp(scenario, "mutate") == 0;
    if (is_mutate && argc != 4) {
        fprintf(stderr, "usage: %s <image_path> mutate <out_path>\n", argv[0]);
        return 2;
    }
    if (load_image(argv[1]) != 0) return 2;
    if (is_mutate) g_writable = 1;

    lfs_t lfs;
    int err = lfs_mount(&lfs, &cfg);
    if (err < 0) {
        fprintf(stderr, "verify_image: mount failed: %d\n", err);
        return 1;
    }

    int rc = 0;
    if (is_mutate) {
        rc = mutate_image(&lfs);
    } else if (strcmp(argv[2], "inline") == 0) {
        rc = verify_file(&lfs, "/cfg", (const uint8_t *)"hello, rust", 11);
    } else if (strcmp(argv[2], "ctz") == 0) {
        uint8_t expected[500];
        for (size_t i = 0; i < sizeof expected; i++) expected[i] = (uint8_t)(i & 0xff);
        rc = verify_file(&lfs, "/payload.bin", expected, sizeof expected);
    } else if (strcmp(argv[2], "nested") == 0) {
        rc = verify_file(&lfs, "/audit/log", (const uint8_t *)"entry-0001;", 11);
    } else if (strcmp(argv[2], "split_dir") == 0) {
        // Every entry must resolve, including those the writer placed in a
        // HardTail continuation pair. lfs_file_open chases the chain via
        // lfs_dir_find, so a missing entry means the chain link is wrong.
        rc = 0;
        for (int i = 0; i < 14 && rc == 0; i++) {
            char path[16];
            snprintf(path, sizeof path, "/d/f%02d", i);
            rc = verify_file(&lfs, path, (const uint8_t *)"x", 1);
        }
    } else if (strcmp(argv[2], "remove") == 0) {
        // Delete-tag wire compatibility (review C3): the image holds
        // /aa = "keep-me" and a REMOVED /bb. Before the fix the
        // crate's writer emitted entry deletes with the reserved
        // length sentinel 0x3FF where the C reference writes size 0;
        // lfs_dir_fetchmatch's exact-compare besttag invalidation
        // (lfs.c:1244) then never matches the delete, so the C
        // reference resolves /bb to its NEIGHBOR /aa, serves its
        // content, and an lfs_remove("/bb") would destroy /aa. The
        // scenario therefore requires BOTH: /aa intact AND /bb
        // genuinely absent under the C reference's view.
        rc = verify_file(&lfs, "/aa", (const uint8_t *)"keep-me", 7);
        if (rc == 0) {
            struct lfs_info info;
            int st = lfs_stat(&lfs, "/bb", &info);
            if (st != LFS_ERR_NOENT) {
                fprintf(stderr,
                        "verify_image: /bb should be deleted but lfs_stat "
                        "returned %d (the delete tag is invisible to the C "
                        "reference)\n", st);
                rc = 1;
            }
        }
    } else if (strcmp(argv[2], "split_root") == 0) {
        // The root pair {0,1} split; every entry must resolve, including
        // those the writer placed in the root's HardTail continuation.
        rc = 0;
        for (int i = 0; i < 12 && rc == 0; i++) {
            char path[16];
            snprintf(path, sizeof path, "/f%02d", i);
            rc = verify_file(&lfs, path, (const uint8_t *)"v", 1);
        }
    } else {
        fprintf(stderr, "verify_image: unknown scenario %s\n", argv[2]);
        rc = 2;
    }

    lfs_unmount(&lfs);

    // In mutate mode, persist the image C just wrote so Rust can remount
    // it. Unmount above flushed all metadata (files were closed inside
    // mutate_image), so `image` now holds the complete mutated device.
    if (is_mutate && rc == 0) {
        rc = dump_image(argv[3]);
    }
    return rc;
}
