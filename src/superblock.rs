//! Superblock parsing.
//!
//! The first metadata pair of a LittleFS image (blocks `(0, 1)`) holds a
//! superblock entry: a NAME tag carrying the magic string `"littlefs"`
//! followed by an INLINESTRUCT tag at id `0` whose 24 byte body encodes the
//! filesystem geometry.
//!
//! # Layout
//!
//! The INLINESTRUCT body is six little endian `u32`s, in order:
//!
//! | offset | field         | meaning                                    |
//! | -----: | ------------- | ------------------------------------------ |
//! |   `0`  | `version`     | `(major << 16) \| minor`. Currently `2.1`. |
//! |   `4`  | `block_size`  | Erase block size in bytes.                 |
//! |   `8`  | `block_count` | Total number of erase blocks.              |
//! |  `12`  | `name_max`    | Max bytes per path component.              |
//! |  `16`  | `file_max`    | Max file size in bytes.                    |
//! |  `20`  | `attr_max`    | Max user attribute size in bytes.          |
//!
//! Verified against `lfs_superblock_fromle32` in the C reference
//! (`lfs.c:474`).
//!
//! # Version compatibility
//!
//! The reader accepts a superblock when:
//!
//! - `major_version` exactly matches [`crate::DISK_VERSION`]'s major; and
//! - `minor_version` is `<=` [`crate::DISK_VERSION`]'s minor.
//!
//! Older minor versions parse successfully. Newer minors are rejected
//! with [`Error::UnsupportedVersion`].

use crate::error::Error;
use crate::meta::MetadataPair;
use crate::tag::TagType;
use crate::{DISK_VERSION, MAGIC};

/// Decoded LittleFS superblock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Superblock {
    /// Encoded as `(major << 16) | minor`.
    pub version: u32,
    /// Erase block size in bytes.
    pub block_size: u32,
    /// Total number of erase blocks.
    pub block_count: u32,
    /// Maximum path component length in bytes. Zero means "use the
    /// driver's default" ([`crate::NAME_MAX`]).
    pub name_max: u32,
    /// Maximum file size in bytes. Zero means "use the driver's default".
    pub file_max: u32,
    /// Maximum user attribute size in bytes. Zero means "use the driver's
    /// default".
    pub attr_max: u32,
}

impl Superblock {
    /// Size of the on disk INLINESTRUCT body in bytes.
    pub const SIZE: usize = 24;

    /// Major version field (high 16 bits of `version`).
    #[inline]
    #[must_use]
    pub const fn major_version(&self) -> u16 {
        (self.version >> 16) as u16
    }

    /// Minor version field (low 16 bits of `version`).
    #[inline]
    #[must_use]
    pub const fn minor_version(&self) -> u16 {
        (self.version & 0xFFFF) as u16
    }

