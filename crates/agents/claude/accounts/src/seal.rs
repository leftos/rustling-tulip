//! Credentials sealed for a file: a [`CredentialBlob`] turned into bytes whose payload is sealed to this Windows user's
//! DPAPI keys, and back.
//!
//! A sealed file is `RTSEAL`, a version byte, a purpose byte and a generation (u64, little-endian), then the sealed
//! payload. The payload is the purpose byte again, the associated data, the text's length (u32, little-endian), the
//! text's SHA-256 and the UTF-8 text. On Windows the payload is sealed with DPAPI at user scope, under an entropy of its
//! own per purpose followed by the associated data, so a file sealed for one use never opens as another.
//!
//! The associated data names the account slot a file belongs to: the slot (an account's number, or `unmanaged`), the
//! organization UUID and the account's identity (its email), each a u32 little-endian length and its UTF-8 bytes, and
//! the generation the header carries as eight bytes. It is checked again in the opened payload, so a file sealed for
//! another slot, another organization or another account's identity does not open as this one, and neither does a file
//! whose header names another generation.
//!
//! Off Windows the payload is stored unsealed, and its SHA-256 only catches damage: it is not authentication, so the
//! store must check that the file and its folder belong to the current user and that no one else can write them.

use std::fmt;

use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::secret::CredentialBlob;

/// The first bytes of every sealed file.
const MAGIC: &[u8; 6] = b"RTSEAL";
/// The file format's version byte.
const VERSION: u8 = 1;
/// The magic, the version byte, the purpose byte and the generation.
const HEADER_LEN: usize = 16;
/// The generation's width in the header.
const GENERATION_LEN: usize = 8;
/// The length and the SHA-256 that come before the text in a payload.
const TEXT_PREFIX_LEN: usize = 4 + 32;
/// The text [`canary`] seals: fixed and not secret.
const CANARY_TEXT: &str = "rustling-tulip sealing canary";
/// The slot [`canary`] seals under, which no account has.
const CANARY_SLOT: &str = "canary";
/// The longest sealed file [`open`] looks into, and the most the store reads of a credential or stash file: 64 KiB.
pub(crate) const MAX_SEALED_LEN: usize = 64 * 1024;

/// What a sealed file belongs to: the slot (an account's number, or `unmanaged` for an unmanaged stash entry), the
/// organization UUID (empty when there is none), the account's identity (its email, empty for a slot no account
/// names) and the generation of the write. A file opens only under the associated data it was sealed with. No part is
/// secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssociatedData {
    slot: String,
    organization: String,
    generation: u64,
    identity: String,
}

impl AssociatedData {
    /// The associated data for `slot` in `organization`, at generation 0 and with no identity: a stash entry, or a
    /// file that names no account.
    #[must_use]
    pub fn new(slot: impl Into<String>, organization: impl Into<String>) -> Self {
        Self {
            slot: slot.into(),
            organization: organization.into(),
            generation: 0,
            identity: String::new(),
        }
    }

    /// Account `number`'s credential file in `organization`, written by the account's `generation`-th write of
    /// `identity` (its email). The number is formatted here, so no call site builds a slot string.
    #[must_use]
    pub fn credentials(
        number: u32,
        organization: impl Into<String>,
        generation: u64,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            slot: number.to_string(),
            organization: organization.into(),
            generation,
            identity: identity.into(),
        }
    }

    /// The generation this associated data names.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The slot, the organization and the identity, each its length (u32, little-endian) and its UTF-8 bytes, then the
    /// generation (u64, little-endian). A part longer than a u32 can count is never sealed.
    fn encode(&self) -> Result<Vec<u8>, SealErrorKind> {
        let mut encoded = Vec::with_capacity(
            16 + self.slot.len() + self.organization.len() + self.identity.len(),
        );
        for part in [&self.slot, &self.organization, &self.identity] {
            let length = u32::try_from(part.len()).map_err(|_| SealErrorKind::SealFailed)?;
            encoded.extend_from_slice(&length.to_le_bytes());
            encoded.extend_from_slice(part.as_bytes());
        }
        encoded.extend_from_slice(&self.generation.to_le_bytes());
        Ok(encoded)
    }
}

