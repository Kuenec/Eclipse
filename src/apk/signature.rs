use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use ring::signature::{UnparsedPublicKey, RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY};
use sha2::{Digest, Sha256};

const ROBLOX_CERTIFICATE_SHA256: &str =
    "44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477";
const ROBLOX_PUBLIC_KEY_SHA256: &str =
    "ca8dde727caf292fcdad1526b5f3a8a3bd0f69128ace74b1287d4cd58fc2fa3b";

const EOCD_MAGIC: u32 = 0x0605_4b50;
const EOCD_MIN_LEN: usize = 22;
const EOCD_MAX_COMMENT: usize = u16::MAX as usize;
const SIGNING_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";
const SIGNING_BLOCK_FOOTER_LEN: u64 = 24;
const SIGNING_BLOCK_MAX_LEN: u64 = 16 * 1024 * 1024;
const V2_BLOCK_ID: u32 = 0x7109_871a;
const RSA_PKCS1_SHA256: u32 = 0x0103;
const CHUNK_LEN: usize = 1024 * 1024;

#[derive(Debug)]
pub enum SignatureError {
    Io(io::Error),
    Malformed(&'static str),
    MissingV2Signature,
    UnsupportedAlgorithm,
    UntrustedCertificate(String),
    UntrustedPublicKey,
    BadSignature,
    ContentDigestMismatch,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Malformed(what) => write!(f, "malformed APK signature data: {what}"),
            Self::MissingV2Signature => write!(f, "no APK Signature Scheme v2 block"),
            Self::UnsupportedAlgorithm => {
                write!(f, "signer has no RSASSA-PKCS1-v1_5 SHA-256 signature")
            }
            Self::UntrustedCertificate(digest) => write!(
                f,
                "signed by certificate {digest}, not Roblox Corporation ({ROBLOX_CERTIFICATE_SHA256})"
            ),
            Self::UntrustedPublicKey => write!(f, "signer public key is not Roblox's"),
            Self::BadSignature => write!(f, "signature does not verify"),
            Self::ContentDigestMismatch => write!(f, "APK contents were modified after signing"),
        }
    }
}

impl std::error::Error for SignatureError {}

impl From<io::Error> for SignatureError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub fn verify_roblox_signature(path: &Path) -> Result<(), SignatureError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut layout = ZipLayout::locate(&mut file, len)?;
    let block = read_signing_block(&mut file, &mut layout)?;
    let v2 = find_pair(&block, V2_BLOCK_ID)?.ok_or(SignatureError::MissingV2Signature)?;

    let mut signers = Cursor::new(Cursor::new(v2).prefixed("signer sequence")?);
    let signer = signers.prefixed("signer")?;
    if !signers.is_empty() {
        return Err(SignatureError::Malformed("more than one signer"));
    }
    let expected_digest = verify_signer(signer)?;

    if content_digest(&mut file, &layout)? == expected_digest {
        Ok(())
    } else {
        Err(SignatureError::ContentDigestMismatch)
    }
}

fn verify_signer(signer: &[u8]) -> Result<Vec<u8>, SignatureError> {
    let mut fields = Cursor::new(signer);
    let signed_data = fields.prefixed("signed data")?;
    let signatures = fields.prefixed("signatures")?;
    let public_key = fields.prefixed("public key")?;

    if sha256_hex(public_key) != ROBLOX_PUBLIC_KEY_SHA256 {
        return Err(SignatureError::UntrustedPublicKey);
    }
    let signature = find_algorithm(signatures, "signature")?;
    UnparsedPublicKey::new(
        &RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
        rsa_public_key(public_key)?,
    )
    .verify(signed_data, signature)
    .map_err(|_| SignatureError::BadSignature)?;

    let mut data = Cursor::new(signed_data);
    let digests = data.prefixed("digests")?;
    let mut certificates = Cursor::new(data.prefixed("certificates")?);
    let certificate_digest = sha256_hex(certificates.prefixed("certificate")?);
    if certificate_digest != ROBLOX_CERTIFICATE_SHA256 {
        return Err(SignatureError::UntrustedCertificate(certificate_digest));
    }
    Ok(find_algorithm(digests, "digest")?.to_vec())
}