    /// Decode 24 little endian bytes into a [`Superblock`].
    ///
    /// Returns [`Error::OutOfRange`] if the slice is the wrong length.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != Self::SIZE {
            return Err(Error::OutOfRange);
        }
        Ok(Self {
            version: read_le_u32(&bytes[0..4]),
            block_size: read_le_u32(&bytes[4..8]),
            block_count: read_le_u32(&bytes[8..12]),
            name_max: read_le_u32(&bytes[12..16]),
            file_max: read_le_u32(&bytes[16..20]),
            attr_max: read_le_u32(&bytes[20..24]),
        })
    }

    /// Encode this superblock to 24 little endian bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.version.to_le_bytes());
        out[4..8].copy_from_slice(&self.block_size.to_le_bytes());
        out[8..12].copy_from_slice(&self.block_count.to_le_bytes());
        out[12..16].copy_from_slice(&self.name_max.to_le_bytes());
        out[16..20].copy_from_slice(&self.file_max.to_le_bytes());
        out[20..24].copy_from_slice(&self.attr_max.to_le_bytes());
        out
    }

    /// Read the superblock from the root metadata pair.
    ///
    /// Walks the pair's active block looking for:
    ///
    /// 1. A `Superblock` NAME tag with body equal to [`crate::MAGIC`]
    ///    (`b"littlefs"`).
    /// 2. An `InlineStruct` tag at id `0` whose body decodes via
    ///    [`Superblock::from_bytes`].
    ///
    /// Returns:
    ///
    /// - [`Error::NotLittleFs`] if no matching magic NAME tag is present.
    /// - [`Error::Corrupt`] if the magic is present but no superblock
    ///   INLINESTRUCT follows it.
    /// - [`Error::UnsupportedVersion`] if the version field's major does
    ///   not match this crate's, or the minor is newer than this crate
    ///   supports.
    pub fn from_pair(pair: &MetadataPair<'_>) -> Result<Self, Error> {
        let mut magic_seen = false;
        let mut decoded: Option<Self> = None;

        for entry in pair.reader.iter_tags() {
            match entry.tag.tag_type() {
                TagType::Superblock if entry.body == MAGIC => {
                    magic_seen = true;
                }
                TagType::InlineStruct if entry.tag.id() == 0 && entry.body.len() == Self::SIZE => {
                    decoded = Some(Self::from_bytes(entry.body)?);
                }
                _ => {}
            }
        }

        if !magic_seen {
            return Err(Error::NotLittleFs);
        }
        let sb = decoded.ok_or(Error::Corrupt)?;

        let our_major = (DISK_VERSION >> 16) as u16;
        let our_minor = (DISK_VERSION & 0xFFFF) as u16;
        if sb.major_version() != our_major || sb.minor_version() > our_minor {
            return Err(Error::UnsupportedVersion(sb.version));
        }
        Ok(sb)
    }
}

