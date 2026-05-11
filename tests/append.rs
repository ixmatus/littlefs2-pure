//! Integration tests for `Fs::append_to_path`.
//!
//! Covers the SMIL audit logger's "append to /audit/log.bin" workflow
//! at various sizes (inline-only, inline-then-CTZ, CTZ-only).

use littlefs2_pure::ctz::CtzStruct;
use littlefs2_pure::tag::TagType;
use littlefs2_pure::{Error, Fs, Path};

mod common;
use common::MemStorage;

fn make_fs() -> Fs<MemStorage> {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap()
}

fn read_content(fs: &mut Fs<MemStorage>, path: &str) -> Vec<u8> {
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new(path).unwrap(), &mut buf_a, &mut buf_b).unwrap();
    match r.struct_type {
        TagType::InlineStruct => r.struct_body.to_vec(),
        TagType::CtzStruct => {
            let ctz = CtzStruct::from_bytes(r.struct_body).unwrap();
            let mut out = vec![0u8; ctz.size as usize];
            let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
            fs.read_ctz(&ctz, &mut out, &mut scratch).unwrap();
            out
        }
        _ => panic!("unexpected struct_type"),
    }
}

#[test]
fn append_creates_file_if_missing() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.append_to_path(
        Path::new("/log").unwrap(),
        b"first entry",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    assert_eq!(read_content(&mut fs, "/log"), b"first entry");
}

#[test]
fn append_inline_grows_inline() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    fs.append_to_path(Path::new("/log").unwrap(), b"AAA", &mut content_scratch, &mut a, &mut b)
        .unwrap();
    fs.append_to_path(Path::new("/log").unwrap(), b"BBB", &mut content_scratch, &mut a, &mut b)
        .unwrap();
    fs.append_to_path(Path::new("/log").unwrap(), b"CCC", &mut content_scratch, &mut a, &mut b)
        .unwrap();

    assert_eq!(read_content(&mut fs, "/log"), b"AAABBBCCC");
}

#[test]
fn append_promotes_inline_to_ctz() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    fs.append_to_path(Path::new("/log").unwrap(), b"head_", &mut content_scratch, &mut a, &mut b)
        .unwrap();

    // Add 200 bytes; total ~205 > INLINE_MAX (128) so the file
    // promotes to CTZ on this append.
    let chunk: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &chunk, &mut content_scratch, &mut a, &mut b)
        .unwrap();

    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let r = fs.resolve(Path::new("/log").unwrap(), &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(r.struct_type, TagType::CtzStruct);

    let mut expected = Vec::new();
    expected.extend_from_slice(b"head_");
    expected.extend_from_slice(&chunk);
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn append_ctz_to_ctz_extends_content() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];

    let v1: Vec<u8> = (0..300).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &v1, &mut content_scratch, &mut a, &mut b)
        .unwrap();
    let v2: Vec<u8> = (300..500).map(|i| (i & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &v2, &mut content_scratch, &mut a, &mut b)
        .unwrap();

    let mut expected = v1.clone();
    expected.extend_from_slice(&v2);
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn append_rejects_directory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.mkdir(Path::new("/d").unwrap(), &mut a, &mut b).unwrap();

    let err = fs
        .append_to_path(Path::new("/d").unwrap(), b"x", &mut content_scratch, &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::AlreadyExists);
}

#[test]
fn append_rejects_undersized_content_scratch_for_inline_path() {
    // The streaming-CTZ path bypasses content_scratch entirely; only
    // the inline-grow path needs the buffer big enough to hold
    // (existing_inline + additional). Establish a small inline file
    // first, then trip OutOfRange on the inline-grow append.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    fs.write_to_path(Path::new("/log").unwrap(), b"AAAA", &mut a, &mut b).unwrap();
    let mut tiny = [0u8; 4];
    let err = fs
        .append_to_path(Path::new("/log").unwrap(), b"hello world", &mut tiny, &mut a, &mut b)
        .unwrap_err();
    assert_eq!(err, Error::OutOfRange);
}

