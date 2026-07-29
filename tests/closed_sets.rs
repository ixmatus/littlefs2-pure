//! Closed set guards for the registries this crate mirrors from LittleFS v2.
//!
//! Three sets in this crate are closed and hand maintained: the tag type
//! space in `src/tag.rs`, the error set in `src/error.rs`, and the format
//! constants in `src/lib.rs`. Hand maintained means each one can drift from
//! the format it claims to mirror with nothing failing. These guards remove
//! that possibility for the parts that are mechanically checkable, and say
//! plainly which parts are not.
//!
//! # The oracle
//!
//! The authoritative numeric values live in the vendored C reference header
//! at `tools/gen_vectors/littlefs/lfs.h`, pinned at revision d01280e (tag
//! v2.9.3) and registered at `docs/references/c-littlefs-oracle.md`. These
//! guards parse that header at test time rather than restating its numbers,
//! so bumping the oracle runs every comparison again instead of leaving a stale
//! copy behind. The specification the header implements is registered at
//! `docs/references/spec-littlefs-v2.md` and vendored beside it. Reading
//! constants out of the oracle is not the same as copying its code: only
//! numeric values and their names cross the boundary, which is exactly the
//! "oracle for behavior, never a template" doctrine of ADR-0002 and ADR-0004.
//!
//! # The shape of a guard here
//!
//! Every guard pins per bucket members and counts, never an aggregate floor.
//! A floor admits silent compensating regressions: one bucket gains a member,
//! another loses one, the total holds, the drift hides. So each assertion
//! names the bucket it is about.
//!
//! Coverage runs in both directions, which is the property that makes these
//! guards worth their maintenance cost:
//!
//! - *Forward*, oracle to crate: every constant parsed out of `lfs.h` must
//!   appear in a classification table below, so a future oracle revision that
//!   adds a type or an error code fails this suite until someone decides what
//!   the crate does about it.
//! - *Reverse*, crate to oracle: the tag decoder is enumerated over its whole
//!   domain and the resulting census is pinned bucket by bucket, so a variant
//!   added to `TagType` changes a count and fails. A second scan reads the
//!   variant identifiers straight out of `src/tag.rs` and `src/error.rs`, which
//!   catches the other half of the same mistake: a variant declared but never
//!   wired into the decoder, which the census alone cannot see.
//!
//! # Why the source scan exists at all
//!
//! `TagType`, `Error`, `AbstractType`, and `EntryKind` are all
//! `#[non_exhaustive]`. That attribute is deliberate and is itself pinned
//! below, but it means an integration test, which is a separate crate, cannot
//! match on these enums exhaustively and cannot make the compiler enforce
//! completeness. Reading the variant identifiers out of the source text is the
//! available substitute. It is a text scan and it is honest about being one:
//! it sees declarations, not semantics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use littlefs2_pure::{
    AbstractType, Error, TagType, DISK_VERSION, MAGIC, NAME_MAX, ROOT_BLOCK_PAIR,
};

// ---------------------------------------------------------------------------
// Reading the oracle
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn oracle_header() -> String {
    read_repo_file("tools/gen_vectors/littlefs/lfs.h")
}

fn oracle_source() -> String {
    read_repo_file("tools/gen_vectors/littlefs/lfs.c")
}

/// Strip a trailing `// ...` comment from a C source line.
fn strip_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Parse a C integer literal: decimal (possibly negative) or `0x` hex.
fn parse_c_int(literal: &str) -> i64 {
    let literal = literal.trim();
    match literal.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16)
            .unwrap_or_else(|e| panic!("`{literal}` is not a hex literal: {e}")),
        None => literal.parse().unwrap_or_else(|e| panic!("`{literal}` is not an integer: {e}")),
    }
}

/// Parse `enum <name> { NAME = <literal>, ... };` into name to value pairs.
///
/// Only lines carrying an `=` and an `LFS_` prefixed name are collected, so
/// the comment banners inside the C enums are ignored.
fn parse_c_enum(src: &str, enum_name: &str) -> BTreeMap<String, i64> {
    let head = format!("enum {enum_name} {{");
    let start = src
        .find(&head)
        .unwrap_or_else(|| panic!("`enum {enum_name}` is missing from the vendored oracle header"));
    let body = &src[start + head.len()..];
    let end = body
        .find("};")
        .unwrap_or_else(|| panic!("`enum {enum_name}` in the oracle header never closes"));

    let mut out = BTreeMap::new();
    for line in body[..end].lines() {
        let line = strip_comment(line).trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !name.starts_with("LFS_") {
            continue;
        }
        let value = parse_c_int(value.trim().trim_end_matches(','));
        assert!(
            out.insert(name.to_string(), value).is_none(),
            "`{name}` is defined twice in `enum {enum_name}`"
        );
    }
    assert!(!out.is_empty(), "`enum {enum_name}` parsed to nothing; the parser has rotted");
    out
}