#[inline]
fn read_le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;

    use super::*;
    use crate::block::BlockAddress;
    use crate::crc;
    use crate::tag::{Tag, TagType, ID_NONE};

    /// Build a single commit metadata block containing a complete
    /// superblock entry: NAME tag with magic + InlineStruct with geometry.
    fn build_superblock_block(sb: &Superblock, block_size: usize) -> std::vec::Vec<u8> {
        let mut buf = vec![0xFFu8; block_size];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());

        let mut ptag: u32 = 0xFFFF_FFFF;
        let mut running_crc = crc::update(crc::INIT, &buf[0..4]);
        let mut off = 4usize;

        // NAME tag with "littlefs" magic. Id 0, length 8.
        let name_tag = Tag::new(true, TagType::Superblock, 0, 8);
        let raw = name_tag.into_bits() ^ ptag;
        buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
        running_crc = crc::update(running_crc, &raw.to_be_bytes());
        buf[off + 4..off + 12].copy_from_slice(MAGIC);
        running_crc = crc::update(running_crc, MAGIC);
        ptag = name_tag.into_bits();
        off += 12;

        // InlineStruct tag with superblock body. Id 0, length 24.
        let inline_tag = Tag::new(true, TagType::InlineStruct, 0, Superblock::SIZE as u16);
        let raw = inline_tag.into_bits() ^ ptag;
        buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
        running_crc = crc::update(running_crc, &raw.to_be_bytes());
        let body = sb.to_bytes();
        buf[off + 4..off + 4 + Superblock::SIZE].copy_from_slice(&body);
        running_crc = crc::update(running_crc, &body);
        ptag = inline_tag.into_bits();
        off += 4 + Superblock::SIZE;

        // CCRC tag.
        let ccrc_tag = Tag::new(true, TagType::CommitCrc(0), ID_NONE, 4);
        let raw = ccrc_tag.into_bits() ^ ptag;
        buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
        running_crc = crc::update(running_crc, &raw.to_be_bytes());
        buf[off + 4..off + 8].copy_from_slice(&running_crc.to_le_bytes());

        buf
    }

    fn well_formed_sb() -> Superblock {
        Superblock {
            version: DISK_VERSION,
            block_size: 4096,
            block_count: 256,
            name_max: 255,
            file_max: 0x7FFF_FFFF,
            attr_max: 1022,
        }
    }

    #[test]
    fn bytes_roundtrip() {
        let sb = well_formed_sb();
        let bytes = sb.to_bytes();
        let recovered = Superblock::from_bytes(&bytes).unwrap();
        assert_eq!(recovered, sb);
    }

    #[test]
    fn rejects_wrong_size_body() {
        let short = [0u8; 12];
        assert_eq!(Superblock::from_bytes(&short).unwrap_err(), Error::OutOfRange);
        let long = [0u8; 32];
        assert_eq!(Superblock::from_bytes(&long).unwrap_err(), Error::OutOfRange);
    }

    #[test]
    fn version_split() {
        let sb = Superblock { version: 0x0002_0001, ..well_formed_sb() };
        assert_eq!(sb.major_version(), 2);
        assert_eq!(sb.minor_version(), 1);
    }

    #[test]
    fn from_pair_decodes_valid_image() {
        let sb = well_formed_sb();
        let block_a = build_superblock_block(&sb, 1024);
        let block_b = vec![0xFFu8; 1024]; // empty alternate
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        let recovered = Superblock::from_pair(&pair).unwrap();
        assert_eq!(recovered, sb);
    }

    #[test]
    fn from_pair_rejects_missing_magic() {
        // Build a block with only an InlineStruct, no NAME magic.
        let mut buf = vec![0xFFu8; 1024];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        let mut ptag: u32 = 0xFFFF_FFFF;
        let mut running_crc = crc::update(crc::INIT, &buf[0..4]);
        let inline_tag = Tag::new(true, TagType::InlineStruct, 0, Superblock::SIZE as u16);
        let raw = inline_tag.into_bits() ^ ptag;
        buf[4..8].copy_from_slice(&raw.to_be_bytes());
        running_crc = crc::update(running_crc, &raw.to_be_bytes());
        let body = well_formed_sb().to_bytes();
        buf[8..8 + Superblock::SIZE].copy_from_slice(&body);
        running_crc = crc::update(running_crc, &body);
        ptag = inline_tag.into_bits();
        let off = 8 + Superblock::SIZE;
        let ccrc_tag = Tag::new(true, TagType::CommitCrc(0), ID_NONE, 4);
        let raw = ccrc_tag.into_bits() ^ ptag;
        buf[off..off + 4].copy_from_slice(&raw.to_be_bytes());
        running_crc = crc::update(running_crc, &raw.to_be_bytes());
        buf[off + 4..off + 8].copy_from_slice(&running_crc.to_le_bytes());

        let block_b = vec![0xFFu8; 1024];
        let pair = MetadataPair::parse(BlockAddress::new(0), &buf, BlockAddress::new(1), &block_b)
            .unwrap();
        assert_eq!(Superblock::from_pair(&pair).unwrap_err(), Error::NotLittleFs);
    }

    #[test]
    fn from_pair_rejects_newer_minor() {
        let our_major = (DISK_VERSION >> 16) as u16;
        let our_minor = (DISK_VERSION & 0xFFFF) as u16;
        let newer_minor_version =
            (u32::from(our_major) << 16) | u32::from(our_minor.saturating_add(1));
        let sb = Superblock { version: newer_minor_version, ..well_formed_sb() };
        let block_a = build_superblock_block(&sb, 1024);
        let block_b = vec![0xFFu8; 1024];
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        assert_eq!(
            Superblock::from_pair(&pair).unwrap_err(),
            Error::UnsupportedVersion(newer_minor_version)
        );
    }

    #[test]
    fn from_pair_rejects_wrong_major() {
        let our_minor = (DISK_VERSION & 0xFFFF) as u16;
        let v3 = (3u32 << 16) | u32::from(our_minor);
        let sb = Superblock { version: v3, ..well_formed_sb() };
        let block_a = build_superblock_block(&sb, 1024);
        let block_b = vec![0xFFu8; 1024];
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        assert_eq!(Superblock::from_pair(&pair).unwrap_err(), Error::UnsupportedVersion(v3));
    }

    #[test]
    fn from_pair_accepts_older_minor() {
        let our_major = (DISK_VERSION >> 16) as u16;
        let older = u32::from(our_major) << 16; // minor = 0
        let sb = Superblock { version: older, ..well_formed_sb() };
        let block_a = build_superblock_block(&sb, 1024);
        let block_b = vec![0xFFu8; 1024];
        let pair =
            MetadataPair::parse(BlockAddress::new(0), &block_a, BlockAddress::new(1), &block_b)
                .unwrap();
        let recovered = Superblock::from_pair(&pair).unwrap();
        assert_eq!(recovered.version, older);
    }
}