#[test]
fn append_to_ctz_does_not_consult_content_scratch() {
    // Once the file is in CTZ form, append_to_path is streaming and
    // must not touch content_scratch. Pass an empty slice to confirm.
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let initial: Vec<u8> = (0..300).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/log").unwrap(), &initial, &mut a, &mut b).unwrap();

    let mut empty: [u8; 0] = [];
    fs.append_to_path(Path::new("/log").unwrap(), b"streaming!", &mut empty, &mut a, &mut b)
        .unwrap();

    let mut expected = initial.clone();
    expected.extend_from_slice(b"streaming!");
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn ctz_streaming_append_preserves_existing_blocks() {
    // Verify the streaming invariant: existing chain blocks are not
    // erased or rewritten across an append; only the tail's free
    // region is programmed (and only if it has room).
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // Write a 4-block CTZ file (well past inline, several blocks).
    let initial: Vec<u8> = (0..800).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/log").unwrap(), &initial, &mut a, &mut b).unwrap();

    // Snapshot the storage and collect the chain's physical addresses.
    let head = {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let r = fs.resolve(Path::new("/log").unwrap(), &mut buf_a, &mut buf_b).unwrap();
        assert_eq!(r.struct_type, TagType::CtzStruct);
        CtzStruct::from_bytes(r.struct_body).unwrap()
    };
    let before_blocks = collect_chain(&mut fs, head);
    let before_bytes: Vec<Vec<u8>> =
        before_blocks.iter().map(|&blk| read_block(&fs, blk)).collect();

    // Append a small chunk that fits in the tail's free space (block 3
    // at 252 bytes content can hold the tail of 800 bytes minus the
    // preceding ~750, leaving ~50 bytes of free room; appending 32
    // bytes should fit cleanly).
    fs.append_to_path(
        Path::new("/log").unwrap(),
        b"01234567890123456789012345678901",
        &mut [],
        &mut a,
        &mut b,
    )
    .unwrap();

    // Read back the new head; existing blocks (except the tail, which
    // had bytes programmed into its erased region) must still match
    // byte-for-byte.
    let head_after = {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let r = fs.resolve(Path::new("/log").unwrap(), &mut buf_a, &mut buf_b).unwrap();
        CtzStruct::from_bytes(r.struct_body).unwrap()
    };
    let after_blocks = collect_chain(&mut fs, head_after);

    // All addresses up to the old tail must be preserved.
    assert_eq!(&before_blocks[..], &after_blocks[..before_blocks.len()]);
    // Non-tail blocks must be byte-identical (no re-erase, no realloc).
    for (i, &blk) in before_blocks.iter().enumerate().take(before_blocks.len() - 1) {
        let now = read_block(&fs, blk);
        assert_eq!(now, before_bytes[i], "block {i} (phys {blk}) was rewritten");
    }

    let mut expected = initial.clone();
    expected.extend_from_slice(b"01234567890123456789012345678901");
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn ctz_streaming_append_grows_chain_across_blocks() {
    // Append enough bytes to require allocating new blocks. Sized to
    // fit MemStorage's 8 block / 2 KiB capacity (root pair + chain).
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let initial: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
    fs.write_to_path(Path::new("/log").unwrap(), &initial, &mut a, &mut b).unwrap();

    // Append 600 bytes - spans two new chain blocks at 256 byte
    // BLOCK_SIZE.
    let extension: Vec<u8> = (0..600).map(|i| ((i + 100) & 0xff) as u8).collect();
    fs.append_to_path(Path::new("/log").unwrap(), &extension, &mut [], &mut a, &mut b).unwrap();

    let mut expected = initial.clone();
    expected.extend_from_slice(&extension);
    assert_eq!(read_content(&mut fs, "/log"), expected);
}

#[test]
fn ctz_streaming_many_small_appends_match_one_large() {
    // The audit-logger workload: many small appends. Resulting bytes
    // must match a single large write.
    let mut fs_streaming = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    // Seed with enough to be CTZ.
    let seed: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
    fs_streaming.write_to_path(Path::new("/log").unwrap(), &seed, &mut a, &mut b).unwrap();
    let mut entries: Vec<u8> = seed.clone();
    for i in 0..40u32 {
        let entry = format!("entry-{i:04};");
        fs_streaming
            .append_to_path(Path::new("/log").unwrap(), entry.as_bytes(), &mut [], &mut a, &mut b)
            .unwrap();
        entries.extend_from_slice(entry.as_bytes());
    }
    assert_eq!(read_content(&mut fs_streaming, "/log"), entries);
}

fn collect_chain(fs: &mut Fs<MemStorage>, ctz: CtzStruct) -> Vec<u32> {
    use littlefs2_pure::ctz::{block_count, collect_chain_blocks};
    use littlefs2_pure::BlockAddress;
    let bs = MemStorage::BLOCK_SIZE as u32;
    let n = block_count(ctz.size, bs);
    let mut out = vec![BlockAddress::NONE; n as usize];
    collect_chain_blocks(fs.storage_mut(), ctz.head_block, n, &mut out).unwrap();
    out.iter().map(|a| a.as_u32()).collect()
}

fn read_block(fs: &Fs<MemStorage>, block: u32) -> Vec<u8> {
    let start = (block as usize) * MemStorage::BLOCK_SIZE;
    fs.storage().data[start..start + MemStorage::BLOCK_SIZE].to_vec()
}

#[test]
fn append_into_subdirectory() {
    let mut fs = make_fs();
    let mut a = [0u8; MemStorage::BLOCK_SIZE];
    let mut b = [0u8; MemStorage::BLOCK_SIZE];
    let mut content_scratch = [0u8; 1024];
    fs.mkdir(Path::new("/audit").unwrap(), &mut a, &mut b).unwrap();
    fs.append_to_path(
        Path::new("/audit/log").unwrap(),
        b"entry0\n",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    fs.append_to_path(
        Path::new("/audit/log").unwrap(),
        b"entry1\n",
        &mut content_scratch,
        &mut a,
        &mut b,
    )
    .unwrap();
    assert_eq!(read_content(&mut fs, "/audit/log"), b"entry0\nentry1\n");
}

#[test]
fn appends_survive_remount() {
    let mut storage = MemStorage::new();
    let mut scratch = [0u8; MemStorage::BLOCK_SIZE];
    Fs::format(&mut storage, &mut scratch).unwrap();
    {
        let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
        let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
        let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
        let mut a = [0u8; MemStorage::BLOCK_SIZE];
        let mut b = [0u8; MemStorage::BLOCK_SIZE];
        let mut cs = [0u8; 1024];
        for i in 0..5u32 {
            let entry = format!("e{i};");
            fs.append_to_path(
                Path::new("/log").unwrap(),
                entry.as_bytes(),
                &mut cs,
                &mut a,
                &mut b,
            )
            .unwrap();
        }
        storage = fs.into_storage();
    }
    // Remount, read back.
    let mut buf_a = [0u8; MemStorage::BLOCK_SIZE];
    let mut buf_b = [0u8; MemStorage::BLOCK_SIZE];
    let mut fs = Fs::mount(storage, &mut buf_a, &mut buf_b).unwrap();
    assert_eq!(read_content(&mut fs, "/log"), b"e0;e1;e2;e3;e4;");
}
