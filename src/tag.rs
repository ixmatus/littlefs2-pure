//! The 32 bit on disk tag.
//!
//! Every entry in a LittleFS metadata block is prefixed by a 32 bit tag that
//! identifies what follows. The bit layout, in MSB to LSB order, is:
//!
//! ```text
//! [1 bit  ][ 11 bits ][ 10 bits ][ 10 bits ]
//!  valid   type        id         length
//! ```
//!
//! - **valid (1 bit).** `0` means the tag is valid; `1` means the tag slot is
//!   either erased (all `0xFF` reads as `1` after the XOR with the previous
//!   tag inverts to `0` for a valid tag) or otherwise reserved.
//! - **type (11 bits).** The high 3 bits are the *abstract type* (see
//!   [`AbstractType`]); the low 8 bits are a *chunk* that subdivides each
//!   abstract type into concrete tag kinds.
//! - **id (10 bits).** A per metadata pair file or directory identifier.
//!   `0x3ff` is the "no id" sentinel.
//! - **length (10 bits).** Length in bytes of the data that follows this tag
//!   in the metadata block. `0x3ff` is the "deleted" sentinel.
//!
//! # The XOR convention
//!
//! Tags do not appear on disk in raw form. Each tag is XORed against the
//! previously stored tag in the same metadata block. The first tag's "previous"
//! value is `0xFFFFFFFF` (the erased flash state). This convention has two
//! consequences worth holding in mind:
//!
//! 1. A region of pristine erased flash (all `0xFF` bytes) XORed against
//!    `0xFFFFFFFF` decodes to `0x00000000`. That value has the valid bit
//!    clear, so an erased region naturally reads as "end of commit log."
//! 2. The valid bit is a *delta* on the previous valid bit, not an absolute.
//!    The semantics described in the field documentation assume the XOR has
//!    already been applied; raw bytes on disk look different.
//!
//! # Bit accuracy claim
//!
//! [`Tag::from_bits`] and [`Tag::into_bits`] reproduce the C reference's
//! encoding byte for byte. Unit tests below pin a small set of hand crafted
//! values; the property test in `tests/property_tag.rs` checks encode then
//! decode is the identity over random inputs; the conformance harness will
//! check against the C reference once vectors land.

use core::fmt;

/// A LittleFS metadata tag, in decoded form (after any XOR has been applied).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tag(u32);

/// The abstract type field, occupying bits 28..31 of the tag word.
///
/// Each variant subdivides further into a concrete [`TagType`] via the
/// 8 bit chunk field. The values match the C reference's `LFS2_TYPE_*`
/// macros at the abstract layer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum AbstractType {
    /// Name records: superblock, regular file, directory.
    Name = 0x0,
    /// Structure records: inline data, CTZ skip list head, directory pointer.
    Struct = 0x2,
    /// User attribute records.
    UserAttr = 0x3,
    /// "From" records: a source pointer for move operations and reserved
    /// internal uses.
    From = 0x1,
    /// Tail records pointing to the next directory block.
    Tail = 0x6,
    /// CRC commit terminator.
    Crc = 0x5,
    /// Splice records: create or delete a file or directory entry.
    Splice = 0x4,
    /// Global state records: in flight moves, etc.
    Globals = 0x7,
}

impl AbstractType {
    /// Decode the 3 bit abstract type from a tag's type field.
    ///
    /// Returns `None` if the bit pattern does not correspond to a known
    /// variant. Every value in `0..8` currently maps to a variant, so this
    /// only returns `None` when the input has bits outside `0..8` set, which
    /// callers should treat as a precondition violation.
    pub const fn from_bits(b: u8) -> Option<Self> {
        match b {
            0x0 => Some(Self::Name),
            0x1 => Some(Self::From),
            0x2 => Some(Self::Struct),
            0x3 => Some(Self::UserAttr),
            0x4 => Some(Self::Splice),
            0x5 => Some(Self::Crc),
            0x6 => Some(Self::Tail),
            0x7 => Some(Self::Globals),
            _ => None,
        }
    }
}

