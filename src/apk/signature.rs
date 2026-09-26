use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt as _;
use std::path::Path;

use ring::signature::{
    RsaParameters, UnparsedPublicKey, RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
    RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY,
};
use sha2::digest::Output;
use sha2::{Digest, Sha256, Sha512};

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
const V3_BLOCK_ID: u32 = 0xf053_68c0;
const V31_BLOCK_ID: u32 = 0x1b93_ad61;
const PROOF_OF_ROTATION_ATTRIBUTE_ID: u32 = 0x3ba0_6f8c;
const PROOF_OF_ROTATION_VERSION: u32 = 1;
const RSA_PKCS1_SHA256: u32 = 0x0103;
const RSA_PKCS1_SHA512: u32 = 0x0104;
const CHUNK_LEN: usize = 1024 * 1024;
const MAX_DIGEST_THREADS: usize = 8;

const DER_INTEGER: u8 = 0x02;
const DER_BIT_STRING: u8 = 0x03;
const DER_SEQUENCE: u8 = 0x30;
const DER_EXPLICIT_VERSION: u8 = 0xa0;
const TBS_FIELDS_BEFORE_SUBJECT_PUBLIC_KEY_INFO: [u8; 5] = [
    DER_INTEGER,
    DER_SEQUENCE,
    DER_SEQUENCE,
    DER_SEQUENCE,
    DER_SEQUENCE,
];

#[derive(Debug)]
pub enum SignatureError {
    Io(io::Error),
    Malformed(&'static str),
    MissingV2Signature,
    UnsupportedAlgorithm,
    UntrustedCertificate(String),
    UntrustedPublicKey,
    CertificateKeyMismatch,
    BadSignature(&'static str),
    RotationProofMismatch,
    ContentDigestMismatch,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Malformed(what) => write!(f, "malformed APK signature data: {what}"),
            Self::MissingV2Signature => write!(f, "no APK Signature Scheme v2 block"),
            Self::UnsupportedAlgorithm => {
                write!(f, "signer has no supported RSASSA-PKCS1-v1_5 signature")
            }
            Self::UntrustedCertificate(digest) => write!(
                f,
                "signed by certificate {digest}, not Roblox Corporation ({ROBLOX_CERTIFICATE_SHA256})"
            ),
            Self::UntrustedPublicKey => write!(f, "signer public key is not Roblox's"),
            Self::CertificateKeyMismatch => {
                write!(f, "v3 signer public key differs from its certificate's")
            }
            Self::BadSignature(record) => write!(f, "{record} signature does not verify"),
            Self::RotationProofMismatch => write!(
                f,
                "proof-of-rotation does not end at the v3 signing certificate"
            ),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningCertificateHistory(Vec<Vec<u8>>);

impl SigningCertificateHistory {
    pub fn certificates(&self) -> &[Vec<u8>] {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn unverified(certificates: Vec<Vec<u8>>) -> Self {
        Self(certificates)
    }
}

pub fn verify_roblox_signature(path: &Path) -> Result<(), SignatureError> {
    verify_v2(&SignedApk::read(path)?)?;
    Ok(())
}

pub fn verify_roblox_signing_history(
    path: &Path,
) -> Result<SigningCertificateHistory, SignatureError> {
    let apk = SignedApk::read(path)?;
    let v2 = verify_v2(&apk)?;
    let v3 = match find_pair(&apk.block, V31_BLOCK_ID)? {
        Some(v31) => Some(v31),
        None => find_pair(&apk.block, V3_BLOCK_ID)?,
    };
    let Some(v3) = v3 else {
        return Ok(SigningCertificateHistory(vec![v2.certificate.to_vec()]));
    };
    let v3 = verify_v3_signer(only_v3_signer(v3)?, v2.certificate)?;
    let contents_match = match v3.algorithm {
        SignatureAlgorithm::RsaPkcs1Sha256 => v3.content_digest == v2.content_digest,
        SignatureAlgorithm::RsaPkcs1Sha512 => {
            content_digest::<Sha512>(&apk.file, &apk.layout)? == v3.content_digest
        }
    };
    if !contents_match {
        return Err(SignatureError::ContentDigestMismatch);
    }
    Ok(SigningCertificateHistory(
        v3.history.into_iter().map(<[u8]>::to_vec).collect(),
    ))
}

struct SignedApk {
    file: File,
    layout: ZipLayout,
    block: Vec<u8>,
}

impl SignedApk {
    fn read(path: &Path) -> Result<Self, SignatureError> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut layout = ZipLayout::locate(&mut file, len)?;
        let block = read_signing_block(&mut file, &mut layout)?;
        Ok(Self {
            file,
            layout,
            block,
        })
    }
}

fn verify_v2(apk: &SignedApk) -> Result<V2Signer<'_>, SignatureError> {
    let v2 = find_pair(&apk.block, V2_BLOCK_ID)?.ok_or(SignatureError::MissingV2Signature)?;
    let mut signers = Cursor::new(Cursor::new(v2).prefixed("signer sequence")?);
    let signer = signers.prefixed("signer")?;
    if !signers.is_empty() {
        return Err(SignatureError::Malformed("more than one signer"));
    }
    let v2 = verify_v2_signer(signer)?;
    if content_digest::<Sha256>(&apk.file, &apk.layout)? != v2.content_digest {
        return Err(SignatureError::ContentDigestMismatch);
    }
    Ok(v2)
}

struct V2Signer<'a> {
    certificate: &'a [u8],
    content_digest: &'a [u8],
}

fn verify_v2_signer(signer: &[u8]) -> Result<V2Signer<'_>, SignatureError> {
    let mut fields = Cursor::new(signer);
    let signed_data = fields.prefixed("signed data")?;
    let signatures = fields.prefixed("signatures")?;
    let public_key = fields.prefixed("public key")?;

