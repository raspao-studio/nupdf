// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 hum
// Derived from MuPDF source/pdf/pdf-form.c (pdf_signature_byte_range,
// validate_certificate_data, validate_byte_ranges,
// pdf_signature_incremental_change_since_signing, pdf_signature_contents),
// source/pdf/pdf-signature.c (pdf_check_digest,
// pdf_signature_format_distinguished_name, pdf_signature_error_description)
// and source/helpers/pkcs7/pkcs7-openssl.c (check_digest, get_signatory),
// Copyright (C) 2004-2026 Artifex Software, Inc.

//! Signatures: the bytes a signature covers, whether the file has grown
//! since, whether the digest it signed is the digest of those bytes, and
//! who signed.
//!
//! MuPDF checks the digest with OpenSSL's `PKCS7_signatureVerify`: the
//! signed attributes' messageDigest against the digest of the signed bytes,
//! then the signer's signature. nupdf does the same for RSA signers
//! (PKCS #1 v1.5); a signer with another kind of key is "Unknown error."
//! here, where OpenSSL would check it (DECISIONS D47).

use crate::der::{self, Tlv};
use crate::document::Document;
use crate::form;
use crate::object::Object;

/// Elements in one DER constructed value we look at, at most.
const MAX_ELEMENTS: usize = 4096;

pub const OK: &str = "OK";
pub const NO_SIGNATURES: &str = "No signatures.";
pub const DIGEST_FAILURE: &str = "Signature invalidated by change to document.";
pub const UNKNOWN: &str = "Unknown error.";

fn value(doc: &Document, field: &Object) -> Object {
    form::inheritable(doc, field, b"V")
}

/// `pdf_signature_byte_range`: the (offset, length) pairs of /V /ByteRange,
/// each inside the file.
pub fn byte_range(doc: &Document, field: &Object) -> Result<Vec<(i64, i64)>, String> {
    let v = value(doc, field);
    let br = doc.resolve(&doc.dict_get(&v, b"ByteRange"));
    let a = br.as_array().unwrap_or(&[]);
    let size = doc.data().len() as i64;
    let int = |i: usize| a.get(i).map_or(0, |v| doc.resolve(v).to_int());
    (0..a.len() / 2)
        .map(|i| {
            let (offset, length) = (i64::from(int(2 * i)), i64::from(int(2 * i + 1)));
            if offset < 0 || offset > size {
                Err("offset of signature byte range outside of file".into())
            } else if length < 0 {
                Err("length of signature byte range negative".into())
            } else if offset + length > size {
                Err("signature byte range extends past end of file".into())
            } else {
                Ok((offset, length))
            }
        })
        .collect()
}

fn is_white(c: u8) -> bool {
    matches!(c, 0 | 9 | 10 | 12 | 13 | 32)
}

/// `validate_certificate_data`: a gap between the ranges holds only the
/// signature, as `<hex>` with white space around.
fn validate_hole(hole: &[u8]) -> Result<(), String> {
    let mut it = hole.iter().copied().peekable();
    let mut c = it.next();
    while c.is_some_and(is_white) {
        c = it.next();
    }
    if c == Some(b'<') {
        c = it.next();
    }
    while c.is_some_and(|c| c.is_ascii_hexdigit() || is_white(c)) {
        c = it.next();
    }
    if c == Some(b'>') {
        c = it.next();
    }
    while c.is_some_and(is_white) {
        c = it.next();
    }
    if c.is_some() {
        return Err("signature certificate data contains invalid character".into());
    }
    Ok(())
}

/// `pdf_signature_hash_bytes`: the bytes the ranges cover, after checking
/// that every gap before a range holds only the signature.
pub fn signed_bytes(doc: &Document, field: &Object) -> Result<Vec<u8>, String> {
    let ranges = byte_range(doc, field)?;
    let data = doc.data();
    let mut sorted = ranges.clone();
    sorted.sort_by_key(|r| r.0);
    let mut offset = 0i64;
    for &(start, length) in &sorted {
        if start > offset {
            validate_hole(&data[offset as usize..start as usize])?;
        }
        offset = offset.max(start + length);
    }
    let mut out = Vec::new();
    for (start, length) in ranges {
        out.extend_from_slice(&data[start as usize..(start + length) as usize]);
    }
    Ok(out)
}

