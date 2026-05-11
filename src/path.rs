//! A fixed capacity, validated path type.
//!
//! LittleFS limits path components to [`crate::NAME_MAX`] = 255
//! bytes. This module exposes:
//!
//! - [`Path`]: a borrowed slice of bytes that has been validated to contain
//!   only legal LittleFS path characters and to fit within `NAME_MAX`.
//! - (Phase 1) a `PathBuf` owned variant gated on the `alloc` feature, for
//!   constructing paths at runtime.
//!
//! # Validation rules
//!
//! - The path must be valid UTF-8.
//! - The path must not be empty.
//! - The path must not exceed [`MAX_PATH`].
//! - Individual components (between `/` separators) must not exceed
//!   [`crate::NAME_MAX`] bytes.
//! - Components must not be `.` or `..`. LittleFS does not interpret these
//!   specially and would create literal entries with those names; the crate
//!   rejects them at the boundary to prevent confusion.
//! - Components must not be empty (so `//` is rejected).
//!
//! The validation is strict on construction so that downstream code can
//! assume the invariant.

use core::fmt;

use crate::error::Error;
use crate::NAME_MAX;

/// Upper bound on the total path length, in bytes. Matches the C reference's
/// practical limit (4 KiB) so a path always fits in one block.
pub const MAX_PATH: usize = 4096;

/// A borrowed, validated path.
///
/// Construct with [`Path::new`]. The contained byte slice is guaranteed valid
/// UTF-8 and to satisfy the rules in the module documentation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Path<'a> {
    inner: &'a str,
}

impl<'a> Path<'a> {
    /// Validate `s` and wrap it as a [`Path`].
    ///
    /// Returns [`Error::InvalidPath`] if any validation rule fails.
    pub fn new(s: &'a str) -> Result<Self, Error> {
        Self::validate(s)?;
        Ok(Self { inner: s })
    }

    /// The validated path as a string slice.
    #[inline]
    #[must_use]
    pub fn as_str(self) -> &'a str {
        self.inner
    }

    /// The validated path as a byte slice. UTF-8 by construction.
    #[inline]
    #[must_use]
    pub fn as_bytes(self) -> &'a [u8] {
        self.inner.as_bytes()
    }

    /// Iterator over the components, in order, excluding any leading or
    /// trailing slash. Each item is a substring containing no `/`.
    pub fn components(self) -> Components<'a> {
        let s = self.inner.trim_start_matches('/').trim_end_matches('/');
        Components { remaining: s }
    }

    /// `true` if the path begins with `/`.
    #[inline]
    #[must_use]
    pub fn is_absolute(self) -> bool {
        self.inner.as_bytes().first() == Some(&b'/')
    }

    /// `true` if the path is relative (does not begin with `/`).
    #[inline]
    #[must_use]
    pub fn is_relative(self) -> bool {
        !self.is_absolute()
    }

    /// The byte length of the path.
    #[inline]
    #[must_use]
    pub fn len(self) -> usize {
        self.inner.len()
    }

    /// `true` if the path string is empty. A constructed [`Path`] is never
    /// empty (the constructor rejects empty input), so this always returns
    /// `false`; the method exists to satisfy clippy's `len_without_is_empty`
    /// lint and to document the invariant.
    #[inline]
    #[must_use]
    pub fn is_empty(self) -> bool {
        false
    }

    /// `true` if the path is the root (`"/"`). This is the only absolute path
    /// with no components.
    #[inline]
    #[must_use]
    pub fn is_root(self) -> bool {
        self.inner == "/"
    }

    fn validate(s: &str) -> Result<(), Error> {
        if s.is_empty() || s.len() > MAX_PATH {
            return Err(Error::InvalidPath);
        }
        // Root is "/" exactly. Otherwise every component is checked.
        if s == "/" {
            return Ok(());
        }
        // Trim a single leading slash for traversal; trailing slash is
        // rejected because it implies an empty trailing component.
        let working = s.strip_prefix('/').unwrap_or(s);
        if working.is_empty() || working.ends_with('/') {
            return Err(Error::InvalidPath);
        }
        for component in working.split('/') {
            if component.is_empty() {
                return Err(Error::InvalidPath);
            }
            if component.len() > NAME_MAX {
                return Err(Error::InvalidPath);
            }
            if component == "." || component == ".." {
                return Err(Error::InvalidPath);
            }
            // No additional byte level restrictions; UTF-8 already
            // guaranteed by &str. LittleFS itself stores arbitrary bytes,
            // but the crate's surface is UTF-8 to match the embedded host
            // ecosystem.
        }
        Ok(())
    }
}

impl fmt::Debug for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Path({:?})", self.inner)
    }
}

impl fmt::Display for Path<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.inner)
    }
}

/// Iterator over the components of a [`Path`]. Returned by
/// [`Path::components`].
#[derive(Clone, Debug)]
pub struct Components<'a> {
    remaining: &'a str,
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        if let Some(idx) = self.remaining.find('/') {
            let (head, rest) = self.remaining.split_at(idx);
            self.remaining = &rest[1..];
            Some(head)
        } else {
            let head = self.remaining;
            self.remaining = "";
            Some(head)
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    use super::*;

    fn collect(p: Path<'_>) -> Vec<String> {
        p.components().map(String::from).collect()
    }

    #[test]
    fn root_path() {
        let p = Path::new("/").unwrap();
        assert!(p.is_absolute());
        assert!(p.is_root());
        assert_eq!(p.components().count(), 0);
    }

    #[test]
    fn simple_absolute() {
        let p = Path::new("/foo/bar/baz").unwrap();
        assert!(p.is_absolute());
        assert!(!p.is_root());
        assert_eq!(collect(p), &["foo", "bar", "baz"]);
    }

    #[test]
    fn simple_relative() {
        let p = Path::new("foo/bar").unwrap();
        assert!(p.is_relative());
        assert_eq!(collect(p), &["foo", "bar"]);
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Path::new(""), Err(Error::InvalidPath));
    }

    #[test]
    fn rejects_trailing_slash() {
        assert_eq!(Path::new("/foo/"), Err(Error::InvalidPath));
        assert_eq!(Path::new("foo/"), Err(Error::InvalidPath));
    }

    #[test]
    fn rejects_double_slash() {
        assert_eq!(Path::new("/foo//bar"), Err(Error::InvalidPath));
    }

    #[test]
    fn rejects_dot_components() {
        assert_eq!(Path::new("/foo/.").unwrap_err(), Error::InvalidPath);
        assert_eq!(Path::new("/foo/..").unwrap_err(), Error::InvalidPath);
        assert_eq!(Path::new("./foo").unwrap_err(), Error::InvalidPath);
        assert_eq!(Path::new("../foo").unwrap_err(), Error::InvalidPath);
    }

    #[test]
    fn rejects_oversized_component() {
        // A 256 byte component (NAME_MAX + 1).
        let big = "a".repeat(NAME_MAX + 1);
        let path = std::format!("/{big}");
        assert_eq!(Path::new(&path), Err(Error::InvalidPath));
    }

    #[test]
    fn accepts_max_size_component() {
        let big = "a".repeat(NAME_MAX);
        let path = std::format!("/{big}");
        let p = Path::new(&path).unwrap();
        assert_eq!(p.components().count(), 1);
    }
}