/// Parse `#define <name> <literal>` out of the oracle header.
///
/// The trailing space in the needle keeps `LFS_DISK_VERSION` from matching
/// `LFS_DISK_VERSION_MAJOR`.
fn parse_c_define(src: &str, name: &str) -> i64 {
    let needle = format!("#define {name} ");
    let line = src
        .lines()
        .find(|l| l.starts_with(&needle))
        .unwrap_or_else(|| panic!("`#define {name}` is missing from the vendored oracle header"));
    parse_c_int(strip_comment(&line[needle.len()..]))
}

// ---------------------------------------------------------------------------
// Reading this crate's own declarations
// ---------------------------------------------------------------------------

/// The variant identifiers declared inside `pub enum <name> { ... }`.
///
/// A text scan, for the reason given in the module documentation: these enums
/// are `#[non_exhaustive]`, so an integration test cannot ask the compiler for
/// the same list. The scan takes the leading identifier of every line in the
/// enum body that starts with an uppercase letter, which skips doc comments,
/// banner comments, and the named fields of struct variants.
fn declared_variants(src: &str, decl: &str) -> BTreeSet<String> {
    let start = src.find(decl).unwrap_or_else(|| panic!("`{decl}` not found"));
    let body = &src[start + decl.len()..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("`{decl}` does not close with a brace at column zero"));

    let mut out = BTreeSet::new();
    for line in body[..end].lines() {
        let line = line.trim();
        let ident: String =
            line.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if ident.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            out.insert(ident);
        }
    }
    assert!(!out.is_empty(), "`{decl}` parsed to no variants; the parser has rotted");
    out
}

/// The run of `#[...]` attribute lines immediately above a declaration.
fn attributes_above(src: &str, decl: &str) -> Vec<String> {
    let start = src.find(decl).unwrap_or_else(|| panic!("`{decl}` not found"));
    src[..start]
        .lines()
        .rev()
        .take_while(|l| l.trim_start().starts_with("#["))
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// Guard 0: the parsers themselves
// ---------------------------------------------------------------------------
//
// Every guard below reads a set out of source text. That makes parser rot the
// dominant failure mode of this whole file: a parser that quietly returns the
// wrong thing turns each downstream assertion vacuous, and a vacuous guard is
// worse than none because it reads as coverage. These tests run the parsers
// against fixtures whose answers are written out by hand, so a parser that
// stops working fails here rather than going quiet everywhere.

#[test]
fn c_enum_parser_reads_a_fixture_exactly() {
    let fixture = "\
// leading noise
enum lfs_fixture {
    // a banner comment, no equals sign
    LFS_A = 0x001,
    LFS_B = -84,  // trailing comment
    LFS_C = 0x7ff,
};
enum lfs_other {
    LFS_D = 1,
};
";
    let parsed = parse_c_enum(fixture, "lfs_fixture");
    assert_eq!(parsed.len(), 3, "the banner comment must not be read as a member");
    assert_eq!(parsed.get("LFS_A"), Some(&0x001));
    assert_eq!(parsed.get("LFS_B"), Some(&-84), "negative decimals must parse");
    assert_eq!(parsed.get("LFS_C"), Some(&0x7ff));
    assert_eq!(parsed.get("LFS_D"), None, "the parser must stop at its own enum's brace");
}

#[test]
fn c_define_parser_does_not_match_a_longer_name() {
    let fixture = "\
#define LFS_DISK_VERSION 0x00020001
#define LFS_DISK_VERSION_MAJOR (0xffff & (LFS_DISK_VERSION >> 16))
#define LFS_NAME_MAX 255  // trailing comment
";
    assert_eq!(parse_c_define(fixture, "LFS_DISK_VERSION"), 0x0002_0001);
    assert_eq!(parse_c_define(fixture, "LFS_NAME_MAX"), 255);
}

#[test]
fn variant_scanner_reads_a_fixture_exactly() {
    let fixture = "\
/// Doc comment above.
#[derive(Debug)]
#[non_exhaustive]
pub enum Fixture {
    // ---- a banner ----
    /// Doc comment on a unit variant.
    Alpha,
    Beta(u8),
    Gamma = 0x3,
    Delta {
        /// A named field, lowercase, must be skipped.
        inner_field: u8,
        chunk: u8,
    },
}

pub enum NotThisOne {
    Epsilon,
}
";
    let variants = declared_variants(fixture, "pub enum Fixture {");
    let expected: BTreeSet<String> =
        ["Alpha", "Beta", "Gamma", "Delta"].iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        variants, expected,
        "the scanner must read unit, tuple, discriminant, and struct variants, and must skip \
         doc comments, banners, named fields, and any later enum"
    );

    let attributes = attributes_above(fixture, "pub enum Fixture {");
    assert!(attributes.iter().any(|a| a.trim() == "#[non_exhaustive]"));
    assert!(attributes.iter().any(|a| a.trim() == "#[derive(Debug)]"));
    assert!(
        !attributes.iter().any(|a| a.contains("Doc comment")),
        "the attribute scan must stop at the first line that is not an attribute"
    );

    let no_attributes = attributes_above(fixture, "pub enum NotThisOne {");
    assert!(no_attributes.is_empty(), "an undecorated enum must report no attributes");
}

