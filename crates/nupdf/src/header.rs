// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-xref.c (pdf_load_version),
// Copyright (C) 2004-2025 Artifex Software, Inc.

//! PDF header detection.
//!
//! The `%PDF-` marker (or `%FDF-` for form data files) may appear anywhere in
//! the first kilobyte. Bytes before it are junk that shift every file offset;
//! the header's `offset` is that shift.

use std::fmt;

/// How many leading bytes are searched for the marker. Same window as MuPDF.
pub const SEARCH_WINDOW: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Fdf,
}

/// A PDF version in tenths: 1.7 is `Version(17)`.
///
/// Kept in MuPDF's representation so odd headers such as `%PDF-1.10` (which
/// MuPDF reads as 1.1) compare equal in the differential harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub i32);

impl Version {
    /// Versions the spec defines: 1.0 through 1.7, and 2.0.
    pub fn is_known(self) -> bool {
        (10..=17).contains(&self.0) || self.0 == 20
    }

    /// Reads a version number the way MuPDF does: as a float, rounded to
    /// tenths. Garbage reads as 0.0, matching C `atof`.
    pub fn parse_lenient(text: &[u8]) -> Version {
        // Same f32 arithmetic as MuPDF, then truncation toward zero.
        Version((10.0f32 * (crate::strtof::fz_atof(text) + 0.05f32)) as i32)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Matches MuPDF's "%d.%d" with C truncating division.
        write!(f, "{}.{}", self.0 / 10, self.0 % 10)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    /// Bytes of junk before the marker. Offsets in the file are relative to it.
    pub offset: usize,
    pub version: Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Fewer than five bytes: there is no room for a marker.
    TooShort,
    /// No `%PDF-` or `%FDF-` within the search window.
    MissingMarker,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::TooShort => f.write_str("file is too short to be a PDF"),
            HeaderError::MissingMarker => {
                write!(f, "no %PDF- header in the first {SEARCH_WINDOW} bytes")
            }
        }
    }
}

impl std::error::Error for HeaderError {}

/// Finds the header marker using the same window MuPDF searches.
pub fn find(data: &[u8]) -> Result<Header, HeaderError> {
    let n = data.len().min(SEARCH_WINDOW);
    if n < 5 {
        return Err(HeaderError::TooShort);
    }
    // MuPDF reads n bytes and overwrites the last with NUL, so the final byte
    // of the window is never part of the marker or the version.
    let window = &data[..n - 1];
    let offset = window
        .windows(5)
        .position(|w| w == b"%PDF-" || w == b"%FDF-")
        .ok_or(HeaderError::MissingMarker)?;
    let kind = if window.get(offset + 1) == Some(&b'F') {
        Kind::Fdf
    } else {
        Kind::Pdf
    };
    let version = Version::parse_lenient(window.get(offset + 5..).unwrap_or_default());
    Ok(Header {
        kind,
        offset,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version_of(header: &[u8]) -> Version {
        find(header).unwrap().version
    }

    #[test]
    fn reads_standard_header() {
        let header = find(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n").unwrap();
        assert_eq!(header.kind, Kind::Pdf);
        assert_eq!(header.offset, 0);
        assert_eq!(header.version, Version(17));
        assert_eq!(header.version.to_string(), "1.7");
    }

    #[test]
    fn junk_before_marker_becomes_offset() {
        let mut data = vec![b'x'; 300];
        data.extend_from_slice(b"%PDF-1.6\n");
        let header = find(&data).unwrap();
        assert_eq!(header.offset, 300);
        assert_eq!(header.version, Version(16));
    }

    #[test]
    fn recognizes_fdf() {
        assert_eq!(find(b"%FDF-1.2\n").unwrap().kind, Kind::Fdf);
    }

    #[test]
    fn rejects_files_without_marker() {
        assert_eq!(find(b"hello world"), Err(HeaderError::MissingMarker));
        assert_eq!(find(b"%PDF"), Err(HeaderError::TooShort));
        assert_eq!(find(b""), Err(HeaderError::TooShort));
    }

    #[test]
    fn marker_must_start_inside_mupdf_window() {
        // The last window position MuPDF checks is SEARCH_WINDOW - 6.
        let at = |pos: usize| {
            let mut data = vec![b' '; pos];
            data.extend_from_slice(b"%PDF-1.4\n");
            find(&data)
        };
        let last = at(SEARCH_WINDOW - 6).unwrap();
        assert_eq!(last.offset, SEARCH_WINDOW - 6);
        // Its version digits fall past the NUL MuPDF writes at byte 1023.
        assert_eq!(last.version, Version(0));
        assert_eq!(at(SEARCH_WINDOW - 5), Err(HeaderError::MissingMarker));
    }

    #[test]
    fn parses_versions_like_mupdf() {
        assert_eq!(version_of(b"%PDF-2.0\n"), Version(20));
        assert_eq!(version_of(b"%PDF-1.10\n"), Version(11));
        assert_eq!(version_of(b"%PDF- 1.4\n"), Version(14));
        assert_eq!(version_of(b"%PDF-1\n"), Version(10));
        assert_eq!(version_of(b"%PDF-.5\n"), Version(5));
        assert_eq!(version_of(b"%PDF-abc\n"), Version(0));
        assert_eq!(version_of(b"%PDF-0.14e1\n"), Version(14));
        assert_eq!(version_of(b"%PDF-1.4e\n"), Version(14));
    }

    #[test]
    fn known_versions() {
        assert!(Version(10).is_known());
        assert!(Version(17).is_known());
        assert!(Version(20).is_known());
        assert!(!Version(18).is_known());
        assert!(!Version(0).is_known());
    }

    #[test]
    fn negative_versions_display_like_c() {
        // MuPDF prints these with "%d.%d", so does nupdf.
        assert_eq!(version_of(b"%PDF--1.4\n").to_string(), "-1.-3");
    }
}