fn find_algorithm<'a>(sequence: &'a [u8], what: &'static str) -> Result<&'a [u8], SignatureError> {
    let mut entries = Cursor::new(sequence);
    while !entries.is_empty() {
        let mut entry = Cursor::new(entries.prefixed(what)?);
        let algorithm = entry.u32(what)?;
        let value = entry.prefixed(what)?;
        if algorithm == RSA_PKCS1_SHA256 {
            return Ok(value);
        }
    }
    Err(SignatureError::UnsupportedAlgorithm)
}

fn rsa_public_key(spki: &[u8]) -> Result<&[u8], SignatureError> {
    let (info, _) = der(spki, 0x30)?;
    let (_, rest) = der(info, 0x30)?;
    let (bits, _) = der(rest, 0x03)?;
    match bits.split_first() {
        Some((0, key)) => Ok(key),
        _ => Err(SignatureError::Malformed("public key bit string")),
    }
}

fn der(input: &[u8], tag: u8) -> Result<(&[u8], &[u8]), SignatureError> {
    const BAD: SignatureError = SignatureError::Malformed("public key DER");
    let (&found, rest) = input.split_first().ok_or(BAD)?;
    if found != tag {
        return Err(BAD);
    }
    let (&first, mut rest) = rest.split_first().ok_or(BAD)?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return Err(BAD);
        }
        let (len_bytes, tail) = rest.split_at(count);
        rest = tail;
        len_bytes
            .iter()
            .fold(0, |acc, &b| (acc << 8) | usize::from(b))
    };
    if rest.len() < len {
        return Err(BAD);
    }
    Ok(rest.split_at(len))
}

struct ZipLayout {
    signing_block_offset: u64,
    central_directory_offset: u64,
    eocd_offset: u64,
    eocd: Vec<u8>,
}

impl ZipLayout {
    fn locate(file: &mut File, len: u64) -> Result<Self, SignatureError> {
        let tail_len = len.min((EOCD_MIN_LEN + EOCD_MAX_COMMENT) as u64);
        let tail_start = len - tail_len;
        let mut tail = vec![0; tail_len as usize];
        file.seek(SeekFrom::Start(tail_start))?;
        file.read_exact(&mut tail)?;

        let eocd_start = (0..=tail.len().saturating_sub(EOCD_MIN_LEN))
            .rev()
            .find(|&at| {
                le_u32(&tail, at) == Some(EOCD_MAGIC)
                    && le_u16(&tail, at + 20).is_some_and(|comment| {
                        at + EOCD_MIN_LEN + usize::from(comment) == tail.len()
                    })
            })
            .ok_or(SignatureError::Malformed("no end of central directory"))?;
        let eocd = tail[eocd_start..].to_vec();
        let eocd_offset = tail_start + eocd_start as u64;
        let central_directory_offset = u64::from(
            le_u32(&eocd, 16).ok_or(SignatureError::Malformed("central directory offset"))?,
        );
        if central_directory_offset > eocd_offset {
            return Err(SignatureError::Malformed("central directory past its end"));
        }
        Ok(Self {
            signing_block_offset: 0,
            central_directory_offset,
            eocd_offset,
            eocd,
        })
    }
}

fn read_signing_block(file: &mut File, layout: &mut ZipLayout) -> Result<Vec<u8>, SignatureError> {
    let footer_start = layout
        .central_directory_offset
        .checked_sub(SIGNING_BLOCK_FOOTER_LEN)
        .ok_or(SignatureError::MissingV2Signature)?;
    let mut footer = [0_u8; SIGNING_BLOCK_FOOTER_LEN as usize];
    file.seek(SeekFrom::Start(footer_start))?;
    file.read_exact(&mut footer)?;
    if &footer[8..] != SIGNING_BLOCK_MAGIC {
        return Err(SignatureError::MissingV2Signature);
    }
    let size = u64::from_le_bytes(footer[..8].try_into().expect("8-byte slice"));
    if !(SIGNING_BLOCK_FOOTER_LEN..=SIGNING_BLOCK_MAX_LEN).contains(&size) {
        return Err(SignatureError::Malformed("signing block size"));
    }
    let start = layout
        .central_directory_offset
        .checked_sub(size + 8)
        .ok_or(SignatureError::Malformed("signing block before file start"))?;
    let mut block = vec![0; (size + 8) as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut block)?;
    if block[..8] != footer[..8] {
        return Err(SignatureError::Malformed(
            "signing block size fields differ",
        ));
    }
    layout.signing_block_offset = start;
    let pairs_end = block.len() - SIGNING_BLOCK_FOOTER_LEN as usize;
    block.truncate(pairs_end);
    block.drain(..8);
    Ok(block)
}