// ---------------------------------------------------------------------------
// Guard 1: the tag type space against the oracle's `enum lfs_type`
// ---------------------------------------------------------------------------

/// What an `enum lfs_type` constant means for this crate's decoder.
#[derive(Debug)]
enum OracleRole {
    /// The constant's 11 bit value must decode to exactly this `TagType`, and
    /// that variant must re-encode to the same value.
    Decodes(TagType),
    /// The constant names an abstract prefix rather than a concrete type: its
    /// high 3 bits select an `AbstractType` and its chunk byte is zero.
    Prefix(AbstractType),
    /// The constant is an in memory marker of the C reference's own commit
    /// machinery and never names a tag on disk, so the crate has no
    /// counterpart by design. The string records why.
    InMemoryOnly(&'static str),
}

/// Every constant in the oracle's `enum lfs_type`, classified.
///
/// Several values are deliberately aliased upstream (`LFS_TYPE_STRUCT` and
/// `LFS_TYPE_DIRSTRUCT` are both `0x200`, `LFS_TYPE_CRC` and `LFS_TYPE_CCRC`
/// are both `0x500`, `LFS_TYPE_TAIL` and `LFS_TYPE_SOFTTAIL` are both
/// `0x600`). This table is keyed by name, not by value, so each alias carries
/// its own role and neither shadows the other.
fn oracle_type_roles() -> Vec<(&'static str, OracleRole)> {
    vec![
        // Abstract prefixes: the chunk byte is not part of the constant.
        ("LFS_TYPE_NAME", OracleRole::Prefix(AbstractType::Name)),
        ("LFS_TYPE_FROM", OracleRole::Prefix(AbstractType::From)),
        ("LFS_TYPE_STRUCT", OracleRole::Prefix(AbstractType::Struct)),
        ("LFS_TYPE_USERATTR", OracleRole::Prefix(AbstractType::UserAttr)),
        ("LFS_TYPE_SPLICE", OracleRole::Prefix(AbstractType::Splice)),
        ("LFS_TYPE_CRC", OracleRole::Prefix(AbstractType::Crc)),
        ("LFS_TYPE_TAIL", OracleRole::Prefix(AbstractType::Tail)),
        ("LFS_TYPE_GLOBALS", OracleRole::Prefix(AbstractType::Globals)),
        // Concrete types the decoder must recognize.
        ("LFS_TYPE_REG", OracleRole::Decodes(TagType::RegularFile)),
        ("LFS_TYPE_DIR", OracleRole::Decodes(TagType::Directory)),
        ("LFS_TYPE_SUPERBLOCK", OracleRole::Decodes(TagType::Superblock)),
        ("LFS_TYPE_DIRSTRUCT", OracleRole::Decodes(TagType::DirStruct)),
        ("LFS_TYPE_INLINESTRUCT", OracleRole::Decodes(TagType::InlineStruct)),
        ("LFS_TYPE_CTZSTRUCT", OracleRole::Decodes(TagType::CtzStruct)),
        ("LFS_TYPE_CREATE", OracleRole::Decodes(TagType::Create)),
        ("LFS_TYPE_DELETE", OracleRole::Decodes(TagType::Delete)),
        ("LFS_TYPE_CCRC", OracleRole::Decodes(TagType::CommitCrc(0))),
        ("LFS_TYPE_FCRC", OracleRole::Decodes(TagType::ForwardCrc)),
        ("LFS_TYPE_SOFTTAIL", OracleRole::Decodes(TagType::SoftTail)),
        ("LFS_TYPE_HARDTAIL", OracleRole::Decodes(TagType::HardTail)),
        ("LFS_TYPE_MOVESTATE", OracleRole::Decodes(TagType::MoveState)),
        // Commit-time instructions. The C reference passes these in the
        // attribute list handed to `lfs_dir_commit`, which expands them into
        // the tags that actually land; the crate decodes their values for
        // completeness of the type space.
        ("LFS_FROM_MOVE", OracleRole::Decodes(TagType::FromMove)),
        ("LFS_FROM_USERATTRS", OracleRole::Decodes(TagType::FromUserAttrs)),
        (
            "LFS_FROM_NOOP",
            OracleRole::InMemoryOnly(
                "a filter marker whose value 0x000 collides with LFS_TYPE_NAME; \
                 it is never written and has no on disk meaning",
            ),
        ),
    ]
}

#[test]
fn oracle_type_constants_are_all_classified() {
    let parsed = parse_c_enum(&oracle_header(), "lfs_type");
    let parsed_names: BTreeSet<&str> = parsed.keys().map(String::as_str).collect();
    let table_names: BTreeSet<&str> = oracle_type_roles().iter().map(|(n, _)| *n).collect();

    let unclassified: Vec<&&str> = parsed_names.difference(&table_names).collect();
    assert!(
        unclassified.is_empty(),
        "the vendored oracle declares tag type constants this crate has not classified: \
         {unclassified:?}. A newer oracle revision has widened the type space; decide what \
         the decoder does about each one and extend `oracle_type_roles`."
    );

    let vanished: Vec<&&str> = table_names.difference(&parsed_names).collect();
    assert!(
        vanished.is_empty(),
        "this crate classifies tag type constants the vendored oracle no longer declares: \
         {vanished:?}. Either the oracle pin moved or the parser has rotted."
    );
}

#[test]
fn every_oracle_type_constant_decodes_as_classified() {
    let parsed = parse_c_enum(&oracle_header(), "lfs_type");
    for (name, role) in oracle_type_roles() {
        let value = *parsed
            .get(name)
            .unwrap_or_else(|| panic!("`{name}` is absent from the oracle header"));
        let value = u16::try_from(value).unwrap_or_else(|_| panic!("`{name}` is not a u16"));
        assert!(value <= 0x7ff, "`{name}` = {value:#05x} does not fit the 11 bit type field");

        match role {
            OracleRole::Decodes(expected) => {
                assert_eq!(
                    TagType::from_bits(value),
                    expected,
                    "`{name}` = {value:#05x} must decode to {expected:?}"
                );
                assert_eq!(
                    expected.into_bits(),
                    value,
                    "{expected:?} must re-encode to `{name}` = {value:#05x}"
                );
            }
            OracleRole::Prefix(expected) => {
                assert_eq!(
                    value & 0xff,
                    0,
                    "`{name}` = {value:#05x} is classified as an abstract prefix but its \
                     chunk byte is not zero"
                );
                let bits = u8::try_from(value >> 8).expect("11 bit value shifted by 8 fits a u8");
                assert_eq!(
                    AbstractType::from_bits(bits),
                    Some(expected),
                    "`{name}` = {value:#05x} must select abstract type {expected:?}"
                );
                assert_eq!(
                    expected as u8, bits,
                    "{expected:?} must carry the discriminant `{name}` >> 8 = {bits:#03x}"
                );
            }
            OracleRole::InMemoryOnly(reason) => {
                // Nothing numeric to check, so the recorded reason is what
                // stands in its place. An empty one means the classification
                // was made without an argument behind it.
                assert!(
                    reason.len() > 20,
                    "`{name}` is excluded from the on disk type space without a stated reason"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Guard 2: the reverse census over the whole tag type domain
// ---------------------------------------------------------------------------

/// The bucket a decoded tag falls into, keyed by variant identifier.
///
/// The wildcard arm is forced by `#[non_exhaustive]` and is not slack: a value
/// reaching it lands in the `UNCLASSIFIED` bucket, which the census pins at
/// zero. That is how a variant added to `TagType` and wired into `from_bits`
/// fails this suite.
fn bucket_of(t: TagType) -> &'static str {
    match t {
        TagType::RegularFile => "RegularFile",
        TagType::Directory => "Directory",
        TagType::Superblock => "Superblock",
        TagType::FromMove => "FromMove",
        TagType::FromUserAttrs => "FromUserAttrs",
        TagType::DirStruct => "DirStruct",
        TagType::InlineStruct => "InlineStruct",
        TagType::CtzStruct => "CtzStruct",
        TagType::UserAttr(_) => "UserAttr",
        TagType::Create => "Create",
        TagType::Delete => "Delete",
        TagType::CommitCrc(_) => "CommitCrc",
        TagType::ForwardCrc => "ForwardCrc",
        TagType::SoftTail => "SoftTail",
        TagType::HardTail => "HardTail",
        TagType::MoveState => "MoveState",
        TagType::RelocateState => "RelocateState",
        TagType::Unknown { .. } => "Unknown",
        _ => "UNCLASSIFIED",
    }
}

/// How many of the 2048 type field values each variant claims.
///
/// Per bucket, never a total: a floor on the sum would let one variant widen
/// while another narrows without anything failing. `UserAttr` claims all 256
/// chunks of `0x3xx` because the chunk byte is the caller's attribute id;
/// `CommitCrc` claims four because the low two bits of `0x5xx` are the erase
/// state hint. `RelocateState` is this crate's own extension at the unused
/// `0x7fe` slot (ADR-0005) and has no oracle counterpart, which is why the
/// oracle table above does not name it.
const EXPECTED_CENSUS: [(&str, usize); 19] = [
    ("RegularFile", 1),
    ("Directory", 1),
    ("Superblock", 1),
    ("FromMove", 1),
    ("FromUserAttrs", 1),
    ("DirStruct", 1),
    ("InlineStruct", 1),
    ("CtzStruct", 1),
    ("UserAttr", 256),
    ("Create", 1),
    ("Delete", 1),
    ("CommitCrc", 4),
    ("ForwardCrc", 1),
    ("SoftTail", 1),
    ("HardTail", 1),
    ("MoveState", 1),
    ("RelocateState", 1),
    ("Unknown", 1773),
    ("UNCLASSIFIED", 0),
];

#[test]
fn tag_type_census_over_the_whole_domain_is_pinned() {
    let mut census: BTreeMap<&str, usize> = BTreeMap::new();
    for value in 0u16..=0x7ff {
        *census.entry(bucket_of(TagType::from_bits(value))).or_insert(0) += 1;
    }

    for (bucket, expected) in EXPECTED_CENSUS {
        let actual = census.get(bucket).copied().unwrap_or(0);
        assert_eq!(
            actual, expected,
            "bucket `{bucket}` claims {actual} of the 2048 type field values, pinned at \
             {expected}. The tag decoder changed; update the pin only with the spec or the \
             oracle in hand."
        );
    }

    let pinned: BTreeSet<&str> = EXPECTED_CENSUS.iter().map(|(b, _)| *b).collect();
    let observed: BTreeSet<&str> = census.keys().copied().collect();
    let surprises: Vec<&&str> = observed.difference(&pinned).collect();
    assert!(surprises.is_empty(), "the decoder produced unpinned buckets: {surprises:?}");

    let total: usize = EXPECTED_CENSUS.iter().map(|(_, n)| n).sum();
    assert_eq!(total, 2048, "the pinned census must partition the whole 11 bit type field");
}

#[test]
fn tag_type_round_trips_over_the_whole_domain() {
    for value in 0u16..=0x7ff {
        assert_eq!(
            TagType::from_bits(value).into_bits(),
            value,
            "decoding then re-encoding {value:#05x} must be the identity"
        );
    }
}

#[test]
fn declared_tag_variants_match_the_census_buckets() {
    let declared = declared_variants(&read_repo_file("src/tag.rs"), "pub enum TagType {");
    let pinned: BTreeSet<String> = EXPECTED_CENSUS
        .iter()
        .map(|(b, _)| *b)
        .filter(|b| *b != "UNCLASSIFIED")
        .map(str::to_string)
        .collect();
    assert_eq!(
        declared, pinned,
        "the variants declared in `src/tag.rs` and the census buckets have diverged. A variant \
         declared but never returned by `from_bits` is unreachable from disk; a bucket without a \
         declaration means the parser has rotted."
    );
}

#[test]
fn abstract_type_domain_is_exactly_three_bits() {
    let source = read_repo_file("src/tag.rs");
    let declared = declared_variants(&source, "pub enum AbstractType {");
    assert_eq!(
        declared.len(),
        8,
        "the abstract type field is 3 bits and every value in 0..8 is spoken for; found {declared:?}"
    );

    for b in 0u8..=0xff {
        let decoded = AbstractType::from_bits(b);
        if b < 8 {
            let decoded = decoded.unwrap_or_else(|| panic!("{b:#03x} must decode"));
            assert_eq!(decoded as u8, b, "{decoded:?} must carry discriminant {b:#03x}");
        } else {
            assert!(decoded.is_none(), "{b:#03x} is outside the 3 bit field and must not decode");
        }
    }
}

// ---------------------------------------------------------------------------
// Guard 3: the error set
// ---------------------------------------------------------------------------

/// What this crate does with each `enum lfs_error` code.
#[derive(Debug)]
enum ErrorRole {
    /// The crate has a variant for exactly this condition.
    Mapped(Error),
    /// The crate folds this condition into a coarser variant. The string
    /// records the reasoning, since no test can check intent.
    Widened(Error, &'static str),
    /// The condition cannot arise here. The string records why.
    Unreachable(&'static str),
}

/// Every constant in the oracle's `enum lfs_error`, classified.
///
/// This table is documentation with a mechanical spine, and the split matters.
/// The mechanical part is coverage: every code the oracle declares appears
/// here, and every crate variant named here exists in the pinned error set
/// below. Both halves fail loudly. The prose reasons are not checkable and do
/// not pretend to be.
///
/// The reason no numeric comparison appears in this guard: `Error` is not a
/// mirror of `enum lfs_error`. The C reference returns negative errno values
/// through an `int` return channel, a C idiom this crate has no reason to
/// inherit, and it carries no discriminants at all. The two sets also do not
/// align one to one in either direction. The crate is finer grained where a
/// caller can act on the difference (`Unformatted` versus `Corrupt` versus
/// `NotLittleFs`, all `LFS_ERR_CORRUPT` upstream) and coarser where a caller
/// cannot (`OutOfRange` absorbs `LFS_ERR_NOSPC` and `LFS_ERR_FBIG`). Pinning a
/// numeric equivalence would therefore assert something false. What is pinned
/// instead is that the mapping decision has been made and recorded for every
/// upstream code.
fn oracle_error_roles() -> Vec<(&'static str, ErrorRole)> {
    vec![
        (
            "LFS_ERR_OK",
            ErrorRole::Unreachable("success is `Ok(_)`; the error type holds no success value"),
        ),
        ("LFS_ERR_IO", ErrorRole::Mapped(Error::Io)),
        ("LFS_ERR_CORRUPT", ErrorRole::Mapped(Error::Corrupt)),
        ("LFS_ERR_NOENT", ErrorRole::Mapped(Error::NotFound)),
        ("LFS_ERR_EXIST", ErrorRole::Mapped(Error::AlreadyExists)),
        (
            "LFS_ERR_NOTDIR",
            ErrorRole::Widened(
                Error::NotFound,
                "`resolve_parent` reports an intermediate component that is not a directory the \
                 same way it reports one that is missing: the path does not resolve",
            ),
        ),
        (
            "LFS_ERR_ISDIR",
            ErrorRole::Widened(
                Error::AlreadyExists,
                "an operation aimed at a file that finds a directory in that name reports the \
                 name as taken",
            ),
        ),
        ("LFS_ERR_NOTEMPTY", ErrorRole::Mapped(Error::NotEmpty)),
        (
            "LFS_ERR_BADF",
            ErrorRole::Unreachable(
                "there is no descriptor table; a `File` is an owned typed handle, so a stale or \
                 invalid descriptor is unrepresentable",
            ),
        ),
        (
            "LFS_ERR_FBIG",
            ErrorRole::Widened(
                Error::OutOfRange,
                "one code covers every capacity and geometry limit the caller exceeded",
            ),
        ),
        (
            "LFS_ERR_INVAL",
            ErrorRole::Widened(
                Error::InvalidPath,
                "the invalid argument the caller controls is almost always the path; a bad \
                 geometry surfaces as `GeometryMismatch` and an oversized body as `OutOfRange`",
            ),
        ),
        (
            "LFS_ERR_NOSPC",
            ErrorRole::Widened(
                Error::OutOfRange,
                "the allocator reports a device with too few free blocks through the same \
                 capacity code",
            ),
        ),
        (
            "LFS_ERR_NOMEM",
            ErrorRole::Unreachable(
                "the kernel never allocates; every buffer is caller provided, so there is no \
                 allocation failure to report",
            ),
        ),
        (
            "LFS_ERR_NOATTR",
            ErrorRole::Unreachable(
                "a missing attribute is not an error here: `Fs::get_attr` returns `Ok(0)`, the \
                 same answer as an attribute present but empty",
            ),
        ),
        (
            "LFS_ERR_NAMETOOLONG",
            ErrorRole::Widened(
                Error::InvalidPath,
                "`Path` validates component length at construction, so an overlong name is \
                 rejected as a malformed path before any operation begins",
            ),
        ),
    ]
}

/// Every variant of this crate's `Error`, with the `Display` text it renders.
///
/// Per variant, never a count alone: a bare count would pass if one variant
/// were swapped for another.
const EXPECTED_ERRORS: [(&str, &str); 15] = [
    ("Io", "storage I/O error"),
    ("NotLittleFs", "not a LittleFS v2 image"),
    ("UnsupportedVersion", "unsupported on disk format version"),
    ("CrcMismatch", "CRC check failed"),
    ("InvalidTag", "invalid metadata tag"),
    ("InvalidPath", "invalid path component"),
    ("ShortRead", "storage returned a short read"),
    ("OutOfRange", "value exceeds the negotiated geometry limit"),
    ("ReadOnly", "filesystem is mounted read only"),
    ("NotFound", "no such file or directory"),
    ("AlreadyExists", "name already exists"),
    ("GeometryMismatch", "storage geometry does not match the superblock"),
    ("Corrupt", "filesystem is corrupt"),
    ("Unformatted", "root metadata pair is in erased state (device not formatted)"),
    ("NotEmpty", "directory is not empty"),
];

/// One constructed value per pinned variant, in the same order, so `Display`
/// can be exercised. `UnsupportedVersion` is the only variant carrying a
/// payload and its text does not depend on the value.
fn sample_errors() -> [Error; 15] {
    [
        Error::Io,
        Error::NotLittleFs,
        Error::UnsupportedVersion(0x0002_0009),
        Error::CrcMismatch,
        Error::InvalidTag,
        Error::InvalidPath,
        Error::ShortRead,
        Error::OutOfRange,
        Error::ReadOnly,
        Error::NotFound,
        Error::AlreadyExists,
        Error::GeometryMismatch,
        Error::Corrupt,
        Error::Unformatted,
        Error::NotEmpty,
    ]
}

#[test]
fn declared_error_variants_are_pinned() {
    let declared = declared_variants(&read_repo_file("src/error.rs"), "pub enum Error {");
    let pinned: BTreeSet<String> =
        EXPECTED_ERRORS.iter().map(|(name, _)| (*name).to_string()).collect();
    assert_eq!(
        declared, pinned,
        "the error set in `src/error.rs` has drifted from its pin. Adding a failure mode is a \
         public surface change: extend `EXPECTED_ERRORS`, classify it against the oracle table, \
         and say so in the changelog."
    );
}

#[test]
fn every_error_renders_its_pinned_text() {
    let samples = sample_errors();
    assert_eq!(samples.len(), EXPECTED_ERRORS.len());
    let mut seen = BTreeSet::new();
    for (error, (name, text)) in samples.iter().zip(EXPECTED_ERRORS) {
        assert_eq!(
            error.to_string(),
            text,
            "the `Display` text for `Error::{name}` has drifted from its pin"
        );
        assert!(seen.insert(text), "two error variants render the same text: `{text}`");
    }
}

#[test]
fn oracle_error_codes_are_all_classified() {
    let parsed = parse_c_enum(&oracle_header(), "lfs_error");
    let parsed_names: BTreeSet<&str> = parsed.keys().map(String::as_str).collect();
    let table = oracle_error_roles();
    let table_names: BTreeSet<&str> = table.iter().map(|(n, _)| *n).collect();

    let unclassified: Vec<&&str> = parsed_names.difference(&table_names).collect();
    assert!(
        unclassified.is_empty(),
        "the vendored oracle declares error codes this crate has not classified: \
         {unclassified:?}. Decide whether each one needs a variant here and extend \
         `oracle_error_roles`."
    );

    let vanished: Vec<&&str> = table_names.difference(&parsed_names).collect();
    assert!(
        vanished.is_empty(),
        "this crate classifies error codes the vendored oracle no longer declares: {vanished:?}."
    );

    // Every crate variant named by the table must exist in the pinned set,
    // and every classification that is not a plain one to one mapping must
    // carry a distinct stated reason. A shared or empty reason is the tell
    // that a code was swept into a bucket rather than decided about.
    let pinned: BTreeSet<&str> = EXPECTED_ERRORS.iter().map(|(name, _)| *name).collect();
    let mut reasons: BTreeSet<&str> = BTreeSet::new();
    for (name, role) in &table {
        let named = match role {
            ErrorRole::Mapped(e) => Some(*e),
            ErrorRole::Widened(e, reason) => {
                assert!(reason.len() > 20, "`{name}` is classified without a stated reason");
                assert!(reasons.insert(reason), "`{name}` reuses another code's reason verbatim");
                Some(*e)
            }
            ErrorRole::Unreachable(reason) => {
                assert!(reason.len() > 20, "`{name}` is classified without a stated reason");
                assert!(reasons.insert(reason), "`{name}` reuses another code's reason verbatim");
                None
            }
        };
        let Some(named) = named else {
            continue;
        };
        let debug = format!("{named:?}");
        let ident = debug.split('(').next().unwrap_or(&debug);
        assert!(
            pinned.contains(ident),
            "`{name}` is classified onto `Error::{ident}`, which is not in the pinned error set"
        );
    }
}

// ---------------------------------------------------------------------------
// Guard 4: the format constants
// ---------------------------------------------------------------------------

#[test]
fn format_constants_match_the_oracle() {
    let header = oracle_header();

    let disk_version = parse_c_define(&header, "LFS_DISK_VERSION");
    assert_eq!(
        i64::from(DISK_VERSION),
        disk_version,
        "`DISK_VERSION` in `src/lib.rs` and `LFS_DISK_VERSION` in the pinned oracle disagree"
    );

    let name_max = parse_c_define(&header, "LFS_NAME_MAX");
    assert_eq!(
        i64::try_from(NAME_MAX).expect("NAME_MAX fits an i64"),
        name_max,
        "`NAME_MAX` in `src/lib.rs` and `LFS_NAME_MAX` in the pinned oracle disagree"
    );
}

#[test]
fn superblock_identity_constants_match_the_oracle() {
    let source = oracle_source();

    // The magic string, anchored on the oracle's own comparison rather than on
    // any restatement of it. A literal substring of a pinned source file is a
    // blunt instrument and is meant to be: moving the pin must re-verify it.
    assert_eq!(MAGIC, b"littlefs", "`MAGIC` in `src/lib.rs` is not the LittleFS magic");
    assert!(
        source.contains(r#"memcmp(superblock.d.magic, "littlefs", 8)"#),
        "the pinned oracle no longer compares the superblock magic the way this guard expects; \
         re-derive `MAGIC` from the oracle and the specification before touching this assertion"
    );

    // The root pair is fixed at blocks 0 and 1, which the oracle states by
    // fetching it from a literal.
    assert_eq!(ROOT_BLOCK_PAIR.a.as_u32(), 0, "the root pair's first block is 0");
    assert_eq!(ROOT_BLOCK_PAIR.b.as_u32(), 1, "the root pair's second block is 1");
    assert!(
        source.contains("lfs_dir_fetch(lfs, &root, (const lfs_block_t[2]){0, 1})"),
        "the pinned oracle no longer fetches the root pair from blocks 0 and 1 the way this \
         guard expects; re-derive `ROOT_BLOCK_PAIR` before touching this assertion"
    );
}

/// The oracle limits with no constant in this crate, recorded rather than
/// checked.
///
/// `LFS_FILE_MAX` and `LFS_ATTR_MAX` are superblock fields, not compile time
/// constants here. `Fs::format` writes zero into `name_max`, `file_max`, and
/// `attr_max`, which is the encoding for "the driver's default" and is what
/// the C reference writes too, so there is no crate side constant to compare
/// and nothing to pin beyond the fields' presence. This test exists to keep
/// the two limits from being quietly forgotten: it fails if the oracle stops
/// declaring them, which is the moment the decision would need revisiting.
#[test]
fn oracle_limits_without_a_crate_constant_still_exist() {
    let header = oracle_header();
    assert_eq!(parse_c_define(&header, "LFS_FILE_MAX"), 2_147_483_647);
    assert_eq!(parse_c_define(&header, "LFS_ATTR_MAX"), 1022);
}

// ---------------------------------------------------------------------------
// Guard 5: the `#[non_exhaustive]` pins
// ---------------------------------------------------------------------------

/// Every closed set this crate exposes is `#[non_exhaustive]`, and that is a
/// semver commitment: `src/lib.rs` promises that a new spec driven variant
/// ships as a minor release. Dropping the attribute would silently convert
/// the next such addition into a breaking change, so the attribute is pinned
/// exactly like the members are.
#[test]
fn public_closed_sets_stay_non_exhaustive() {
    for (file, decl) in [
        ("src/error.rs", "pub enum Error {"),
        ("src/tag.rs", "pub enum TagType {"),
        ("src/tag.rs", "pub enum AbstractType {"),
        ("src/dir.rs", "pub enum EntryKind {"),
    ] {
        let attributes = attributes_above(&read_repo_file(file), decl);
        assert!(
            attributes.iter().any(|a| a.trim() == "#[non_exhaustive]"),
            "`{decl}` in {file} lost its `#[non_exhaustive]` attribute; that attribute is part \
             of the semver contract stated in `src/lib.rs`"
        );
    }
}

/// `EntryKind` mirrors the two file types the format defines, and nothing
/// else. Directory entries on disk carry one of exactly two NAME tag types.
#[test]
fn entry_kind_mirrors_the_two_file_types() {
    let declared = declared_variants(&read_repo_file("src/dir.rs"), "pub enum EntryKind {");
    let expected: BTreeSet<String> =
        ["RegularFile", "Directory"].iter().map(|s| (*s).to_string()).collect();
    assert_eq!(declared, expected, "`EntryKind` has drifted from the format's file types");

    let parsed = parse_c_enum(&oracle_header(), "lfs_type");
    assert_eq!(parsed.get("LFS_TYPE_REG"), Some(&0x001));
    assert_eq!(parsed.get("LFS_TYPE_DIR"), Some(&0x002));
}