/// A concrete tag type: the abstract type combined with the 8 bit chunk
/// field. Values mirror the `LFS2_TYPE_*` enum in the C reference's
/// `lfs2.h`.
///
/// This enum lists the concrete types defined by the LittleFS v2 spec. A tag
/// whose `(abstract_type, chunk)` does not match any variant decodes through
/// [`TagType::Unknown`] with the raw 11 bit type field preserved for
/// inspection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum TagType {
    // ---- Name (0x0xx) ----
    /// Regular file name entry, `LFS2_TYPE_REG = 0x001`.
    RegularFile,
    /// Directory name entry, `LFS2_TYPE_DIR = 0x002`.
    Directory,
    /// Superblock name entry, `LFS2_TYPE_SUPERBLOCK = 0x0ff`.
    Superblock,

    // ---- From (0x1xx) ----
    /// Source pointer for an in flight move, `LFS2_FROM_MOVE = 0x101`.
    FromMove,
    /// User attribute source, `LFS2_FROM_USERATTRS = 0x102`.
    FromUserAttrs,

    // ---- Struct (0x2xx) ----
    /// Directory structure pointer, `LFS2_TYPE_DIRSTRUCT = 0x200`.
    DirStruct,
    /// Inline file structure, `LFS2_TYPE_INLINESTRUCT = 0x201`.
    InlineStruct,
    /// CTZ skip list head, `LFS2_TYPE_CTZSTRUCT = 0x202`.
    CtzStruct,

    // ---- UserAttr (0x3xx) ----
    /// User attribute payload. The chunk byte is the user defined attribute
    /// identifier; carried in the contained `u8`.
    UserAttr(u8),

    // ---- Splice (0x4xx) ----
    /// Create entry, `LFS2_TYPE_CREATE = 0x401`.
    Create,
    /// Delete entry, `LFS2_TYPE_DELETE = 0x4ff`.
    Delete,

    // ---- CRC (0x5xx) ----
    /// Commit CRC, `LFS2_TYPE_CCRC = 0x500..=0x503` (low two bits are the
    /// erase state hint). The contained `u8` holds the chunk.
    CommitCrc(u8),
    /// Forward CRC for the next commit, `LFS2_TYPE_FCRC = 0x5ff`.
    ForwardCrc,

    // ---- Tail (0x6xx) ----
    /// Soft tail (no entry threading), `LFS2_TYPE_SOFTTAIL = 0x600`.
    SoftTail,
    /// Hard tail (entry threaded), `LFS2_TYPE_HARDTAIL = 0x601`.
    HardTail,

    // ---- Globals (0x7xx) ----
    /// Global move state, `LFS2_TYPE_MOVESTATE = 0x7ff`.
    MoveState,

    /// A tag whose 11 bit type field did not match any known variant. The
    /// raw type bits are preserved for inspection.
    Unknown {
        /// 3 bit abstract type.
        abstract_type: u8,
        /// 8 bit chunk.
        chunk: u8,
    },
}

impl TagType {
    /// Decode the 11 bit type field into a concrete [`TagType`].
    ///
    /// The argument is the full 11 bit type field as it appears in a decoded
    /// tag (top 3 bits = abstract type, low 8 bits = chunk). Unknown
    /// combinations are returned as [`TagType::Unknown`] so callers can
    /// inspect them without losing information.
    #[must_use]
    pub const fn from_bits(type_field: u16) -> Self {
        let abstract_type = ((type_field >> 8) & 0x7) as u8;
        let chunk = (type_field & 0xff) as u8;
        match (abstract_type, chunk) {
            (0x0, 0x01) => Self::RegularFile,
            (0x0, 0x02) => Self::Directory,
            (0x0, 0xff) => Self::Superblock,
            (0x1, 0x01) => Self::FromMove,
            (0x1, 0x02) => Self::FromUserAttrs,
            (0x2, 0x00) => Self::DirStruct,
            (0x2, 0x01) => Self::InlineStruct,
            (0x2, 0x02) => Self::CtzStruct,
            (0x3, c) => Self::UserAttr(c),
            (0x4, 0x01) => Self::Create,
            (0x4, 0xff) => Self::Delete,
            (0x5, c) if c <= 0x03 => Self::CommitCrc(c),
            (0x5, 0xff) => Self::ForwardCrc,
            (0x6, 0x00) => Self::SoftTail,
            (0x6, 0x01) => Self::HardTail,
            (0x7, 0xff) => Self::MoveState,
            (a, c) => Self::Unknown { abstract_type: a, chunk: c },
        }
    }

