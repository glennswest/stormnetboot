//! STORMSIG signature verification.
//!
//! Every byte this server hands a booting machine is content it will execute
//! as its kernel, so the boot pallet's signature is checked before anything is
//! served from it. The format is defined by stormblock-registry
//! (`docs/signing.md`, `src/signing.rs`); this is the read side of it.
//!
//! The statement is a fixed 64-byte layout with no parser, deliberately, so
//! that a verifier with no JSON is possible before a kernel exists. We are not
//! that constrained here, but we verify the same bytes the same way.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

/// OCI artifactType of a signature referrer.
pub const SIGNATURE_ARTIFACT_TYPE: &str = "application/vnd.stormblock.signature.v1+json";

const MAGIC: &[u8; 8] = b"STORMSIG";
const STATEMENT_LEN: usize = 64;
const STATEMENT_VERSION: u8 = 1;

/// Byte offsets within the statement.
mod off {
    pub const VERSION: usize = 8;
    pub const SUBJECT_KIND: usize = 9;
    pub const DIGEST: usize = 16;
    pub const DIGEST_END: usize = 48;
    pub const SIGNED_AT: usize = 48;
    pub const SIGNED_AT_END: usize = 56;
}

/// What the signature covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectKind {
    Other,
    Pallet,
    Stack,
}

impl SubjectKind {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Pallet,
            2 => Self::Stack,
            _ => Self::Other,
        }
    }
}