/// `pdf_signature_incremental_change_since_signing`: whether the file goes
/// on past the end of the last range.
pub fn changed_since_signing(doc: &Document, field: &Object) -> Result<bool, String> {
    if !form::is_signed(doc, field) {
        return Ok(false);
    }
    let ranges = byte_range(doc, field)?;
    Ok(ranges
        .last()
        .is_some_and(|&(o, l)| doc.data().len() as i64 > o + l))
}

/// `pdf_signature_contents`: the /Contents bytes of the signature
/// dictionary, read from the file without decrypting it (the signature is
/// stored in the clear). A dictionary that is not an object of its own in
/// the file has none.
pub fn contents(doc: &Document, field: &Object) -> Vec<u8> {
    let Object::Ref(r) = value(doc, field) else {
        return Vec::new();
    };
    let Some(v) = doc.load_unencrypted(r.num) else {
        return Vec::new();
    };
    let c = v.as_dict().and_then(|d| d.get(b"Contents")).cloned();
    match c.map(|c| doc.resolve(&c)) {
        Some(Object::String(s)) => s.to_vec(),
        _ => Vec::new(),
    }
}

/// The /ByteRange numbers as given, in pairs.
pub fn raw_byte_range(doc: &Document, field: &Object) -> Vec<(i64, i64)> {
    let v = value(doc, field);
    let br = doc.resolve(&doc.dict_get(&v, b"ByteRange"));
    let a = br.as_array().unwrap_or(&[]);
    let int = |i: usize| a.get(i).map_or(0, |v| i64::from(doc.resolve(v).to_int()));
    (0..a.len() / 2)
        .map(|i| (int(2 * i), int(2 * i + 1)))
        .collect()
}

/// What a PKCS #7 signature holds that the checks need.
struct SignedData<'a> {
    certificates: Vec<Tlv<'a>>,
    signers: Vec<Signer<'a>>,
}

struct Signer<'a> {
    issuer: Tlv<'a>,
    serial: &'a [u8],
    digest_algorithm: String,
    /// The signed attributes, and their messageDigest.
    signed_attrs: Option<Tlv<'a>>,
    message_digest: Option<&'a [u8]>,
    signature_algorithm: String,
    signature: &'a [u8],
}

fn seq<'a>(t: &Tlv<'a>) -> Result<Vec<Tlv<'a>>, String> {
    der::children(t.value, MAX_ELEMENTS)
}

/// `d2i_PKCS7` far enough for the checks: a ContentInfo of type
/// signedData, its certificates and its signer infos.
fn parse(sig: &[u8]) -> Result<SignedData<'_>, String> {
    let (ci, _) = der::expect(sig, der::SEQUENCE)?;
    let ci = seq(&ci)?;
    if ci.first().map(|t| der::oid(t.value)).as_deref() != Some("1.2.840.113549.1.7.2") {
        return Err("not PKCS #7 signed data".into());
    }
    let wrapped = ci
        .get(1)
        .filter(|t| t.tag == der::context(0))
        .ok_or("no content")?;
    let (sd, _) = der::expect(wrapped.value, der::SEQUENCE)?;
    let sd = seq(&sd)?;
    // version, digestAlgorithms, contentInfo, then the optional parts.
    let mut certificates = Vec::new();
    let mut signer_infos = None;
    for t in sd.iter().skip(3) {
        match t.tag {
            0xa0 => certificates = der::children(t.value, MAX_ELEMENTS)?,
            der::SET => signer_infos = Some(*t),
            _ => {}
        }
    }
    let mut signers = Vec::new();
    for si in seq(&signer_infos.ok_or("no signer infos")?)? {
        let f = seq(&si)?;
        let ias = f
            .get(1)
            .filter(|t| t.tag == der::SEQUENCE)
            .ok_or("no issuer and serial")?;
        let ias = seq(ias)?;
        let (issuer, serial) = match (ias.first(), ias.get(1)) {
            (Some(i), Some(s)) if s.tag == der::INTEGER => (*i, s.value),
            _ => return Err("bad issuer and serial".into()),
        };
        let alg = f.get(2).ok_or("no digest algorithm")?;
        let digest_algorithm = seq(alg)?
            .first()
            .map(|t| der::oid(t.value))
            .unwrap_or_default();
        let mut message_digest = None;
        let signed_attrs = f.get(3).filter(|t| t.tag == der::context(0)).copied();
        if let Some(attrs) = &signed_attrs {
            for attr in der::children(attrs.value, MAX_ELEMENTS)? {
                let a = seq(&attr)?;
                if a.first().map(|t| der::oid(t.value)).as_deref() == Some("1.2.840.113549.1.9.4")
                    && let Some(values) = a.get(1)
                    && let Some(v) = seq(values)?.first()
                    && v.tag == der::OCTET_STRING
                {
                    message_digest = Some(v.value);
                }
            }
        }
        let rest = &f[3 + usize::from(signed_attrs.is_some())..];
        let signature_algorithm = rest
            .first()
            .and_then(|a| seq(a).ok()?.first().map(|t| der::oid(t.value)))
            .unwrap_or_default();
        let signature = rest
            .get(1)
            .filter(|t| t.tag == der::OCTET_STRING)
            .map_or(&[][..], |t| t.value);
        signers.push(Signer {
            issuer,
            serial,
            digest_algorithm,
            signed_attrs,
            message_digest,
            signature_algorithm,
            signature,
        });
    }
    Ok(SignedData {
        certificates,
        signers,
    })
}