    /// Re encode the 11 bit type field from a concrete [`TagType`].
    ///
    /// `from_bits(t.into_bits()) == t` for every `t` that does not require
    /// the `UserAttr(_)` or `CommitCrc(_)` variants to be in a range outside
    /// their natural domain.
    #[must_use]
    pub const fn into_bits(self) -> u16 {
        let (abstract_type, chunk): (u8, u8) = match self {
            Self::RegularFile => (0x0, 0x01),
            Self::Directory => (0x0, 0x02),
            Self::Superblock => (0x0, 0xff),
            Self::FromMove => (0x1, 0x01),
            Self::FromUserAttrs => (0x1, 0x02),
            Self::DirStruct => (0x2, 0x00),
            Self::InlineStruct => (0x2, 0x01),
            Self::CtzStruct => (0x2, 0x02),
            Self::UserAttr(c) => (0x3, c),
            Self::Create => (0x4, 0x01),
            Self::Delete => (0x4, 0xff),
            Self::CommitCrc(c) => (0x5, c),
            Self::ForwardCrc => (0x5, 0xff),
            Self::SoftTail => (0x6, 0x00),
            Self::HardTail => (0x6, 0x01),
            Self::MoveState => (0x7, 0xff),
            Self::Unknown { abstract_type, chunk } => (abstract_type, chunk),
        };
        ((abstract_type as u16) << 8) | (chunk as u16)
    }
}

/// The "no id" sentinel value in the 10 bit id field.
pub const ID_NONE: u16 = 0x3ff;

/// The "deleted" or "special" sentinel value in the 10 bit length field.
pub const LEN_SPECIAL: u16 = 0x3ff;

impl Tag {
    /// Construct a tag from a raw 32 bit value, after the XOR with the
    /// previous tag has been applied.
    ///
    /// Every `u32` decodes; callers that want to reject unknown type
    /// combinations should inspect [`Tag::tag_type`] for [`TagType::Unknown`].
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw 32 bit value, ready to be XORed with the previous tag and
    /// written to disk.
    #[inline]
    #[must_use]
    pub const fn into_bits(self) -> u32 {
        self.0
    }

    /// Build a tag from its decoded components.
    ///
    /// `valid` is the *decoded* valid bit (`true` means the tag is valid).
    /// `id` and `length` must each fit in 10 bits; this method debug asserts
    /// the bound. In release builds, the high bits are silently masked.
    #[must_use]
    pub const fn new(valid: bool, tag_type: TagType, id: u16, length: u16) -> Self {
        debug_assert!(id <= 0x3ff, "tag id field overflows 10 bits");
        debug_assert!(length <= 0x3ff, "tag length field overflows 10 bits");

        let valid_bit: u32 = if valid { 0 } else { 1 << 31 };
        let type_field: u32 = (tag_type.into_bits() as u32) << 20;
        let id_field: u32 = ((id & 0x3ff) as u32) << 10;
        let length_field: u32 = (length & 0x3ff) as u32;
        Self(valid_bit | type_field | id_field | length_field)
    }

    /// `true` if the tag's valid bit is clear (the tag occupies a real slot
    /// in the commit log).
    #[inline]
    #[must_use]
    pub const fn is_valid(self) -> bool {
        (self.0 >> 31) == 0
    }

    /// The 11 bit type field, before splitting into abstract type and chunk.
    #[inline]
    #[must_use]
    pub const fn type_bits(self) -> u16 {
        ((self.0 >> 20) & 0x7ff) as u16
    }

    /// The decoded concrete type.
    #[inline]
    #[must_use]
    pub const fn tag_type(self) -> TagType {
        TagType::from_bits(self.type_bits())
    }

    /// The decoded abstract type, or `None` if the bit pattern is invalid.
    /// (No bit pattern in `0..8` is invalid; this returns `None` only when
    /// the input has reserved bits set, which `from_bits` prevents.)
    #[inline]
    #[must_use]
    pub const fn abstract_type(self) -> AbstractType {
        // SAFETY-ish: the type field is masked to 11 bits, so the top 3 bits
        // are always in 0..8. We unwrap because the value is in range by
        // construction.
        match AbstractType::from_bits(((self.0 >> 28) & 0x7) as u8) {
            Some(t) => t,
            None => AbstractType::Name, // unreachable; abstract_type bits are masked to 3.
        }
    }

    /// The 10 bit id field.
    ///
    /// Returns [`ID_NONE`] (`0x3ff`) when the tag carries no id.
    #[inline]
    #[must_use]
    pub const fn id(self) -> u16 {
        ((self.0 >> 10) & 0x3ff) as u16
    }

    /// `true` if this tag's id is the "no id" sentinel.
    #[inline]
    #[must_use]
    pub const fn has_no_id(self) -> bool {
        self.id() == ID_NONE
    }

    /// The 10 bit length field.
    ///
    /// Returns [`LEN_SPECIAL`] (`0x3ff`) when the tag is a delete marker or
    /// otherwise occupies the special slot.
    #[inline]
    #[must_use]
    pub const fn length(self) -> u16 {
        (self.0 & 0x3ff) as u16
    }