    if sha256_hex(public_key) != ROBLOX_PUBLIC_KEY_SHA256 {
        return Err(SignatureError::UntrustedPublicKey);
    }
    let signature = find_algorithm(signatures, RSA_PKCS1_SHA256, "signature")?;
    SignatureAlgorithm::RsaPkcs1Sha256.verify(public_key, signed_data, signature, "v2 signer")?;

    let mut data = Cursor::new(signed_data);
    let digests = data.prefixed("digests")?;
    let mut certificates = Cursor::new(data.prefixed("certificates")?);
    let certificate = certificates.prefixed("certificate")?;
    let certificate_digest = sha256_hex(certificate);
    if certificate_digest != ROBLOX_CERTIFICATE_SHA256 {
        return Err(SignatureError::UntrustedCertificate(certificate_digest));
    }
    Ok(V2Signer {
        certificate,
        content_digest: find_algorithm(digests, RSA_PKCS1_SHA256, "digest")?,
    })
}

fn only_v3_signer(block: &[u8]) -> Result<&[u8], SignatureError> {
    let mut signers = Cursor::new(Cursor::new(block).prefixed("v3 signer sequence")?);
    let signer = signers.prefixed("v3 signer")?;
    if !signers.is_empty() {
        return Err(SignatureError::Malformed("more than one v3 signer"));
    }
    Ok(signer)
}

struct V3Signer<'a> {
    algorithm: SignatureAlgorithm,
    content_digest: &'a [u8],
    history: Vec<&'a [u8]>,
}

