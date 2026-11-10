// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum

use std::fmt;

/// Identifies a document by its bytes: the first 128 bits of its BLAKE3 hash.
///
/// Every address nupdf hands out (span ids, search hits) is only meaningful
/// together with the `DocId` of the file it came from, so agents can detect
/// when a file changed underneath them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DocId([u8; 16]);

impl DocId {
    pub fn of(bytes: &[u8]) -> Self {
        Self::from_hash(&blake3::hash(bytes))
    }

    /// The id of the bytes `hash` is the BLAKE3 hash of.
    pub fn from_hash(hash: &blake3::Hash) -> Self {
        let mut id = [0u8; 16];
        id.copy_from_slice(&hash.as_bytes()[..16]);
        DocId(id)
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("b3:")?;
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_bytes_same_id_different_bytes_different_id() {
        assert_eq!(DocId::of(b"%PDF-1.7"), DocId::of(b"%PDF-1.7"));
        assert_ne!(DocId::of(b"%PDF-1.7"), DocId::of(b"%PDF-1.6"));
    }

    #[test]
    fn displays_as_prefixed_hex() {
        let id = DocId::of(b"").to_string();
        // BLAKE3 of the empty input starts af1349b9f5f9a1a6.
        assert_eq!(id, "b3:af1349b9f5f9a1a6a0404dea36dcc949");
        assert_eq!(id.len(), 3 + 32);
    }
}