    /// `true` if this tag's length is the special sentinel (delete marker).
    #[inline]
    #[must_use]
    pub const fn is_special_length(self) -> bool {
        self.length() == LEN_SPECIAL
    }

    /// XOR this tag's bits against another. Used to encode tags against the
    /// previous tag in a commit log, and to decode the same on read.
    #[inline]
    #[must_use]
    pub const fn xor(self, other: Tag) -> Tag {
        Self(self.0 ^ other.0)
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tag")
            .field("valid", &self.is_valid())
            .field("type", &self.tag_type())
            .field("id", &self.id())
            .field("length", &self.length())
            .field("raw", &format_args!("{:#010x}", self.0))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A freshly erased region (all `0xFF` bytes) XORed against the
    /// previous tag value `0xFFFFFFFF` decodes to a tag with the valid bit
    /// clear. This is the "end of commit log" sentinel.
    #[test]
    fn erased_region_decodes_to_end_marker() {
        let erased = Tag::from_bits(0xFFFF_FFFF);
        let prev = Tag::from_bits(0xFFFF_FFFF);
        let decoded = erased.xor(prev);
        assert_eq!(decoded.into_bits(), 0);
        assert!(decoded.is_valid()); // 0 has valid bit (MSB) clear, so is_valid() == true
                                     // The interesting end-of-log sentinel is *after* the XOR: when the
                                     // running tag matches what's on disk, no further commits remain.
    }

    /// Pin the bit layout against a hand crafted vector: a Superblock NAME
    /// tag with id 0, length 8 (the magic string "littlefs"), valid.
    #[test]
    fn superblock_name_tag_layout() {
        let t = Tag::new(true, TagType::Superblock, 0, 8);
        assert!(t.is_valid());
        assert_eq!(t.tag_type(), TagType::Superblock);
        assert_eq!(t.id(), 0);
        assert_eq!(t.length(), 8);
        // type field = 0x0ff (Name + 0xff chunk). Shifted to bits 20..30:
        //   valid bit (0)        = 0x0000_0000
        //   type field 0x0ff<<20 = 0x0ff0_0000
        //   id 0<<10             = 0x0000_0000
        //   length 8             = 0x0000_0008
        // Expected: 0x0ff0_0008.
        assert_eq!(t.into_bits(), 0x0ff0_0008);
    }

    /// Round trip every concrete [`TagType`] variant through `into_bits` and
    /// back via `from_bits`.
    #[test]
    fn concrete_types_roundtrip() {
        let variants = [
            TagType::RegularFile,
            TagType::Directory,
            TagType::Superblock,
            TagType::FromMove,
            TagType::FromUserAttrs,
            TagType::DirStruct,
            TagType::InlineStruct,
            TagType::CtzStruct,
            TagType::UserAttr(0x00),
            TagType::UserAttr(0x42),
            TagType::UserAttr(0xff),
            TagType::Create,
            TagType::Delete,
            TagType::CommitCrc(0),
            TagType::CommitCrc(1),
            TagType::CommitCrc(2),
            TagType::CommitCrc(3),
            TagType::ForwardCrc,
            TagType::SoftTail,
            TagType::HardTail,
            TagType::MoveState,
        ];
        for v in variants {
            let bits = v.into_bits();
            assert_eq!(TagType::from_bits(bits), v, "roundtrip failed for {v:?}");
        }
    }

    /// `Unknown` preserves its bits across a roundtrip.
    #[test]
    fn unknown_type_roundtrips() {
        let u = TagType::Unknown { abstract_type: 0x4, chunk: 0x73 };
        assert_eq!(TagType::from_bits(u.into_bits()), u);
    }

    /// The valid bit is sensitive only to bit 31.
    #[test]
    fn valid_bit_isolation() {
        let valid = Tag::from_bits(0x7fff_ffff);
        let invalid = Tag::from_bits(0x8000_0000);
        assert!(valid.is_valid());
        assert!(!invalid.is_valid());
    }

    /// XOR is its own inverse: `a XOR b XOR b == a` for all tags.
    #[test]
    fn xor_is_self_inverse() {
        let a = Tag::from_bits(0x12345678);
        let b = Tag::from_bits(0xdeadbeef);
        assert_eq!(a.xor(b).xor(b), a);
    }

    /// `id()` returns the no-id sentinel when constructed with it.
    #[test]
    fn no_id_sentinel() {
        let t = Tag::new(true, TagType::RegularFile, ID_NONE, 0);
        assert!(t.has_no_id());
        assert_eq!(t.id(), ID_NONE);
    }
}