fn verify_v3_signer<'a>(
    signer: &'a [u8],
    roblox_certificate: &[u8],
) -> Result<V3Signer<'a>, SignatureError> {
    let mut fields = Cursor::new(signer);
    let signed_data = fields.prefixed("v3 signed data")?;
    let min_sdk = fields.u32("v3 minimum SDK")?;
    let max_sdk = fields.u32("v3 maximum SDK")?;
    let signatures = fields.prefixed("v3 signatures")?;
    let public_key = fields.prefixed("v3 public key")?;

    let algorithm = strongest_algorithm(signatures)?;
    let signature = find_algorithm(signatures, algorithm.id(), "v3 signature")?;
    algorithm.verify(public_key, signed_data, signature, "v3 signer")?;

    let mut data = Cursor::new(signed_data);
    let digests = data.prefixed("v3 digests")?;
    let mut certificates = Cursor::new(data.prefixed("v3 certificates")?);
    let certificate = certificates.prefixed("v3 certificate")?;
    if data.u32("v3 signed minimum SDK")? != min_sdk
        || data.u32("v3 signed maximum SDK")? != max_sdk
    {
        return Err(SignatureError::Malformed(
            "v3 SDK range differs from its signed data",
        ));
    }
    let attributes = data.prefixed("v3 attributes")?;
    if subject_public_key_info(certificate)? != public_key {
        return Err(SignatureError::CertificateKeyMismatch);
    }

    let history = match find_attribute(attributes, PROOF_OF_ROTATION_ATTRIBUTE_ID)? {
        Some(proof) => verify_rotation_proof(proof)?,
        None => vec![certificate],
    };
    let (Some(&first), Some(&last)) = (history.first(), history.last()) else {
        return Err(SignatureError::RotationProofMismatch);
    };
    if last != certificate {
        return Err(SignatureError::RotationProofMismatch);
    }
    if first != roblox_certificate {
        return Err(SignatureError::UntrustedCertificate(sha256_hex(first)));
    }
    Ok(V3Signer {
        algorithm,
        content_digest: find_algorithm(digests, algorithm.id(), "v3 digest")?,
        history,
    })
}

fn verify_rotation_proof(proof: &[u8]) -> Result<Vec<&[u8]>, SignatureError> {
    let mut proof = Cursor::new(proof);
    if proof.u32("proof-of-rotation version")? != PROOF_OF_ROTATION_VERSION {
        return Err(SignatureError::Malformed("proof-of-rotation version"));
    }
    let mut certificates: Vec<&[u8]> = Vec::new();
    let mut signing_parent: Option<(&[u8], u32)> = None;
    while !proof.is_empty() {
        let mut level = Cursor::new(proof.prefixed("proof-of-rotation level")?);
        let signed_data = level.prefixed("proof-of-rotation signed data")?;
        level.u32("proof-of-rotation flags")?;
        let algorithm_for_next = level.u32("proof-of-rotation algorithm")?;
        let signature = level.prefixed("proof-of-rotation signature")?;

        let mut data = Cursor::new(signed_data);
        let certificate = data.prefixed("proof-of-rotation certificate")?;
        let signed_algorithm = data.u32("proof-of-rotation signed algorithm")?;
        if let Some((parent, parent_algorithm)) = signing_parent {
            if signed_algorithm != parent_algorithm {
                return Err(SignatureError::Malformed(
                    "proof-of-rotation algorithm differs from its parent's",
                ));
            }
            SignatureAlgorithm::from_id(parent_algorithm)
                .ok_or(SignatureError::UnsupportedAlgorithm)?
                .verify(
                    subject_public_key_info(parent)?,
                    signed_data,
                    signature,
                    "proof-of-rotation",
                )?;
        }
        if certificates.contains(&certificate) {
            return Err(SignatureError::Malformed(
                "duplicate proof-of-rotation certificate",
            ));
        }
        certificates.push(certificate);
        signing_parent = Some((certificate, algorithm_for_next));
    }
    Ok(certificates)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SignatureAlgorithm {
    RsaPkcs1Sha256,
    RsaPkcs1Sha512,
}

impl SignatureAlgorithm {
    fn from_id(id: u32) -> Option<Self> {
        match id {
            RSA_PKCS1_SHA256 => Some(Self::RsaPkcs1Sha256),
            RSA_PKCS1_SHA512 => Some(Self::RsaPkcs1Sha512),
            _ => None,
        }
    }

    fn id(self) -> u32 {
        match self {
            Self::RsaPkcs1Sha256 => RSA_PKCS1_SHA256,
            Self::RsaPkcs1Sha512 => RSA_PKCS1_SHA512,
        }
    }

    fn verify(
        self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
        record: &'static str,
    ) -> Result<(), SignatureError> {
        let parameters: &'static RsaParameters = match self {
            Self::RsaPkcs1Sha256 => &RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
            Self::RsaPkcs1Sha512 => &RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY,
        };
        UnparsedPublicKey::new(parameters, rsa_public_key(public_key)?)
            .verify(message, signature)
            .map_err(|_| SignatureError::BadSignature(record))
    }
}