fn find_pair(pairs: &[u8], id: u32) -> Result<Option<&[u8]>, SignatureError> {
    let mut rest = pairs;
    while !rest.is_empty() {
        let len = le_u64(rest, 0).ok_or(SignatureError::Malformed("pair length"))?;
        let len = usize::try_from(len)
            .ok()
            .filter(|&len| len >= 4 && len <= rest.len() - 8)
            .ok_or(SignatureError::Malformed("pair length"))?;
        let pair = &rest[8..8 + len];
        if le_u32(pair, 0) == Some(id) {
            return Ok(Some(&pair[4..]));
        }
        rest = &rest[8 + len..];
    }
    Ok(None)
}

fn content_digest(file: &mut File, layout: &ZipLayout) -> Result<Vec<u8>, SignatureError> {
    let mut eocd = layout.eocd.clone();
    let offset = u32::try_from(layout.signing_block_offset)
        .map_err(|_| SignatureError::Malformed("signing block offset"))?;
    eocd[16..20].copy_from_slice(&offset.to_le_bytes());

    let mut chunk_digests = Vec::new();
    let mut buffer = vec![0; CHUNK_LEN];
    for (start, end) in [
        (0, layout.signing_block_offset),
        (layout.central_directory_offset, layout.eocd_offset),
    ] {
        file.seek(SeekFrom::Start(start))?;
        let mut remaining = end - start;
        while remaining > 0 {
            let chunk = &mut buffer[..remaining.min(CHUNK_LEN as u64) as usize];
            file.read_exact(chunk)?;
            chunk_digests.push(chunk_digest(chunk));
            remaining -= chunk.len() as u64;
        }
    }
    for chunk in eocd.chunks(CHUNK_LEN) {
        chunk_digests.push(chunk_digest(chunk));
    }

    let count =
        u32::try_from(chunk_digests.len()).map_err(|_| SignatureError::Malformed("chunk count"))?;
    let mut top = Sha256::new();
    top.update([0x5a]);
    top.update(count.to_le_bytes());
    for digest in &chunk_digests {
        top.update(digest);
    }
    Ok(top.finalize().to_vec())
}