/// What a sealed file is for. A file opens only as the purpose it was sealed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealPurpose {
    /// An account's stored credential file.
    Credentials,
    /// An entry in the unclaimed-grant stash.
    StashEntry,
}

impl SealPurpose {
    fn scope(self) -> Scope {
        match self {
            Self::Credentials => CREDENTIALS_SCOPE,
            Self::StashEntry => STASH_ENTRY_SCOPE,
        }
    }
}

/// A purpose's tag byte and, on Windows, the DPAPI entropy it seals under.
#[derive(Clone, Copy)]
struct Scope {
    tag: u8,
    #[cfg(windows)]
    entropy: &'static [u8],
}

const CREDENTIALS_SCOPE: Scope = Scope {
    tag: 1,
    #[cfg(windows)]
    entropy: b"rustling-tulip/accounts/credentials/v1",
};

const STASH_ENTRY_SCOPE: Scope = Scope {
    tag: 2,
    #[cfg(windows)]
    entropy: b"rustling-tulip/accounts/stash-entry/v1",
};

const CANARY_SCOPE: Scope = Scope {
    tag: 3,
    #[cfg(windows)]
    entropy: b"rustling-tulip/accounts/canary/v1",
};

/// A sealed file's bytes, made only by [`seal`] and by [`crate::guard::read_file`], so every file opened was read
/// through the guard. Zeroed on drop; `Debug` prints `SealedBytes(<redacted>)`; no `Clone`, `Display` or `Serialize`.
pub struct SealedBytes(Zeroizing<Vec<u8>>);

impl SealedBytes {
    /// The bytes to write to the file.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The generation the header names, `None` when the bytes are not a sealed file of this version.
    #[must_use]
    pub fn generation(&self) -> Option<u64> {
        let header = self.0.get(..HEADER_LEN)?;
        if &header[..MAGIC.len()] != MAGIC || header[6] != VERSION {
            return None;
        }
        let mut generation = [0_u8; GENERATION_LEN];
        generation.copy_from_slice(&header[HEADER_LEN - GENERATION_LEN..]);
        Some(u64::from_le_bytes(generation))
    }

    /// Bytes the guard read back from a sealed file.
    pub(crate) fn from_file_bytes(bytes: Zeroizing<Vec<u8>>) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for SealedBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SealedBytes(<redacted>)")
    }
}

/// Why sealing or opening failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealErrorKind {
    /// The bytes are not a sealed file for the purpose asked: too short, another format, another version or another
    /// purpose.
    Foreign,
    /// A sealed file that does not open: sealed by another Windows user or on another PC, or damaged.
    Unopenable,
    /// Windows refused to seal.
    SealFailed,
    /// The credentials were empty, which are never sealed.
    Empty,
    /// The sealed file would be over 64 KiB, which [`open`] and the guard refuse, so it is never written.
    TooLarge,
}

impl SealErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Foreign => "not a file the account store sealed",
            Self::Unopenable => {
                "sealed for another Windows user or PC, or damaged; add the login again"
            }
            Self::SealFailed => "Windows could not seal the credentials",
            Self::Empty => "empty credentials are never sealed",
            Self::TooLarge => "the credentials are too large to seal",
        }
    }
}

/// A failed seal or open: its kind and, when Windows reported one, the OS error code. It never holds the text, the
/// sealed bytes or where they came from.
#[derive(Debug, thiserror::Error)]
#[error("{}{}", .kind.message(), OsCode(*.os_code))]
pub struct SealError {
    kind: SealErrorKind,
    os_code: Option<i32>,
}