fn strongest_algorithm(signatures: &[u8]) -> Result<SignatureAlgorithm, SignatureError> {
    let mut entries = Cursor::new(signatures);
    let mut strongest = None;
    while !entries.is_empty() {
        let mut entry = Cursor::new(entries.prefixed("v3 signature")?);
        strongest = strongest.max(SignatureAlgorithm::from_id(entry.u32("v3 signature")?));
    }
    strongest.ok_or(SignatureError::UnsupportedAlgorithm)
}

fn find_algorithm<'a>(
    sequence: &'a [u8],
    id: u32,
    what: &'static str,
) -> Result<&'a [u8], SignatureError> {
    let mut entries = Cursor::new(sequence);
    while !entries.is_empty() {
        let mut entry = Cursor::new(entries.prefixed(what)?);
        let algorithm = entry.u32(what)?;
        let value = entry.prefixed(what)?;
        if algorithm == id {
            return Ok(value);
        }
    }
    Err(SignatureError::UnsupportedAlgorithm)
}

fn find_attribute(attributes: &[u8], id: u32) -> Result<Option<&[u8]>, SignatureError> {
    let mut entries = Cursor::new(attributes);
    while !entries.is_empty() {
        let mut attribute = Cursor::new(entries.prefixed("v3 attribute")?);
        if attribute.u32("v3 attribute")? == id {
            return Ok(Some(attribute.rest()));
        }
    }
    Ok(None)
}

fn subject_public_key_info(certificate: &[u8]) -> Result<&[u8], SignatureError> {
    const WHAT: &str = "certificate DER";
    let (body, _) = der(certificate, DER_SEQUENCE, WHAT)?;
    let (mut fields, _) = der(body, DER_SEQUENCE, WHAT)?;
    if fields.first() == Some(&DER_EXPLICIT_VERSION) {
        fields = der(fields, DER_EXPLICIT_VERSION, WHAT)?.1;
    }
    for tag in TBS_FIELDS_BEFORE_SUBJECT_PUBLIC_KEY_INFO {
        fields = der(fields, tag, WHAT)?.1;
    }
    let (_, after) = der(fields, DER_SEQUENCE, WHAT)?;
    Ok(&fields[..fields.len() - after.len()])
}

fn rsa_public_key(spki: &[u8]) -> Result<&[u8], SignatureError> {
    const WHAT: &str = "public key DER";
    let (info, _) = der(spki, DER_SEQUENCE, WHAT)?;
    let (_, rest) = der(info, DER_SEQUENCE, WHAT)?;
    let (bits, _) = der(rest, DER_BIT_STRING, WHAT)?;
    match bits.split_first() {
        Some((0, key)) => Ok(key),
        _ => Err(SignatureError::Malformed("public key bit string")),
    }
}