/// What a certificate says of itself: issuer, serial number, subject and
/// public key info.
struct Cert<'a> {
    issuer: Tlv<'a>,
    serial: &'a [u8],
    subject: Tlv<'a>,
    key: Tlv<'a>,
}

fn cert_fields<'a>(cert: &Tlv<'a>) -> Option<Cert<'a>> {
    let tbs = *seq(cert).ok()?.first()?;
    let f = seq(&tbs).ok()?;
    // An explicit version comes first when there is one.
    let skip = usize::from(f.first()?.tag == der::context(0));
    Some(Cert {
        serial: f.get(skip)?.value,
        issuer: *f.get(skip + 2)?,
        subject: *f.get(skip + 4)?,
        key: *f.get(skip + 5)?,
    })
}

/// A name as `X509_NAME_cmp` compares them: each entry's type and its
/// value as text, white space trimmed and collapsed, ASCII lowercased.
fn canonical(name: &Tlv) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for rdn in seq(name).unwrap_or_default() {
        for atv in seq(&rdn).unwrap_or_default() {
            let a = seq(&atv).unwrap_or_default();
            if let (Some(t), Some(v)) = (a.first(), a.get(1)) {
                let text = der::text(v).to_ascii_lowercase();
                let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                out.push((der::oid(t.value), text));
            }
        }
    }
    out
}

fn integer(v: &[u8]) -> &[u8] {
    let lead = v.iter().take_while(|&&b| b == 0).count();
    &v[lead.min(v.len().saturating_sub(1))..]
}

/// `X509_find_by_issuer_and_serial`.
fn signer_cert<'a>(sd: &SignedData<'a>, s: &Signer) -> Option<Cert<'a>> {
    let issuer = canonical(&s.issuer);
    sd.certificates
        .iter()
        .filter_map(cert_fields)
        .find(|c| integer(c.serial) == integer(s.serial) && canonical(&c.issuer) == issuer)
}

/// The RSA key (modulus, exponent) a certificate's public key info holds.
fn rsa_key<'a>(cert: &Cert<'a>) -> Option<(&'a [u8], &'a [u8])> {
    let spki = seq(&cert.key).ok()?;
    let alg = seq(spki.first()?).ok()?;
    if der::oid(alg.first()?.value) != "1.2.840.113549.1.1.1" {
        return None;
    }
    let bits = spki.get(1)?.value;
    // A BIT STRING: the count of unused bits, then the key.
    let (key, _) = der::expect(bits.get(1..)?, der::SEQUENCE).ok()?;
    let key = seq(&key).ok()?;
    Some((key.first()?.value, key.get(1)?.value))
}