/// The signature document, stored as the *config blob* of the signature
/// referrer manifest — not as a layer, so verifying costs one fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureDoc {
    /// Digest of the manifest that was signed, `sha256:…`.
    pub subject: String,
    pub subject_kind: String,
    /// 64 hex chars.
    pub public_key: String,
    /// First 16 hex chars of `public_key`.
    pub key_id: String,
    /// 128 hex chars — 64 raw bytes.
    pub signature: String,
    pub signed_at: u64,
    /// 128 hex chars — the 64-byte statement, verbatim as signed.
    pub statement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Statement was not 64 bytes.
    StatementLength(usize),
    BadMagic,
    /// A statement version this reader does not understand. Refused rather
    /// than guessed at — a newer version may mean something different.
    UnsupportedVersion(u8),
    NotHex(&'static str),
    /// Public key was not 32 bytes, or is not a valid Ed25519 point.
    BadPublicKey,
    /// Signature was not 64 bytes.
    BadSignature,
    /// The statement does not name the manifest we asked about. This is the
    /// check that stops a valid signature for *something else* being replayed
    /// onto this pallet.
    SubjectMismatch { signed: String, expected: String },
    /// The key is not in our trusted set.
    Untrusted { key_id: String },
    /// Ed25519 said no.
    Invalid,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StatementLength(n) => write!(f, "statement is {n} bytes, expected 64"),
            Self::BadMagic => write!(f, "statement magic is not STORMSIG"),
            Self::UnsupportedVersion(v) => {
                write!(f, "statement version {v} is newer than this reader")
            }
            Self::NotHex(field) => write!(f, "{field} is not valid hex"),
            Self::BadPublicKey => write!(f, "public key is not a valid Ed25519 key"),
            Self::BadSignature => write!(f, "signature is not 64 bytes"),
            Self::SubjectMismatch { signed, expected } => write!(
                f,
                "signature covers {signed} but we are verifying {expected}"
            ),
            Self::Untrusted { key_id } => write!(f, "key {key_id} is not trusted"),
            Self::Invalid => write!(f, "signature does not verify"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Parsed view of a statement's fixed fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub subject_hex: String,
    pub subject_kind: SubjectKind,
    pub signed_at: u64,
}

/// Parse the 64-byte statement, refusing anything it does not fully understand.
pub fn parse_statement(bytes: &[u8]) -> Result<Statement, VerifyError> {
    if bytes.len() != STATEMENT_LEN {
        return Err(VerifyError::StatementLength(bytes.len()));
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(VerifyError::BadMagic);
    }
    let version = bytes[off::VERSION];
    if version != STATEMENT_VERSION {
        return Err(VerifyError::UnsupportedVersion(version));
    }

    let mut signed_at = [0u8; 8];
    signed_at.copy_from_slice(&bytes[off::SIGNED_AT..off::SIGNED_AT_END]);

    Ok(Statement {
        subject_hex: hex::encode(&bytes[off::DIGEST..off::DIGEST_END]),
        subject_kind: SubjectKind::from_u8(bytes[off::SUBJECT_KIND]),
        signed_at: u64::from_le_bytes(signed_at),
    })
}

/// Strip an optional `sha256:` prefix and lowercase, so callers can pass a
/// digest in either of the forms the registry uses.
fn normalise_digest(digest: &str) -> String {
    digest
        .trim()
        .trim_start_matches("sha256:")
        .to_ascii_lowercase()
}

/// Verify a signature document against the manifest digest it should cover.
///
/// The order matters and mirrors the registry's own verifier: the key must be
/// trusted and the statement must *bind this subject* before the signature
/// maths is consulted. A cryptographically valid signature over a different
/// pallet is exactly the attack this ordering refuses.
pub fn verify(
    doc: &SignatureDoc,
    subject_digest: &str,
    trusted_keys: &[String],
) -> Result<Statement, VerifyError> {
    let key_id = doc.key_id.to_ascii_lowercase();
    let public_key = doc.public_key.to_ascii_lowercase();

    let trusted = trusted_keys.iter().any(|t| {
        let t = t.trim().to_ascii_lowercase();
        !t.is_empty() && (t == public_key || t == key_id)
    });
    if !trusted {
        return Err(VerifyError::Untrusted { key_id });
    }

    let statement_bytes =
        hex::decode(doc.statement.trim()).map_err(|_| VerifyError::NotHex("statement"))?;
    let statement = parse_statement(&statement_bytes)?;

    let expected = normalise_digest(subject_digest);
    if statement.subject_hex != expected {
        return Err(VerifyError::SubjectMismatch {
            signed: statement.subject_hex,
            expected,
        });
    }

    let key_bytes = hex::decode(&public_key).map_err(|_| VerifyError::NotHex("public_key"))?;
    let key_arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| VerifyError::BadPublicKey)?;
    let key = VerifyingKey::from_bytes(&key_arr).map_err(|_| VerifyError::BadPublicKey)?;

    let sig_bytes =
        hex::decode(doc.signature.trim()).map_err(|_| VerifyError::NotHex("signature"))?;
    let sig_arr: [u8; 64] = sig_bytes.try_into().map_err(|_| VerifyError::BadSignature)?;
    let signature = Signature::from_bytes(&sig_arr);

    // Verified over the statement bytes exactly as stored, never a rebuilt
    // statement: re-deriving them would verify our own assumptions instead of
    // what the signer actually signed.
    //
    // `verify_strict` rather than `verify`: it additionally rejects small-order
    // keys. Trusted-key matching above is the real gate, so this only removes
    // a degenerate case, and a boot chain is the right place to spend that.
    key.verify_strict(&statement_bytes, &signature)
        .map_err(|_| VerifyError::Invalid)?;

    Ok(statement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    struct Signed {
        doc: SignatureDoc,
        subject: String,
        public_key: String,
    }

    /// Build a real signature the way the registry does, so these tests prove
    /// interoperability with the documented format rather than with itself.
    fn sign_subject(seed: u8, subject_raw: [u8; 32], kind: u8, signed_at: u64) -> Signed {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let public_key = hex::encode(key.verifying_key().to_bytes());

        let mut stmt = [0u8; STATEMENT_LEN];
        stmt[..8].copy_from_slice(MAGIC);
        stmt[off::VERSION] = STATEMENT_VERSION;
        stmt[off::SUBJECT_KIND] = kind;
        stmt[off::DIGEST..off::DIGEST_END].copy_from_slice(&subject_raw);
        stmt[off::SIGNED_AT..off::SIGNED_AT_END].copy_from_slice(&signed_at.to_le_bytes());

        let signature = key.sign(&stmt);
        let subject = hex::encode(subject_raw);

        Signed {
            doc: SignatureDoc {
                subject: format!("sha256:{subject}"),
                subject_kind: "pallet".into(),
                key_id: public_key[..16].to_owned(),
                public_key: public_key.clone(),
                signature: hex::encode(signature.to_bytes()),
                signed_at,
                statement: hex::encode(stmt),
            },
            subject,
            public_key,
        }
    }

    #[test]
    fn verifies_a_well_formed_signature() {
        let s = sign_subject(7, [0xab; 32], 1, 1_756_000_000);
        let statement = verify(&s.doc, &s.subject, &[s.public_key.clone()]).unwrap();

        assert_eq!(statement.subject_hex, s.subject);
        assert_eq!(statement.subject_kind, SubjectKind::Pallet);
        assert_eq!(statement.signed_at, 1_756_000_000);
    }

    #[test]
    fn accepts_the_subject_with_or_without_the_sha256_prefix() {
        let s = sign_subject(7, [0xcd; 32], 1, 1);
        assert!(verify(&s.doc, &s.subject, &[s.public_key.clone()]).is_ok());
        assert!(verify(&s.doc, &format!("sha256:{}", s.subject), &[s.public_key]).is_ok());
    }

    #[test]
    fn trusts_by_key_id_as_well_as_full_key() {
        let s = sign_subject(9, [0x11; 32], 1, 1);
        let key_id = s.doc.key_id.clone();
        assert!(verify(&s.doc, &s.subject, &[key_id]).is_ok());
    }

    #[test]
    fn refuses_an_untrusted_key_before_anything_else() {
        let s = sign_subject(7, [0xab; 32], 1, 1);
        let err = verify(&s.doc, &s.subject, &["deadbeef".into()]).unwrap_err();
        assert!(matches!(err, VerifyError::Untrusted { .. }));
    }

    #[test]
    fn refuses_a_valid_signature_for_a_different_pallet() {
        // The replay case: the signature verifies, the key is trusted, but it
        // covers another artifact entirely.
        let s = sign_subject(7, [0xab; 32], 1, 1);
        let other = hex::encode([0x22u8; 32]);
        let err = verify(&s.doc, &other, &[s.public_key]).unwrap_err();
        assert!(matches!(err, VerifyError::SubjectMismatch { .. }), "{err}");
    }

    #[test]
    fn refuses_a_tampered_signature() {
        let mut s = sign_subject(7, [0xab; 32], 1, 1);
        // Flip one byte of the signature.
        s.doc.signature.replace_range(0..2, "00");
        let err = verify(&s.doc, &s.subject, &[s.public_key]).unwrap_err();
        assert!(matches!(err, VerifyError::Invalid | VerifyError::BadSignature), "{err}");
    }

    #[test]
    fn refuses_a_statement_signed_by_another_key() {
        let mine = sign_subject(7, [0xab; 32], 1, 1);
        let theirs = sign_subject(8, [0xab; 32], 1, 1);
        // Their signature, presented with my (trusted) public key.
        let mut doc = mine.doc.clone();
        doc.signature = theirs.doc.signature;
        let err = verify(&doc, &mine.subject, &[mine.public_key]).unwrap_err();
        assert_eq!(err, VerifyError::Invalid);
    }

    #[test]
    fn refuses_unknown_statement_versions() {
        let mut stmt = [0u8; STATEMENT_LEN];
        stmt[..8].copy_from_slice(MAGIC);
        stmt[off::VERSION] = 2;
        assert_eq!(
            parse_statement(&stmt),
            Err(VerifyError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn refuses_bad_magic_and_bad_length() {
        assert_eq!(parse_statement(&[0u8; 64]), Err(VerifyError::BadMagic));
        assert_eq!(
            parse_statement(&[0u8; 32]),
            Err(VerifyError::StatementLength(32))
        );
    }

    #[test]
    fn parses_the_documented_byte_layout() {
        let s = sign_subject(3, [0xfe; 32], 2, 0x0102_0304_0506_0708);
        let bytes = hex::decode(&s.doc.statement).unwrap();

        assert_eq!(&bytes[..8], b"STORMSIG");
        assert_eq!(bytes[8], 1);
        assert_eq!(bytes[9], 2);
        assert!(bytes[10..16].iter().all(|b| *b == 0), "reserved must be zero");
        assert_eq!(&bytes[16..48], &[0xfe; 32]);
        assert_eq!(&bytes[48..56], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert!(bytes[56..64].iter().all(|b| *b == 0), "reserved must be zero");

        let parsed = parse_statement(&bytes).unwrap();
        assert_eq!(parsed.subject_kind, SubjectKind::Stack);
        assert_eq!(parsed.signed_at, 0x0102_0304_0506_0708);
    }
}