impl SealError {
    fn new(kind: SealErrorKind) -> Self {
        Self {
            kind,
            os_code: None,
        }
    }

    #[cfg(windows)]
    fn with_os_code(kind: SealErrorKind, os_code: Option<i32>) -> Self {
        Self { kind, os_code }
    }

    /// Why it failed.
    #[must_use]
    pub fn kind(&self) -> SealErrorKind {
        self.kind
    }

    /// The OS error code Windows reported, if any.
    #[must_use]
    pub fn os_code(&self) -> Option<i32> {
        self.os_code
    }
}

/// ` (os error N)` when there is a code, nothing otherwise.
struct OsCode(Option<i32>);

impl fmt::Display for OsCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(code) => write!(formatter, " (os error {code})"),
            None => Ok(()),
        }
    }
}

/// Seals `blob` for `purpose` and the account slot `ad`, as the bytes of a file.
///
/// Sealing is not deterministic on Windows: the same text seals to different bytes each time, so two sealed files are
/// never compared to tell whether they hold the same text.
///
/// # Errors
///
/// [`SealErrorKind::Empty`] for empty credentials; [`SealErrorKind::TooLarge`] when the file would be over 64 KiB;
/// [`SealErrorKind::SealFailed`] when Windows refuses to seal.
pub fn seal(
    purpose: SealPurpose,
    ad: &AssociatedData,
    blob: &CredentialBlob,
) -> Result<SealedBytes, SealError> {
    seal_scope(purpose.scope(), ad, blob.expose())
}

/// Opens a sealed file's bytes as the credentials sealed for `purpose` and the account slot `ad`.
///
/// # Errors
///
/// [`SealErrorKind::Foreign`] when the bytes are not a sealed file for `purpose`; [`SealErrorKind::Unopenable`] when
/// they are but do not open (another Windows user or PC, another account slot, or damage). Bytes over 64 KiB are
/// [`SealErrorKind::Foreign`] before DPAPI sees them.
pub fn open(
    purpose: SealPurpose,
    ad: &AssociatedData,
    sealed: &SealedBytes,
) -> Result<CredentialBlob, SealError> {
    open_scope(purpose.scope(), ad, sealed.as_bytes())
}

/// Seals and opens a fixed, non-secret text under a purpose and associated data of its own, which tells a sealing that
/// is broken in this process from a file that is bad.
///
/// # Errors
///
/// The error sealing or opening the canary gave, or [`SealErrorKind::Unopenable`] when it opened to other text.
pub fn canary() -> Result<(), SealError> {
    let ad = AssociatedData::new(CANARY_SLOT, "");
    let sealed = seal_scope(CANARY_SCOPE, &ad, CANARY_TEXT)?;
    let opened = open_scope(CANARY_SCOPE, &ad, sealed.as_bytes())?;
    if opened.expose() == CANARY_TEXT {
        Ok(())
    } else {
        Err(SealError::new(SealErrorKind::Unopenable))
    }
}

/// The DPAPI entropy for `scope` and the encoded associated data `ad`: the purpose's constant, then `ad`.
#[cfg(windows)]
fn entropy(scope: Scope, ad: &[u8]) -> Vec<u8> {
    let mut entropy = Vec::with_capacity(scope.entropy.len() + ad.len());
    entropy.extend_from_slice(scope.entropy);
    entropy.extend_from_slice(ad);
    entropy
}