/// `PKCS7_signatureVerify`'s last step for an RSA signer: the signature
/// carries a DigestInfo of `digest`.
fn signature_ok(cert: &Cert, s: &Signer, digest: &[u8]) -> bool {
    if !matches!(
        s.signature_algorithm.as_str(),
        "1.2.840.113549.1.1.1"
            | "1.2.840.113549.1.1.5"
            | "1.2.840.113549.1.1.11"
            | "1.2.840.113549.1.1.12"
            | "1.2.840.113549.1.1.13"
            | "1.2.840.113549.1.1.14"
            | "1.2.840.113549.1.1.4"
    ) {
        return false;
    }
    let Some((n, e)) = rsa_key(cert) else {
        return false;
    };
    let Some(info) = crate::rsa::recover(n, e, s.signature) else {
        return false;
    };
    let Ok((info, rest)) = der::expect(&info, der::SEQUENCE) else {
        return false;
    };
    let f = seq(&info).unwrap_or_default();
    rest.is_empty()
        && f.len() == 2
        && f[1].tag == der::OCTET_STRING
        && f[1].value == digest
        && seq(&f[0])
            .ok()
            .and_then(|a| a.first().map(|t| der::oid(t.value)))
            .is_some_and(|alg| alg == s.digest_algorithm)
}

fn digest(algorithm: &str, data: &[u8]) -> Option<Vec<u8>> {
    use sha2::Digest;
    Some(match algorithm {
        "1.3.14.3.2.26" => sha1::Sha1::digest(data).to_vec(),
        "2.16.840.1.101.3.4.2.1" => sha2::Sha256::digest(data).to_vec(),
        "2.16.840.1.101.3.4.2.2" => sha2::Sha384::digest(data).to_vec(),
        "2.16.840.1.101.3.4.2.3" => sha2::Sha512::digest(data).to_vec(),
        "2.16.840.1.101.3.4.2.4" => sha2::Sha224::digest(data).to_vec(),
        "1.2.840.113549.2.5" => md5::Md5::digest(data).to_vec(),
        _ => return None,
    })
}

/// `pdf_check_digest`, as nupdf checks it: each signer's signed digest
/// against the digest of the signed bytes, in MuPDF's words
/// (`pdf_signature_error_description`). An error is a range that cannot be
/// read, as MuPDF throws.
pub fn check_digest(doc: &Document, field: &Object) -> Result<&'static str, String> {
    if !form::is_signed(doc, field) {
        return Ok("Not signed.");
    }
    let sig = contents(doc, field);
    let bytes = signed_bytes(doc, field)?;
    let Ok(sd) = parse(&sig) else {
        return Ok(UNKNOWN);
    };
    if sd.signers.is_empty() {
        return Ok(NO_SIGNATURES);
    }
    for s in &sd.signers {
        let Some(cert) = signer_cert(&sd, s) else {
            return Ok(UNKNOWN);
        };
        let Some(data_digest) = digest(&s.digest_algorithm, &bytes) else {
            return Ok(UNKNOWN);
        };
        // With signed attributes, the signature is over them, and they
        // carry the data's digest; without, it is over the data's digest.
        let signed_digest = match &s.signed_attrs {
            Some(attrs) => {
                if s.message_digest != Some(data_digest.as_slice()) {
                    return Ok(DIGEST_FAILURE);
                }
                // The attributes are signed as a SET, not as [0].
                let mut der = attrs.raw.to_vec();
                der[0] = der::SET;
                match digest(&s.digest_algorithm, &der) {
                    Some(d) => d,
                    None => return Ok(UNKNOWN),
                }
            }
            None => data_digest,
        };
        if !signature_ok(&cert, s, &signed_digest) {
            return Ok(UNKNOWN);
        }
    }
    Ok(OK)
}

/// `pdf_signature_get_signatory` as `PDFWidget.getSignatory` formats it:
/// the first signer's certificate subject as
/// `cn=..., o=..., ou=..., email=..., c=...`, or None where the signature
/// cannot be read or names no certificate it holds.
pub fn signatory(doc: &Document, field: &Object) -> Option<String> {
    if !form::is_signed(doc, field) {
        return None;
    }
    signatory_of(&contents(doc, field))
}