fn der<'a>(
    input: &'a [u8],
    tag: u8,
    what: &'static str,
) -> Result<(&'a [u8], &'a [u8]), SignatureError> {
    let bad = || SignatureError::Malformed(what);
    let (&found, rest) = input.split_first().ok_or_else(bad)?;
    if found != tag {
        return Err(bad());
    }
    let (&first, mut rest) = rest.split_first().ok_or_else(bad)?;
    let len = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return Err(bad());
        }
        let (len_bytes, tail) = rest.split_at(count);
        rest = tail;
        len_bytes
            .iter()
            .fold(0, |acc, &b| (acc << 8) | usize::from(b))
    };
    if rest.len() < len {
        return Err(bad());
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

fn content_digest<D>(file: &File, layout: &ZipLayout) -> Result<Vec<u8>, SignatureError>
where
    D: Digest,
    Output<D>: Send,
{
    let mut eocd = layout.eocd.clone();
    let offset = u32::try_from(layout.signing_block_offset)
        .map_err(|_| SignatureError::Malformed("signing block offset"))?;
    eocd[16..20].copy_from_slice(&offset.to_le_bytes());

    let mut chunks = Vec::new();
    for (start, end) in [
        (0, layout.signing_block_offset),
        (layout.central_directory_offset, layout.eocd_offset),
    ] {
        let mut at = start;
        while at < end {
            let len = (end - at).min(CHUNK_LEN as u64);
            chunks.push((at, len as usize));
            at += len;
        }
    }
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_DIGEST_THREADS);
    let mut digests = std::thread::scope(|scope| {
        let workers = chunks
            .chunks(chunks.len().div_ceil(threads).max(1))
            .map(|group| {
                std::thread::Builder::new()
                    .spawn_scoped(scope, move || digest_chunks::<D>(file, group))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut digests = Vec::with_capacity(chunks.len() + 1);
        for worker in workers {
            let group = worker
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
            digests.extend(group);
        }
        Ok::<_, SignatureError>(digests)
    })?;
    for chunk in eocd.chunks(CHUNK_LEN) {
        digests.push(chunk_digest::<D>(chunk));
    }
    top_digest::<D>(&digests)
}

fn digest_chunks<D: Digest>(
    file: &File,
    chunks: &[(u64, usize)],
) -> Result<Vec<Output<D>>, SignatureError> {
    let mut buffer = vec![0; CHUNK_LEN];
    let mut digests = Vec::with_capacity(chunks.len());
    for &(offset, len) in chunks {
        let chunk = &mut buffer[..len];
        file.read_exact_at(chunk, offset)?;
        digests.push(chunk_digest::<D>(chunk));
    }
    Ok(digests)
}

fn chunk_digest<D: Digest>(chunk: &[u8]) -> Output<D> {
    let mut hasher = D::new();
    hasher.update([0xa5]);
    hasher.update((chunk.len() as u32).to_le_bytes());
    hasher.update(chunk);
    hasher.finalize()
}

fn top_digest<D: Digest>(chunk_digests: &[Output<D>]) -> Result<Vec<u8>, SignatureError> {
    let count =
        u32::try_from(chunk_digests.len()).map_err(|_| SignatureError::Malformed("chunk count"))?;
    let mut top = D::new();
    top.update([0x5a]);
    top.update(count.to_le_bytes());
    for digest in chunk_digests {
        top.update(digest);
    }
    Ok(top.finalize().to_vec())
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

    fn rest(self) -> &'a [u8] {
        self.0
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
    const ECDSA_SHA256: u32 = 0x0201;
    const ROBLOX_ROTATED_CERTIFICATE_SHA256: &str =
        "2bebd189e8d3106401347056c93d045b61e20e22d0c3cbed85474aeb00a3d12a";

    type ErrorCheck = fn(&SignatureError) -> bool;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "eclipse-signature-test-{tag}-{:?}.apk",
            std::thread::current().id()
        ))
    }

    fn verify_bytes(tag: &str, bytes: &[u8]) -> Result<SigningCertificateHistory, SignatureError> {
        let path = temp_path(tag);
        std::fs::write(&path, bytes).expect("write test APK");
        let result = verify_roblox_signing_history(&path);
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

    struct V31Offsets {
        block_id: usize,
        minimum_sdk: usize,
        signer_signature_end: usize,
        proof_of_rotation: std::ops::Range<usize>,
    }

    impl V31Offsets {
        fn of(apk: &Path) -> Self {
            let mut file = File::open(apk).expect("open official APK");
            let len = file.metadata().expect("official APK metadata").len();
            let mut layout = ZipLayout::locate(&mut file, len).expect("official zip layout");
            let block = read_signing_block(&mut file, &mut layout).expect("official signing block");
            let pairs_start = layout.signing_block_offset as usize + 8;
            let file_offset =
                |inner: &[u8]| pairs_start + (inner.as_ptr() as usize - block.as_ptr() as usize);

            let v31 = find_pair(&block, V31_BLOCK_ID)
                .expect("official signing block pairs")
                .expect("official v3.1 block");
            let mut fields = Cursor::new(only_v3_signer(v31).expect("one official v3.1 signer"));
            let signed_data = fields.prefixed("signed data").expect("v3.1 signed data");
            fields.u32("minimum SDK").expect("v3.1 minimum SDK");
            fields.u32("maximum SDK").expect("v3.1 maximum SDK");
            let signatures = fields.prefixed("signatures").expect("v3.1 signatures");

            let mut data = Cursor::new(signed_data);
            data.prefixed("digests").expect("v3.1 digests");
            data.prefixed("certificates").expect("v3.1 certificates");
            data.u32("minimum SDK").expect("v3.1 signed minimum SDK");
            data.u32("maximum SDK").expect("v3.1 signed maximum SDK");
            let attributes = data.prefixed("attributes").expect("v3.1 attributes");
            let proof = find_attribute(attributes, PROOF_OF_ROTATION_ATTRIBUTE_ID)
                .expect("v3.1 attribute list")
                .expect("official proof-of-rotation");

            Self {
                block_id: file_offset(v31) - 4,
                minimum_sdk: file_offset(signed_data) + signed_data.len(),
                signer_signature_end: file_offset(signatures) + signatures.len() - 1,
                proof_of_rotation: file_offset(proof)..file_offset(proof) + proof.len(),
            }
        }
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
    fn official_apks_report_roblox_rotated_signing_history() {
        let Some(apks) = official_apks() else {
            return;
        };
        for apk in apks {
            let history = verify_roblox_signing_history(&apk).expect("official APK verifies");
            let digests: Vec<String> = history
                .certificates()
                .iter()
                .map(|certificate| sha256_hex(certificate))
                .collect();
            assert_eq!(
                digests,
                [ROBLOX_CERTIFICATE_SHA256, ROBLOX_ROTATED_CERTIFICATE_SHA256],
                "{}",
                apk.display()
            );
            let original_key = subject_public_key_info(&history.certificates()[0])
                .expect("the 2014 certificate has a public key");
            assert_eq!(sha256_hex(original_key), ROBLOX_PUBLIC_KEY_SHA256);
        }
    }

    #[test]
    fn official_proof_of_rotation_verifies_and_a_flipped_signature_bit_does_not() {
        let Some(apks) = official_apks() else {
            return;
        };
        let range = V31Offsets::of(&apks[0]).proof_of_rotation;
        let mut proof = vec![0; range.len()];
        File::open(&apks[0])
            .and_then(|file| file.read_exact_at(&mut proof, range.start as u64))
            .expect("read the official proof-of-rotation");

        let digests: Vec<String> = verify_rotation_proof(&proof)
            .expect("the official proof-of-rotation verifies")
            .into_iter()
            .map(sha256_hex)
            .collect();
        assert_eq!(
            digests,
            [ROBLOX_CERTIFICATE_SHA256, ROBLOX_ROTATED_CERTIFICATE_SHA256]
        );

        let last = proof.len() - 1;
        proof[last] ^= 0x01;
        let result = verify_rotation_proof(&proof);
        assert!(
            matches!(
                result,
                Err(SignatureError::BadSignature("proof-of-rotation"))
            ),
            "got {result:?}"
        );
    }

    #[test]
    fn tampered_v31_signer_signature_fails_only_the_history_check() {
        let Some(apks) = official_apks() else {
            return;
        };
        let offsets = V31Offsets::of(&apks[0]);
        let mut bytes = std::fs::read(&apks[0]).expect("read official APK");
        bytes[offsets.signer_signature_end] ^= 0x01;
        let path = temp_path("tampered-v31");
        std::fs::write(&path, &bytes).expect("write test APK");
        let history = verify_roblox_signing_history(&path);
        let v2 = verify_roblox_signature(&path);
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(history, Err(SignatureError::BadSignature("v3 signer"))),
            "got {history:?}"
        );
        assert!(
            v2.is_ok(),
            "the v2 check must not read the v3.1 block: {v2:?}"
        );
    }

    #[test]
    fn a_v31_sdk_range_that_differs_from_its_signed_copy_is_rejected() {
        let Some(apks) = official_apks() else {
            return;
        };
        let offsets = V31Offsets::of(&apks[0]);
        let mut bytes = std::fs::read(&apks[0]).expect("read official APK");
        let signed = le_u32(&bytes, offsets.minimum_sdk).expect("v3.1 minimum SDK");
        bytes[offsets.minimum_sdk..offsets.minimum_sdk + 4]
            .copy_from_slice(&(signed - 1).to_le_bytes());
        let result = verify_bytes("v31-sdk-range", &bytes);
        assert!(
            matches!(
                result,
                Err(SignatureError::Malformed(
                    "v3 SDK range differs from its signed data"
                ))
            ),
            "got {result:?}"
        );
    }

    #[test]
    fn without_a_v31_block_the_v3_signer_alone_is_the_history() {
        let Some(apks) = official_apks() else {
            return;
        };
        let offsets = V31Offsets::of(&apks[0]);
        let mut bytes = std::fs::read(&apks[0]).expect("read official APK");
        bytes[offsets.block_id..offsets.block_id + 4]
            .copy_from_slice(&VERITY_PADDING_BLOCK_ID.to_le_bytes());
        let history = verify_bytes("without-v31", &bytes).expect("the v3.0 signer verifies");
        let digests: Vec<String> = history
            .certificates()
            .iter()
            .map(|certificate| sha256_hex(certificate))
            .collect();
        assert_eq!(digests, [ROBLOX_CERTIFICATE_SHA256]);
    }

    #[test]
    fn proof_of_rotation_structure_errors_precede_signature_checks() {
        let level = |certificate: &[u8], signed_algorithm: u32, next_algorithm: u32| {
            let mut signed_data = length_prefixed(certificate);
            signed_data.extend_from_slice(&signed_algorithm.to_le_bytes());
            let mut level = length_prefixed(&signed_data);
            level.extend_from_slice(&0_u32.to_le_bytes());
            level.extend_from_slice(&next_algorithm.to_le_bytes());
            level.extend_from_slice(&length_prefixed(b"signature"));
            length_prefixed(&level)
        };
        let proof = |version: u32, levels: &[Vec<u8>]| {
            let mut proof = version.to_le_bytes().to_vec();
            for level in levels {
                proof.extend_from_slice(level);
            }
            proof
        };

        let single = proof(1, &[level(b"original", 0, RSA_PKCS1_SHA256)]);
        assert_eq!(
            verify_rotation_proof(&single).expect("one level needs no signature"),
            [b"original".as_slice()]
        );

        let cases: [(&str, Vec<u8>, ErrorCheck); 3] = [
            (
                "version",
                proof(2, &[level(b"original", 0, RSA_PKCS1_SHA256)]),
                |error| {
                    matches!(
                        error,
                        SignatureError::Malformed("proof-of-rotation version")
                    )
                },
            ),
            (
                "algorithm-mismatch",
                proof(
                    1,
                    &[
                        level(b"original", 0, RSA_PKCS1_SHA256),
                        level(b"rotated", RSA_PKCS1_SHA512, 0),
                    ],
                ),
                |error| {
                    matches!(
                        error,
                        SignatureError::Malformed(
                            "proof-of-rotation algorithm differs from its parent's"
                        )
                    )
                },
            ),
            (
                "unsupported-algorithm",
                proof(
                    1,
                    &[
                        level(b"original", 0, ECDSA_SHA256),
                        level(b"rotated", ECDSA_SHA256, 0),
                    ],
                ),
                |error| matches!(error, SignatureError::UnsupportedAlgorithm),
            ),
        ];
        for (tag, bytes, expected) in cases {
            let result = verify_rotation_proof(&bytes);
            assert!(
                result.as_ref().is_err_and(expected),
                "{tag}: got {result:?}"
            );
        }
    }

    #[test]
    fn the_strongest_supported_v3_signature_algorithm_is_chosen() {
        let signatures = |ids: &[u32]| -> Vec<u8> {
            ids.iter()
                .flat_map(|id| {
                    let mut entry = id.to_le_bytes().to_vec();
                    entry.extend_from_slice(&length_prefixed(b"signature"));
                    length_prefixed(&entry)
                })
                .collect()
        };
        assert_eq!(
            strongest_algorithm(&signatures(&[
                RSA_PKCS1_SHA512,
                ECDSA_SHA256,
                RSA_PKCS1_SHA256
            ]))
            .expect("RSA algorithms are supported"),
            SignatureAlgorithm::RsaPkcs1Sha512
        );
        assert!(matches!(
            strongest_algorithm(&signatures(&[ECDSA_SHA256])),
            Err(SignatureError::UnsupportedAlgorithm)
        ));
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