fn seal_scope(scope: Scope, ad: &AssociatedData, text: &str) -> Result<SealedBytes, SealError> {
    if text.is_empty() {
        return Err(SealError::new(SealErrorKind::Empty));
    }
    if text.len() > MAX_SEALED_LEN {
        return Err(SealError::new(SealErrorKind::TooLarge));
    }
    let generation = ad.generation;
    let ad = ad.encode().map_err(SealError::new)?;
    let payload = encode_payload(scope.tag, &ad, text).map_err(SealError::new)?;
    #[cfg(windows)]
    let body = dpapi::protect(&payload, &entropy(scope, &ad))
        .map_err(|code| SealError::with_os_code(SealErrorKind::SealFailed, code))?;
    #[cfg(not(windows))]
    let body = payload;
    if HEADER_LEN + body.len() > MAX_SEALED_LEN {
        return Err(SealError::new(SealErrorKind::TooLarge));
    }
    let mut file = Zeroizing::new(Vec::with_capacity(HEADER_LEN + body.len()));
    file.extend_from_slice(MAGIC);
    file.push(VERSION);
    file.push(scope.tag);
    file.extend_from_slice(&generation.to_le_bytes());
    file.extend_from_slice(&body);
    Ok(SealedBytes(file))
}

fn open_scope(
    scope: Scope,
    ad: &AssociatedData,
    sealed: &[u8],
) -> Result<CredentialBlob, SealError> {
    if sealed.len() > MAX_SEALED_LEN {
        return Err(SealError::new(SealErrorKind::Foreign));
    }
    let Some((header, body)) = sealed.split_first_chunk::<HEADER_LEN>() else {
        return Err(SealError::new(SealErrorKind::Foreign));
    };
    if header[..MAGIC.len()] != MAGIC[..] || header[6] != VERSION || header[7] != scope.tag {
        return Err(SealError::new(SealErrorKind::Foreign));
    }
    let mut generation = [0_u8; GENERATION_LEN];
    generation.copy_from_slice(&header[HEADER_LEN - GENERATION_LEN..]);
    if u64::from_le_bytes(generation) != ad.generation {
        return Err(SealError::new(SealErrorKind::Foreign));
    }
    // Associated data too long to encode was never sealed, so no file opens under it.
    let ad = ad
        .encode()
        .map_err(|_| SealError::new(SealErrorKind::Unopenable))?;
    #[cfg(windows)]
    let opened = dpapi::unprotect(body, &entropy(scope, &ad))
        .map_err(|code| SealError::with_os_code(SealErrorKind::Unopenable, code))?;
    #[cfg(windows)]
    let payload: &[u8] = &opened;
    #[cfg(not(windows))]
    let payload = body;
    decode_payload(scope.tag, &ad, payload).map_err(SealError::new)
}

/// The payload for `text` under the purpose `tag` and the encoded associated data `ad`, before sealing.
fn encode_payload(tag: u8, ad: &[u8], text: &str) -> Result<Zeroizing<Vec<u8>>, SealErrorKind> {
    payload_bytes(tag, ad, text.as_bytes())
}