fn chunk_digest(chunk: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([0xa5]);
    hasher.update((chunk.len() as u32).to_le_bytes());
    hasher.update(chunk);
    hasher.finalize().into()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, SignatureError> {
        let value = le_u32(self.0, 0).ok_or(SignatureError::Malformed(what))?;
        self.0 = &self.0[4..];
        Ok(value)
    }

    fn prefixed(&mut self, what: &'static str) -> Result<&'a [u8], SignatureError> {
        let len = self.u32(what)? as usize;
        if len > self.0.len() {
            return Err(SignatureError::Malformed(what));
        }
        let (value, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(value)
    }
}

fn le_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::path::PathBuf;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    const VERITY_PADDING_BLOCK_ID: u32 = 0x4272_6577;

    type ErrorCheck = fn(&SignatureError) -> bool;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "eclipse-signature-test-{tag}-{:?}.apk",
            std::thread::current().id()
        ))
    }

    fn verify_bytes(tag: &str, bytes: &[u8]) -> Result<(), SignatureError> {
        let path = temp_path(tag);
        std::fs::write(&path, bytes).expect("write test APK");
        let result = verify_roblox_signature(&path);
        std::fs::remove_file(&path).ok();
        result
    }

    fn plain_zip() -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("AndroidManifest.xml", SimpleFileOptions::default())
            .expect("start entry");
        writer.write_all(b"manifest").expect("write entry");
        writer.finish().expect("finish zip").into_inner()
    }

    fn length_prefixed(value: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(value.len())
            .expect("test value fits u32")
            .to_le_bytes()
            .to_vec();
        out.extend_from_slice(value);
        out
    }

    fn pair(id: u32, value: &[u8]) -> Vec<u8> {
        let mut out = (value.len() as u64 + 4).to_le_bytes().to_vec();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(value);
        out
    }

    fn signer(public_key: &[u8]) -> Vec<u8> {
        let mut digest = RSA_PKCS1_SHA256.to_le_bytes().to_vec();
        digest.extend_from_slice(&length_prefixed(&[0x11; 32]));
        let mut signed_data = length_prefixed(&length_prefixed(&digest));
        signed_data.extend_from_slice(&length_prefixed(&length_prefixed(b"certificate")));
        signed_data.extend_from_slice(&length_prefixed(&[]));

        let mut signature = RSA_PKCS1_SHA256.to_le_bytes().to_vec();
        signature.extend_from_slice(&length_prefixed(&[0x22; 128]));

        let mut out = length_prefixed(&signed_data);
        out.extend_from_slice(&length_prefixed(&length_prefixed(&signature)));
        out.extend_from_slice(&length_prefixed(public_key));
        out
    }

    fn v2_block(signers: &[Vec<u8>]) -> Vec<u8> {
        let sequence: Vec<u8> = signers
            .iter()
            .flat_map(|signer| length_prefixed(signer))
            .collect();
        pair(V2_BLOCK_ID, &length_prefixed(&sequence))
    }

    fn central_directory_offset(zip: &[u8]) -> usize {
        let eocd_at = zip.len() - EOCD_MIN_LEN;
        assert_eq!(le_u32(zip, eocd_at), Some(EOCD_MAGIC));
        le_u32(zip, eocd_at + 16).expect("cd offset") as usize
    }

    fn signing_block(pairs: &[u8], header_size: u64, footer_size: u64) -> Vec<u8> {
        let mut block = header_size.to_le_bytes().to_vec();
        block.extend_from_slice(pairs);
        block.extend_from_slice(&footer_size.to_le_bytes());
        block.extend_from_slice(SIGNING_BLOCK_MAGIC);
        block
    }

    fn insert_block(zip: &[u8], block: &[u8]) -> Vec<u8> {
        let cd_offset = central_directory_offset(zip);
        let mut out = zip[..cd_offset].to_vec();
        out.extend_from_slice(block);
        out.extend_from_slice(&zip[cd_offset..]);
        let new_eocd = out.len() - EOCD_MIN_LEN;
        let new_cd = u32::try_from(cd_offset + block.len()).expect("small zip");
        out[new_eocd + 16..new_eocd + 20].copy_from_slice(&new_cd.to_le_bytes());
        out
    }

    fn with_signing_block(zip: &[u8], pairs: &[u8]) -> Vec<u8> {
        let size = pairs.len() as u64 + SIGNING_BLOCK_FOOTER_LEN;
        insert_block(zip, &signing_block(pairs, size, size))
    }

    fn official_apks() -> Option<Vec<PathBuf>> {
        let paths = crate::apk::ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable");
        let Some(paths) = paths else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to verify the official Roblox APKs");
            return None;
        };
        Some(paths.native_split.into_iter().chain([paths.base]).collect())
    }

    #[test]
    fn official_roblox_split_and_base_verify() {
        let Some(apks) = official_apks() else {
            return;
        };
        for apk in apks {
            verify_roblox_signature(&apk)
                .unwrap_or_else(|error| panic!("{} must verify: {error}", apk.display()));
        }
    }

    #[test]
    fn one_flipped_bit_is_a_content_digest_mismatch() {
        let Some(apks) = official_apks() else {
            return;
        };
        let mut bytes = std::fs::read(&apks[0]).expect("read official APK");
        bytes[4096] ^= 0x01;
        let result = verify_bytes("tampered", &bytes);
        assert!(
            matches!(result, Err(SignatureError::ContentDigestMismatch)),
            "got {result:?}"
        );
    }

    #[test]
    fn truncated_official_apk_is_a_typed_error() {
        let Some(apks) = official_apks() else {
            return;
        };
        let mut head = vec![0; 4 * 1024 * 1024];
        File::open(&apks[0])
            .and_then(|mut file| file.read_exact(&mut head))
            .expect("read the start of the official APK");
        let result = verify_bytes("truncated-official", &head);
        assert!(
            matches!(
                result,
                Err(SignatureError::Malformed("no end of central directory"))
            ),
            "got {result:?}"
        );
    }

    #[test]
    fn non_zip_empty_and_unsigned_inputs_are_typed_errors() {
        for (tag, bytes) in [
            ("empty", Vec::new()),
            ("text", b"this is plainly not an APK".to_vec()),
        ] {
            let result = verify_bytes(tag, &bytes);
            assert!(
                matches!(
                    result,
                    Err(SignatureError::Malformed("no end of central directory"))
                ),
                "{tag}: got {result:?}"
            );
        }

        let result = verify_bytes("unsigned", &plain_zip());
        assert!(
            matches!(result, Err(SignatureError::MissingV2Signature)),
            "got {result:?}"
        );

        let missing = temp_path("missing");
        std::fs::remove_file(&missing).ok();
        assert!(matches!(
            verify_roblox_signature(&missing),
            Err(SignatureError::Io(_))
        ));
    }

    #[test]
    fn every_truncation_of_a_signed_layout_is_a_typed_error() {
        let signed = with_signing_block(&plain_zip(), &v2_block(&[signer(b"key")]));
        for len in 0..signed.len() {
            assert!(
                verify_bytes("truncation", &signed[..len]).is_err(),
                "a {len}-byte prefix must not verify"
            );
        }
    }

    #[test]
    fn synthetic_malformed_signing_blocks_are_typed_errors() {
        let zip = plain_zip();
        let pairs = v2_block(&[signer(b"key")]);
        let size = pairs.len() as u64 + SIGNING_BLOCK_FOOTER_LEN;
        let cases: [(&str, Vec<u8>, ErrorCheck); 8] = [
            ("empty-block", with_signing_block(&zip, &[]), |error| {
                matches!(error, SignatureError::MissingV2Signature)
            }),
            (
                "no-v2-pair",
                with_signing_block(&zip, &pair(VERITY_PADDING_BLOCK_ID, &[0; 16])),
                |error| matches!(error, SignatureError::MissingV2Signature),
            ),
            (
                "signer-sequence-overrun",
                with_signing_block(&zip, &pair(V2_BLOCK_ID, &[0xff; 4])),
                |error| matches!(error, SignatureError::Malformed("signer sequence")),
            ),
            (
                "two-signers",
                with_signing_block(&zip, &v2_block(&[signer(b"a"), signer(b"b")])),
                |error| matches!(error, SignatureError::Malformed("more than one signer")),
            ),
            (
                "foreign-key",
                with_signing_block(&zip, &v2_block(&[signer(b"not roblox")])),
                |error| matches!(error, SignatureError::UntrustedPublicKey),
            ),
            (
                "pair-length-overrun",
                with_signing_block(&zip, &u64::MAX.to_le_bytes()),
                |error| matches!(error, SignatureError::Malformed("pair length")),
            ),
            (
                "size-fields-differ",
                insert_block(&zip, &signing_block(&pairs, size - 8, size)),
                |error| matches!(error, SignatureError::Malformed(_)),
            ),
            (
                "undersized-block",
                insert_block(&zip, &signing_block(&[], 16, 16)),
                |error| matches!(error, SignatureError::Malformed("signing block size")),
            ),
        ];
        for (tag, bytes, expected) in cases {
            let result = verify_bytes(tag, &bytes);
            assert!(
                result.as_ref().is_err_and(expected),
                "{tag}: got {result:?}"
            );
        }
    }

    #[test]
    fn mutated_signing_blocks_never_panic() {
        let zip = plain_zip();
        let pairs = v2_block(&[signer(b"key")]);
        let signed = with_signing_block(&zip, &pairs);
        let block_start = central_directory_offset(&zip);
        let block_end = central_directory_offset(&signed);
        for index in block_start..block_end {
            for value in [0x00, 0x01, 0x7f, 0xff] {
                let mut mutated = signed.clone();
                mutated[index] = value;
                assert!(verify_bytes("mutation", &mutated).is_err());
            }
        }
    }
}
