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

// Create `path`, write `n` bytes of `body` (nothing when n == 0), close.
// Used by the scenarios added for the 2026-06 review coverage classes.
static void mkfile(lfs_t *lfs, const char *path, const void *body, size_t n) {
    lfs_file_t f;
    must(lfs_file_open(lfs, &f, path, LFS_O_WRONLY | LFS_O_CREAT), "open");
    if (n) must(lfs_file_write(lfs, &f, body, n), "write");
    must(lfs_file_close(lfs, &f), "close");
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

// 05_hardtail_dir: a directory dense enough that the C reference splits
// its metadata across more than one pair, linking them with a HardTail.
// At this geometry (256 byte blocks, 8 blocks) the device is tiny, so we
// add single byte named, zero length files until the filesystem reports
// no space, then stop. The densest directory the geometry admits is the
// one most likely to have forced a continuation pair; the Rust side
// asserts every created name is listed, which only holds if the reader
// walks the whole HardTail chain.
static void scenario_hardtail_dir(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    must(lfs_mkdir(&lfs, "/d"), "mkdir");
    // Names "/d/a" .. "/d/z"; stop at the first allocation failure
    // instead of aborting so the image is the densest one that fits.
    char path[8];
    for (char c = 'a'; c <= 'z'; c++) {
        snprintf(path, sizeof path, "/d/%c", c);
        lfs_file_t f;
        int err = lfs_file_open(&lfs, &f, path, LFS_O_WRONLY | LFS_O_CREAT);
        if (err < 0) break;  // out of space: the directory is now full
        if (lfs_file_close(&lfs, &f) < 0) break;
    }
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 06_inline_ctz_boundary: two files straddling the inline/CTZ struct
// boundary, /b128 (128 bytes) and /b129 (129 bytes). Which one the C
// reference stores inline versus as a CTZ skip list is geometry
// dependent; the point of the vector is that our reader classifies each
// exactly as the C writer did, so the Rust test reads the struct type
// back rather than assuming it.
static void scenario_inline_ctz_boundary(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    uint8_t body[129];
    for (size_t i = 0; i < sizeof body; i++) body[i] = (uint8_t)(i & 0xff);
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/b128", LFS_O_WRONLY | LFS_O_CREAT), "open b128");
    must(lfs_file_write(&lfs, &f, body, 128), "write b128");
    must(lfs_file_close(&lfs, &f), "close b128");
    must(lfs_file_open(&lfs, &f, "/b129", LFS_O_WRONLY | LFS_O_CREAT), "open b129");
    must(lfs_file_write(&lfs, &f, body, 129), "write b129");
    must(lfs_file_close(&lfs, &f), "close b129");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 07_deleted_recreated: create /x, delete it, recreate /x with a
// different body. Exercises the reader against a metadata pair that
// carries a delete tombstone followed by a fresh create for the same
// name; a naive reader that stops at the first matching name or ignores
// the delete would resolve the stale content.
static void scenario_deleted_recreated(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/x", LFS_O_WRONLY | LFS_O_CREAT), "open first");
    static const char first[] = "stale-v1";  // 8 bytes
    must(lfs_file_write(&lfs, &f, first, 8), "write first");
    must(lfs_file_close(&lfs, &f), "close first");
    must(lfs_remove(&lfs, "/x"), "remove");
    must(lfs_file_open(&lfs, &f, "/x", LFS_O_WRONLY | LFS_O_CREAT), "open second");
    static const char second[] = "fresh-v2!!";  // 10 bytes
    must(lfs_file_write(&lfs, &f, second, 10), "write second");
    must(lfs_file_close(&lfs, &f), "close second");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 08_user_attrs: three entries each carrying user attributes, with the
// middle one (/bb) removed so the surviving entries' live ids shift under
// the delete splice. A reader that reads attributes off the raw committed
// id rather than splice-correcting to the live id loses /cc's attribute or
// leaks one across entries (review C1/C2). The Rust side reads every
// attribute back through get_attr and confirms /bb is absent. Attr ids are
// the ASCII bytes 't' and 'u'.
static void scenario_user_attrs(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    mkfile(&lfs, "/aa", "keep-me", 7);
    must(lfs_setattr(&lfs, "/aa", 't', "meta", 4), "setattr aa t");
    must(lfs_setattr(&lfs, "/aa", 'u', "data99", 6), "setattr aa u");
    mkfile(&lfs, "/bb", "second", 6);
    must(lfs_setattr(&lfs, "/bb", 't', "bmeta", 5), "setattr bb t");
    mkfile(&lfs, "/cc", "third", 5);
    must(lfs_setattr(&lfs, "/cc", 't', "cmeta", 5), "setattr cc t");
    must(lfs_remove(&lfs, "/bb"), "remove");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 09_deep_ctz: a single file large enough to span several CTZ data blocks
// (900 bytes lands as 4 blocks at this geometry), so reading it back
// exercises the skip-list back-pointer traversal past the one or two
// blocks the 03/06 vectors reach. Content is i & 0xff.
static void scenario_deep_ctz(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    uint8_t body[900];
    for (size_t i = 0; i < sizeof body; i++) body[i] = (uint8_t)(i & 0xff);
    lfs_file_t f;
    must(lfs_file_open(&lfs, &f, "/big.bin", LFS_O_WRONLY | LFS_O_CREAT), "open");
    must(lfs_file_write(&lfs, &f, body, sizeof body), "write");
    must(lfs_file_close(&lfs, &f), "close");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 10_delete_tombstone: /aa kept, /bb created then removed with no
// recreate, leaving a bare delete tombstone beside a live neighbor. This
// is the C-writes/Rust-reads companion to the roundtrip `remove` scenario
// (review C3): a reader that mishandles the size-0 delete tag resolves /bb
// to its neighbor /aa. Distinct from 07, which recreates the deleted name.
static void scenario_delete_tombstone(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    mkfile(&lfs, "/aa", "keep-me", 7);
    mkfile(&lfs, "/bb", "doomed", 6);
    must(lfs_remove(&lfs, "/bb"), "remove");
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 11_compacted_rename: three entries, one renamed, then the pair forced
// through a compaction. C's lfs_dir_compact re-emits the surviving latest
// tags in log order, so the renamed entry's NAME (rewritten last) lands
// after higher-id NAMEs: the compacted block carries NAME tags whose ids
// are not monotonic in log order (here 1, 2, 4, 3). A reader that requires
// id-dense NAME order rejects this valid C image (review H1). The churned
// /t entry is only the compaction trigger; it is removed before unmount.
static void scenario_compacted_rename(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    mkfile(&lfs, "/aaa", "AAAA", 4);
    mkfile(&lfs, "/bbb", "BBBB", 4);
    mkfile(&lfs, "/ccc", "CCCC", 4);
    must(lfs_rename(&lfs, "/aaa", "/zzz"), "rename");
    // Churn a filler entry until the log overflows and a commit triggers
    // lfs_dir_compact; twelve rounds clear the 256-byte block several times.
    for (int k = 0; k < 12; k++) {
        mkfile(&lfs, "/t", "z", 1);
        must(lfs_remove(&lfs, "/t"), "remove t");
    }
    must(lfs_unmount(&lfs), "unmount");
    dump(out_path);
}

// 12_multimove_gstate: two cross-directory renames into the same
// destination /dst with no intervening compaction, so /dst's log holds two
// MOVESTATE (gstate) tags. C reads a pair's gstate contribution as the
// single latest matching tag; a reader that XOR-accumulates every MOVESTATE
// tag in the log decodes a phantom pending move and deletes a live entry at
// mount (review C4). The Rust side mounts and confirms both moved files
// resolve under /dst and /src is empty.
static void scenario_multimove_gstate(const char *out_path) {
    reset();
    lfs_t lfs;
    must(lfs_format(&lfs, &cfg), "format");
    must(lfs_mount(&lfs, &cfg), "mount");
    must(lfs_mkdir(&lfs, "/src"), "mkdir src");
    must(lfs_mkdir(&lfs, "/dst"), "mkdir dst");
    mkfile(&lfs, "/src/a", "AA", 2);
    mkfile(&lfs, "/src/b", "BB", 2);
    must(lfs_rename(&lfs, "/src/a", "/dst/a"), "rename a");
    must(lfs_rename(&lfs, "/src/b", "/dst/b"), "rename b");
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

    snprintf(path, sizeof path, "%s/05_hardtail_dir.bin", outdir);
    scenario_hardtail_dir(path);

    snprintf(path, sizeof path, "%s/06_inline_ctz_boundary.bin", outdir);
    scenario_inline_ctz_boundary(path);

    snprintf(path, sizeof path, "%s/07_deleted_recreated.bin", outdir);
    scenario_deleted_recreated(path);

    snprintf(path, sizeof path, "%s/08_user_attrs.bin", outdir);
    scenario_user_attrs(path);

    snprintf(path, sizeof path, "%s/09_deep_ctz.bin", outdir);
    scenario_deep_ctz(path);

    snprintf(path, sizeof path, "%s/10_delete_tombstone.bin", outdir);
    scenario_delete_tombstone(path);

    snprintf(path, sizeof path, "%s/11_compacted_rename.bin", outdir);
    scenario_compacted_rename(path);

    snprintf(path, sizeof path, "%s/12_multimove_gstate.bin", outdir);
    scenario_multimove_gstate(path);

    return 0;
}