/// The payload for any bytes, valid UTF-8 or not. Text longer than a u32 can count is never sealed.
fn payload_bytes(tag: u8, ad: &[u8], bytes: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealErrorKind> {
    let length = u32::try_from(bytes.len()).map_err(|_| SealErrorKind::SealFailed)?;
    let mut payload = Zeroizing::new(Vec::with_capacity(
        1 + ad.len() + TEXT_PREFIX_LEN + bytes.len(),
    ));
    payload.push(tag);
    payload.extend_from_slice(ad);
    payload.extend_from_slice(&length.to_le_bytes());
    payload.extend_from_slice(&Sha256::digest(bytes));
    payload.extend_from_slice(bytes);
    Ok(payload)
}

/// The credentials in an opened payload, checked against the purpose `tag`, the encoded associated data `ad`, and the
/// length and the hash it carries.
fn decode_payload(tag: u8, ad: &[u8], payload: &[u8]) -> Result<CredentialBlob, SealErrorKind> {
    let Some((&inner_tag, rest)) = payload.split_first() else {
        return Err(SealErrorKind::Unopenable);
    };
    let Some(rest) = rest.strip_prefix(ad) else {
        return Err(SealErrorKind::Unopenable);
    };
    let Some((prefix, text)) = rest.split_first_chunk::<TEXT_PREFIX_LEN>() else {
        return Err(SealErrorKind::Unopenable);
    };
    let (length, hash) = prefix.split_at(4);
    let length = u32::from_le_bytes([length[0], length[1], length[2], length[3]]);
    let length_matches = usize::try_from(length).is_ok_and(|length| length == text.len());
    if inner_tag != tag || length == 0 || !length_matches || Sha256::digest(text).as_slice() != hash
    {
        return Err(SealErrorKind::Unopenable);
    }
    let text = std::str::from_utf8(text).map_err(|_| SealErrorKind::Unopenable)?;
    Ok(CredentialBlob::new(text.to_owned()))
}

/// DPAPI's protect and unprotect at user scope, with no UI, no description and no prompt.
#[cfg(windows)]
mod dpapi {
    use std::io;
    use std::ptr;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use zeroize::{Zeroize as _, Zeroizing};

    /// What a DPAPI call's output amounts to.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Outcome {
        /// The call succeeded with bytes.
        Bytes,
        /// The call failed.
        Failed,
        /// The call reported success with no bytes (a null buffer or a zero length), which is a failure too.
        NoBytes,
    }

    /// The outcome of a call that returned `succeeded` and left an output buffer that is null or not, of `len` bytes.
    pub(super) fn outcome(succeeded: bool, output_is_null: bool, len: u32) -> Outcome {
        if !succeeded {
            Outcome::Failed
        } else if output_is_null || len == 0 {
            Outcome::NoBytes
        } else {
            Outcome::Bytes
        }
    }

    /// A DPAPI output buffer. When it drops, its bytes are zeroed if a successful call filled it, and any buffer DPAPI
    /// left in it is freed.
    struct LocalBuffer {
        blob: CRYPT_INTEGER_BLOB,
        filled: bool,
    }

    impl LocalBuffer {
        fn empty() -> Self {
            Self {
                blob: CRYPT_INTEGER_BLOB::default(),
                filled: false,
            }
        }

        /// The bytes of a call that returned `succeeded`, copied into a buffer sized to them exactly and zeroed on
        /// drop; `os_code` when the call failed, and no code when it reported success with no bytes.
        fn into_result(
            mut self,
            succeeded: bool,
            os_code: Option<i32>,
        ) -> Result<Zeroizing<Vec<u8>>, Option<i32>> {
            match outcome(succeeded, self.blob.pbData.is_null(), self.blob.cbData) {
                Outcome::Failed => Err(os_code),
                Outcome::NoBytes => Err(None),
                Outcome::Bytes => {
                    self.filled = true;
                    let len = self.blob.cbData as usize;
                    let mut copy = Zeroizing::new(Vec::with_capacity(len));
                    // SAFETY: the call succeeded with a non-null `pbData`, so DPAPI set it to a buffer of `cbData`
                    // bytes it allocated, which only this guard refers to until it drops.
                    copy.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(self.blob.pbData, len)
                    });
                    Ok(copy)
                }
            }
        }
    }

    impl Drop for LocalBuffer {
        fn drop(&mut self) {
            if self.blob.pbData.is_null() {
                return;
            }
            if self.filled {
                // SAFETY: `filled` is set only after a successful call with a non-null `pbData`, so it points at
                // `cbData` bytes DPAPI allocated, which nothing else refers to.
                unsafe {
                    std::slice::from_raw_parts_mut(self.blob.pbData, self.blob.cbData as usize)
                }
                .zeroize();
            }
            // SAFETY: a non-null `pbData` was set by DPAPI to memory it allocated with `LocalAlloc` (the pointer starts
            // null), and it is freed here once, after which the pointer is cleared.
            let failed = unsafe { LocalFree(self.blob.pbData.cast()) };
            debug_assert!(failed.is_null(), "LocalFree refused DPAPI's output buffer");
            self.blob.pbData = ptr::null_mut();
        }
    }

    /// The calling thread's last OS error code; none when it is 0, which names no error.
    fn last_os_code() -> Option<i32> {
        io::Error::last_os_error()
            .raw_os_error()
            .filter(|&code| code != 0)
    }

    /// A blob that points at `bytes`, for a DPAPI input it only reads. `None` when `bytes` is longer than a u32.
    fn input_blob(bytes: &[u8]) -> Option<CRYPT_INTEGER_BLOB> {
        Some(CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).ok()?,
            pbData: bytes.as_ptr().cast_mut(),
        })
    }

    /// `payload` sealed for this Windows user under `entropy`. The error is the OS code, if Windows gave one.
    pub(super) fn protect(
        payload: &[u8],
        entropy: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, Option<i32>> {
        let (Some(input), Some(entropy)) = (input_blob(payload), input_blob(entropy)) else {
            return Err(None);
        };
        let mut output = LocalBuffer::empty();
        // SAFETY: `input` and `entropy` point at slices that outlive this synchronous call and DPAPI only reads them;
        // the description, reserved and prompt pointers are null, which the API allows; `output` receives a buffer
        // the guard frees.
        let sealed = unsafe {
            CryptProtectData(
                &raw const input,
                ptr::null(),
                &raw const entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut output.blob,
            )
        } != 0;
        let os_code = if sealed { None } else { last_os_code() };
        output.into_result(sealed, os_code)
    }

    /// `sealed` opened for this Windows user under `entropy`. The error is the OS code, if Windows gave one.
    pub(super) fn unprotect(
        sealed: &[u8],
        entropy: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, Option<i32>> {
        let (Some(input), Some(entropy)) = (input_blob(sealed), input_blob(entropy)) else {
            return Err(None);
        };
        let mut output = LocalBuffer::empty();
        // SAFETY: `input` and `entropy` point at slices that outlive this synchronous call and DPAPI only reads them;
        // the description out pointer, reserved and prompt pointers are null, which the API allows; `output` receives
        // a buffer the guard frees.
        let opened = unsafe {
            CryptUnprotectData(
                &raw const input,
                ptr::null_mut(),
                &raw const entropy,
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut output.blob,
            )
        } != 0;
        let os_code = if opened { None } else { last_os_code() };
        output.into_result(opened, os_code)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a payload that cannot be encoded must fail the test loudly"
)]
mod tests {
    use super::{AssociatedData, SealErrorKind, decode_payload, encode_payload, payload_bytes};

    const TEXT: &str = r#"{"claudeAiOauth":{"accessToken":"unit-access"}}"#;

    /// Account 1 in organization `org-a`, encoded.
    fn ad() -> Vec<u8> {
        AssociatedData::new("1", "org-a")
            .encode()
            .expect("the associated data encodes")
    }

    /// Where the text's length starts in a payload sealed under [`ad`].
    fn length_at() -> usize {
        1 + ad().len()
    }

    fn decoded(tag: u8, payload: &[u8]) -> Result<String, SealErrorKind> {
        decoded_for(tag, &ad(), payload)
    }

    fn decoded_for(tag: u8, ad: &[u8], payload: &[u8]) -> Result<String, SealErrorKind> {
        decode_payload(tag, ad, payload).map(|blob| blob.expose().to_owned())
    }

    fn payload() -> Vec<u8> {
        encode_payload(1, &ad(), TEXT)
            .map(|payload| payload.to_vec())
            .expect("the payload encodes")
    }

    #[test]
    fn associated_data_is_each_part_length_prefixed_before_the_generation() {
        let encoded = AssociatedData::new("12", "ab")
            .encode()
            .expect("the associated data encodes");
        assert_eq!(
            encoded,
            b"\x02\x00\x00\x0012\x02\x00\x00\x00ab\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
        );
        let empty = AssociatedData::new("unmanaged", "")
            .encode()
            .expect("the associated data encodes");
        assert_eq!(
            empty,
            b"\x09\x00\x00\x00unmanaged\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
        );
        let credential = AssociatedData::credentials(12, "ab", 7, "e");
        assert_eq!(
            credential.encode().expect("the associated data encodes"),
            b"\x02\x00\x00\x0012\x02\x00\x00\x00ab\x01\x00\x00\x00e\x07\x00\x00\x00\x00\x00\x00\x00"
        );
        assert_eq!(credential.generation(), 7);
    }

    #[test]
    fn encode_then_decode_round_trips() {
        assert_eq!(decoded(1, &payload()), Ok(TEXT.to_owned()));
        assert_eq!(
            encode_payload(2, &ad(), "x").map(|payload| decoded(2, &payload)),
            Ok(Ok("x".to_owned()))
        );
    }

    #[test]
    fn a_wrong_hash_is_unopenable() {
        let mut payload = payload();
        payload[length_at() + 4] ^= 0x01;
        assert_eq!(decoded(1, &payload), Err(SealErrorKind::Unopenable));
    }

    #[test]
    fn a_wrong_length_is_unopenable() {
        let mut payload = payload();
        payload[length_at()] ^= 0x01;
        assert_eq!(decoded(1, &payload), Err(SealErrorKind::Unopenable));
        let full = payload_bytes(1, &ad(), TEXT.as_bytes())
            .map(|payload| payload.to_vec())
            .expect("the payload encodes");
        assert_eq!(
            decoded(1, &full[..full.len() - 1]),
            Err(SealErrorKind::Unopenable)
        );
    }

    #[test]
    fn a_wrong_inner_tag_is_unopenable() {
        assert_eq!(decoded(2, &payload()), Err(SealErrorKind::Unopenable));
    }

    #[test]
    fn a_flipped_associated_data_byte_in_the_payload_is_unopenable() {
        for at in 1..length_at() {
            let mut payload = payload();
            payload[at] ^= 0x01;
            assert_eq!(
                decoded(1, &payload),
                Err(SealErrorKind::Unopenable),
                "byte {at}"
            );
        }
    }

    #[test]
    fn a_payload_for_other_associated_data_is_unopenable() {
        let other = AssociatedData::new("2", "org-a")
            .encode()
            .expect("the associated data encodes");
        assert_eq!(
            decoded_for(1, &other, &payload()),
            Err(SealErrorKind::Unopenable)
        );
        assert_eq!(
            decoded_for(1, &[], &payload()),
            Err(SealErrorKind::Unopenable)
        );
    }

    #[test]
    fn invalid_utf8_is_unopenable() {
        let payload = payload_bytes(1, &ad(), &[0xff, 0xfe, 0x41])
            .map(|payload| payload.to_vec())
            .expect("the payload encodes");
        assert_eq!(decoded(1, &payload), Err(SealErrorKind::Unopenable));
    }

    #[cfg(windows)]
    #[test]
    fn a_dpapi_success_without_bytes_is_a_failure() {
        use super::dpapi::{Outcome, outcome};

        assert_eq!(outcome(true, false, 12), Outcome::Bytes);
        assert_eq!(outcome(true, true, 12), Outcome::NoBytes);
        assert_eq!(outcome(true, false, 0), Outcome::NoBytes);
        assert_eq!(outcome(true, true, 0), Outcome::NoBytes);
        assert_eq!(outcome(false, false, 12), Outcome::Failed);
        assert_eq!(outcome(false, true, 0), Outcome::Failed);
    }

    #[cfg(windows)]
    #[test]
    fn only_the_entropy_refuses_a_payload_sealed_for_another_purpose() {
        use super::{CREDENTIALS_SCOPE, MAGIC, SealPurpose, VERSION, dpapi, entropy, open};

        let payload = encode_payload(2, &ad(), TEXT).expect("the payload encodes");
        let body =
            dpapi::protect(&payload, &entropy(CREDENTIALS_SCOPE, &ad())).expect("DPAPI seals");
        let mut file = MAGIC.to_vec();
        file.push(VERSION);
        file.push(2);
        file.extend_from_slice(&0_u64.to_le_bytes());
        file.extend_from_slice(&body);

        let file = super::SealedBytes::from_file_bytes(zeroize::Zeroizing::new(file));
        let error = open(
            SealPurpose::StashEntry,
            &AssociatedData::new("1", "org-a"),
            &file,
        )
        .expect_err("the entropy must refuse it");
        assert_eq!(error.kind(), SealErrorKind::Unopenable);
        assert!(error.os_code().is_some(), "DPAPI refused it: {error:?}");
    }

    /// DPAPI opens the payload, because the entropy is account 1's, and the payload's own associated data, account
    /// 2's, refuses it.
    #[cfg(windows)]
    #[test]
    fn the_payload_refuses_other_associated_data_under_matching_entropy() {
        use super::{CREDENTIALS_SCOPE, MAGIC, SealPurpose, VERSION, dpapi, entropy, open};

        let other = AssociatedData::new("2", "org-a")
            .encode()
            .expect("the associated data encodes");
        let payload = encode_payload(1, &other, TEXT).expect("the payload encodes");
        let body =
            dpapi::protect(&payload, &entropy(CREDENTIALS_SCOPE, &ad())).expect("DPAPI seals");
        let mut file = MAGIC.to_vec();
        file.push(VERSION);
        file.push(1);
        file.extend_from_slice(&0_u64.to_le_bytes());
        file.extend_from_slice(&body);

        let file = super::SealedBytes::from_file_bytes(zeroize::Zeroizing::new(file));
        let error = open(
            SealPurpose::Credentials,
            &AssociatedData::new("1", "org-a"),
            &file,
        )
        .expect_err("the payload must refuse it");
        assert_eq!(error.kind(), SealErrorKind::Unopenable);
        assert_eq!(
            error.os_code(),
            None,
            "DPAPI opened it and the payload refused it: {error:?}"
        );
    }

    #[test]
    fn a_file_over_64_kib_is_foreign_before_any_opening() {
        use super::{SealPurpose, SealedBytes, open};

        // DPAPI and the payload check would both call these bytes unopenable, DPAPI with an OS code, so `Foreign`
        // with no code means neither saw them.
        let mut bytes = b"RTSEAL\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        bytes.resize(65_537, 0x5a);
        let file = SealedBytes::from_file_bytes(zeroize::Zeroizing::new(bytes));
        let error = open(
            SealPurpose::Credentials,
            &AssociatedData::new("1", "org-a"),
            &file,
        )
        .expect_err("over the cap");
        assert_eq!(error.kind(), SealErrorKind::Foreign);
        assert_eq!(error.os_code(), None);
    }

    #[test]
    fn a_file_of_exactly_64_kib_reaches_the_opening() {
        use super::{SealPurpose, SealedBytes, open};

        let mut bytes = b"RTSEAL\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        bytes.resize(65_536, 0x5a);
        let file = SealedBytes::from_file_bytes(zeroize::Zeroizing::new(bytes));
        let error = open(
            SealPurpose::Credentials,
            &AssociatedData::new("1", "org-a"),
            &file,
        )
        .expect_err("garbage");
        assert_eq!(error.kind(), SealErrorKind::Unopenable);
        #[cfg(windows)]
        assert!(
            error.os_code().is_some(),
            "DPAPI must have been asked: {error:?}"
        );
    }

    #[test]
    fn a_zero_length_is_unopenable() {
        let payload = payload_bytes(1, &ad(), b"")
            .map(|payload| payload.to_vec())
            .expect("the payload encodes");
        assert_eq!(payload.len(), 1 + ad().len() + 36);
        assert_eq!(decoded(1, &payload), Err(SealErrorKind::Unopenable));
        assert_eq!(decoded(1, &[]), Err(SealErrorKind::Unopenable));
    }
}