/// The signatory a PKCS #7 signature names, as `signatory` gives it.
pub fn signatory_of(sig: &[u8]) -> Option<String> {
    let sd = parse(sig).ok()?;
    let subject = signer_cert(&sd, sd.signers.first()?)?.subject;
    // X509_NAME_get_index_by_NID: the first entry of each kind.
    let entry = |oid: &str| -> String {
        for rdn in seq(&subject).unwrap_or_default() {
            for atv in seq(&rdn).unwrap_or_default() {
                let a = seq(&atv).unwrap_or_default();
                if a.first().map(|t| der::oid(t.value)).as_deref() == Some(oid)
                    && let Some(v) = a.get(1)
                {
                    // ASN1_STRING_get0_data read as a C string.
                    let raw = v.value;
                    let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
                    return String::from_utf8_lossy(raw).into_owned();
                }
            }
        }
        String::new()
    };
    Some(format!(
        "cn={}, o={}, ou={}, email={}, c={}",
        entry("2.5.4.3"),
        entry("2.5.4.10"),
        entry("2.5.4.11"),
        entry("1.2.840.113549.1.9.1"),
        entry("2.5.4.6")
    ))
}

/// Every signed signature field on the pages, in page order: its name and
/// what `check_digest` says of it.
pub fn summary(doc: &Document) -> Vec<(String, String)> {
    let Ok(count) = doc.page_count() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..count {
        let Ok(page) = doc.page(i) else { continue };
        for w in crate::annot::page_annots(doc, &page).1 {
            if !form::is_signed(doc, &w) {
                continue;
            }
            let name = form::name(doc, &w).unwrap_or_default();
            let status = match check_digest(doc, &w) {
                Ok(s) => s.to_string(),
                Err(e) => e,
            };
            out.push((name, status));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gap_holds_only_the_signature() {
        assert!(validate_hole(b"<0aFF \n 12>").is_ok());
        assert!(validate_hole(b"  <abc>  ").is_ok());
        assert!(validate_hole(b"0123").is_ok());
        assert!(validate_hole(b"").is_ok());
        assert!(validate_hole(b"<12> /X").is_err());
        assert!(validate_hole(b"<12g>").is_err());
    }

    #[test]
    fn byte_ranges_must_lie_in_the_file() {
        let doc = crate::testutil::doc_of(&[
            "<</Type/Catalog/Pages 2 0 R>>",
            "<</Type/Pages/Kids[]/Count 0>>",
            "<</FT/Sig/V<</ByteRange[-1 5]>>>>",
            "<</FT/Sig/V<</ByteRange[0 -5]>>>>",
            "<</FT/Sig/V<</ByteRange[0 99999]>>>>",
            "<</FT/Sig/V 8 0 R>>",
            "<</FT/Tx/V(text)>>",
            "<</Type/Sig/ByteRange[0 10]/Contents<3000>>>",
        ]);
        let r = |n| Object::Ref(crate::object::ObjRef::new(n, 0));
        assert_eq!(
            byte_range(&doc, &r(3)),
            Err("offset of signature byte range outside of file".into())
        );
        assert_eq!(
            byte_range(&doc, &r(4)),
            Err("length of signature byte range negative".into())
        );
        assert_eq!(
            byte_range(&doc, &r(5)),
            Err("signature byte range extends past end of file".into())
        );
        assert!(check_digest(&doc, &r(5)).is_err());
        // A direct signature dictionary has no /Contents to read.
        assert!(contents(&doc, &r(3)).is_empty());
        // Contents that are no PKCS #7 data.
        assert_eq!(contents(&doc, &r(6)), [0x30, 0x00]);
        assert_eq!(check_digest(&doc, &r(6)), Ok(UNKNOWN));
        assert_eq!(signatory(&doc, &r(6)), None);
        assert_eq!(changed_since_signing(&doc, &r(6)), Ok(true));
        // Not a signature at all.
        assert_eq!(check_digest(&doc, &r(7)), Ok("Not signed."));
        assert_eq!(changed_since_signing(&doc, &r(7)), Ok(false));
        assert_eq!(signatory(&doc, &r(7)), None);
        assert!(summary(&doc).is_empty());
    }
}
