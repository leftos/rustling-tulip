//! Sealed credential files: what seals opens again under the same purpose and associated data, and anything else
//! (another purpose, another account slot or organization, a tampered or truncated file, a cswap base64 file) is
//! refused without a secret in the error. Every crafted file is written into a guard-made folder and read back
//! through the guard, the only way bytes from a file become `SealedBytes`.

use std::fs;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use claude_accounts::CredentialBlob;
use claude_accounts::guard;
use claude_accounts::seal::{
    self, AssociatedData, SealError, SealErrorKind, SealPurpose, SealedBytes,
};

use crate::guard::private_base;
use crate::support::Scratch;

/// The header's length: `OTSEAL`, the version, the purpose and the generation.
const HEADER_LEN: usize = 16;

const PLAINTEXT: &str =
    r#"{"claudeAiOauth":{"accessToken":"seal-test-access","refreshToken":"seal-test-refresh"}}"#;
const SECRET_WORDS: [&str; 3] = [PLAINTEXT, "seal-test-access", "seal-test-refresh"];
const ORGANIZATION: &str = "0c6f3d52-7a41-4e8b-9f20-3b5d8e1a6c74";
const OTHER_ORGANIZATION: &str = "9a2e7c18-5b3d-4f60-8e94-1d7c0b6a2f53";

/// Account 1 in [`ORGANIZATION`]: the associated data every fixture seals under.
fn ad() -> AssociatedData {
    AssociatedData::new("1", ORGANIZATION)
}

fn sealed(purpose: SealPurpose) -> SealedBytes {
    sealed_for(purpose, &ad())
}

fn sealed_for(purpose: SealPurpose, ad: &AssociatedData) -> SealedBytes {
    seal::seal(purpose, ad, &CredentialBlob::new(PLAINTEXT.to_owned())).expect("the fixture seals")
}

fn opened_text(purpose: SealPurpose, file: &SealedBytes) -> Result<String, SealErrorKind> {
    opened_text_for(purpose, &ad(), file)
}

fn opened_text_for(
    purpose: SealPurpose,
    ad: &AssociatedData,
    file: &SealedBytes,
) -> Result<String, SealErrorKind> {
    seal::open(purpose, ad, file)
        .map(|blob| blob.expose().to_owned())
        .map_err(|error| error.kind())
}

fn open_error(purpose: SealPurpose, file: &SealedBytes) -> SealError {
    open_error_for(purpose, &ad(), file)
}

fn open_error_for(purpose: SealPurpose, ad: &AssociatedData, file: &SealedBytes) -> SealError {
    seal::open(purpose, ad, file).expect_err("the bytes must be refused")
}

/// A guard-made folder in which a test writes bytes and reads them back as `SealedBytes`.
struct Files {
    _scratch: Scratch,
    base: PathBuf,
    folder: PathBuf,
}

impl Files {
    fn new(test: &str) -> Self {
        let (scratch, base) = private_base(&format!("seal-{test}"));
        let folder = base.join("credentials");
        guard::create_folder(&base, &folder).expect("the guard makes the folder");
        Self {
            _scratch: scratch,
            base,
            folder,
        }
    }

    /// `bytes` written to the file `name` and read back through the guard.
    fn read_back(&self, name: &str, bytes: &[u8]) -> SealedBytes {
        let path = self.folder.join(name);
        fs::write(&path, bytes).expect("the file can be written");
        guard::read_file(&self.base, &path).expect("the guard reads the file")
    }
}

/// A sealed `Credentials` file whose purpose byte claims `StashEntry`.
fn rewritten_purpose(files: &Files) -> SealedBytes {
    let mut bytes = sealed(SealPurpose::Credentials).as_bytes().to_vec();
    bytes[7] = 2;
    files.read_back("rewritten-purpose", &bytes)
}

/// A sealed `Credentials` file with one byte flipped halfway through what follows the header.
fn flipped_byte(files: &Files) -> SealedBytes {
    let mut bytes = sealed(SealPurpose::Credentials).as_bytes().to_vec();
    let middle = HEADER_LEN + (bytes.len() - HEADER_LEN) / 2;
    bytes[middle] ^= 0x01;
    files.read_back("flipped-byte", &bytes)
}

/// An intact header for a `Credentials` file, followed by bytes that are no sealed payload.
fn garbage_after_header(files: &Files) -> SealedBytes {
    let mut bytes = b"OTSEAL\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
    bytes.extend_from_slice(&[0x5a; 64]);
    files.read_back("garbage", &bytes)
}

#[test]
fn round_trip_credentials() {
    let file = sealed(SealPurpose::Credentials);
    assert_eq!(
        opened_text(SealPurpose::Credentials, &file),
        Ok(PLAINTEXT.to_owned())
    );

    let files = Files::new("round-trip");
    let reread = files.read_back("1.cred", file.as_bytes());
    assert_eq!(
        opened_text(SealPurpose::Credentials, &reread),
        Ok(PLAINTEXT.to_owned())
    );
}

#[test]
fn round_trip_stash_entry() {
    let file = sealed(SealPurpose::StashEntry);
    assert_eq!(
        opened_text(SealPurpose::StashEntry, &file),
        Ok(PLAINTEXT.to_owned())
    );
}

#[test]
fn an_unmanaged_slot_with_no_organization_round_trips() {
    let unmanaged = AssociatedData::new("unmanaged", "");
    let file = sealed_for(SealPurpose::StashEntry, &unmanaged);
    assert_eq!(
        opened_text_for(SealPurpose::StashEntry, &unmanaged, &file),
        Ok(PLAINTEXT.to_owned())
    );
}

/// On Windows DPAPI refuses it, which proves the associated data is in the entropy; off Windows the payload does.
fn assert_refused_by_dpapi(error: &SealError) {
    assert_eq!(error.kind(), SealErrorKind::Unopenable);
    #[cfg(windows)]
    assert!(error.os_code().is_some(), "DPAPI must refuse it: {error:?}");
    #[cfg(not(windows))]
    assert_eq!(error.os_code(), None);
}

#[test]
fn opening_as_another_slot_is_refused() {
    let file = sealed(SealPurpose::Credentials);
    let other_slot = AssociatedData::new("2", ORGANIZATION);
    assert_refused_by_dpapi(&open_error_for(
        SealPurpose::Credentials,
        &other_slot,
        &file,
    ));
}

#[test]
fn opening_as_another_organization_is_refused() {
    let file = sealed(SealPurpose::Credentials);
    for organization in [OTHER_ORGANIZATION, ""] {
        let other = AssociatedData::new("1", organization);
        assert_refused_by_dpapi(&open_error_for(SealPurpose::Credentials, &other, &file));
    }
}

#[test]
fn the_slot_and_organization_do_not_run_together() {
    let file = sealed_for(SealPurpose::Credentials, &AssociatedData::new("1", "2x"));
    assert_refused_by_dpapi(&open_error_for(
        SealPurpose::Credentials,
        &AssociatedData::new("12", "x"),
        &file,
    ));
}

/// On Windows the associated data lives only inside DPAPI's sealed payload and its entropy; off Windows the payload is
/// stored unsealed, so it carries the associated data in the clear beside the credentials themselves.
#[cfg(windows)]
#[test]
fn the_stored_bytes_carry_no_associated_data() {
    let unmanaged = AssociatedData::new("unmanaged", ORGANIZATION);
    let file = sealed_for(SealPurpose::Credentials, &unmanaged);
    for text in ["unmanaged", ORGANIZATION] {
        assert!(
            !file
                .as_bytes()
                .windows(text.len())
                .any(|window| window == text.as_bytes()),
            "the sealed file must not carry {text:?}"
        );
    }
}

#[cfg(windows)]
#[test]
fn sealing_twice_gives_different_bytes_that_both_open() {
    let first = sealed(SealPurpose::Credentials);
    let second = sealed(SealPurpose::Credentials);
    assert_ne!(first.as_bytes(), second.as_bytes());
    assert_eq!(
        opened_text(SealPurpose::Credentials, &first),
        Ok(PLAINTEXT.to_owned())
    );
    assert_eq!(
        opened_text(SealPurpose::Credentials, &second),
        Ok(PLAINTEXT.to_owned())
    );
}

#[test]
fn opening_with_the_other_purpose_is_refused() {
    let credentials = sealed(SealPurpose::Credentials);
    assert_eq!(
        opened_text(SealPurpose::StashEntry, &credentials),
        Err(SealErrorKind::Foreign)
    );
    let stash = sealed(SealPurpose::StashEntry);
    assert_eq!(
        opened_text(SealPurpose::Credentials, &stash),
        Err(SealErrorKind::Foreign)
    );
}

#[test]
fn a_rewritten_purpose_byte_is_refused() {
    let files = Files::new("rewritten-purpose");
    let error = open_error(SealPurpose::StashEntry, &rewritten_purpose(&files));
    assert_eq!(error.kind(), SealErrorKind::Unopenable);
    // On Windows DPAPI refuses it, so the entropy separates the purposes; off Windows the inner tag does.
    #[cfg(windows)]
    assert!(error.os_code().is_some(), "DPAPI must refuse it: {error:?}");
    #[cfg(not(windows))]
    assert_eq!(error.os_code(), None);
}

#[test]
fn a_full_length_file_of_another_version_is_foreign() {
    let files = Files::new("version");
    let mut bytes = sealed(SealPurpose::Credentials).as_bytes().to_vec();
    bytes[6] = 2;
    assert_eq!(
        opened_text(SealPurpose::Credentials, &files.read_back("1.cred", &bytes)),
        Err(SealErrorKind::Foreign)
    );
}

#[test]
fn a_full_length_file_with_wrong_magic_is_foreign() {
    let files = Files::new("magic");
    let mut bytes = sealed(SealPurpose::Credentials).as_bytes().to_vec();
    bytes[0] = b'X';
    assert_eq!(
        opened_text(SealPurpose::Credentials, &files.read_back("1.cred", &bytes)),
        Err(SealErrorKind::Foreign)
    );
}

#[test]
fn garbage_after_an_intact_header_is_unopenable() {
    let files = Files::new("garbage");
    let error = open_error(SealPurpose::Credentials, &garbage_after_header(&files));
    assert_eq!(error.kind(), SealErrorKind::Unopenable);
    #[cfg(windows)]
    assert!(error.os_code().is_some(), "DPAPI must refuse it: {error:?}");
}

#[cfg(windows)]
#[test]
fn a_dpapi_refusal_names_its_os_code() {
    let files = Files::new("os-code");
    let error = open_error(SealPurpose::Credentials, &garbage_after_header(&files));
    let code = error.os_code().expect("DPAPI gave a code");
    assert_eq!(
        error.to_string(),
        format!(
            "sealed for another Windows user or PC, or damaged; add the login again (os error {code})"
        )
    );
}

#[test]
fn a_flipped_ciphertext_byte_is_unopenable() {
    let files = Files::new("flipped");
    assert_eq!(
        opened_text(SealPurpose::Credentials, &flipped_byte(&files)),
        Err(SealErrorKind::Unopenable)
    );
}

#[test]
fn a_truncated_file_is_refused() {
    let files = Files::new("truncated");
    let file = sealed(SealPurpose::Credentials);
    let bytes = file.as_bytes();
    assert_eq!(
        opened_text(SealPurpose::Credentials, &files.read_back("7", &bytes[..7])),
        Err(SealErrorKind::Foreign)
    );
    assert_eq!(
        opened_text(
            SealPurpose::Credentials,
            &files.read_back("15", &bytes[..HEADER_LEN - 1])
        ),
        Err(SealErrorKind::Foreign)
    );
    assert_eq!(
        opened_text(
            SealPurpose::Credentials,
            &files.read_back("16", &bytes[..HEADER_LEN])
        ),
        Err(SealErrorKind::Unopenable)
    );
    assert_eq!(
        opened_text(
            SealPurpose::Credentials,
            &files.read_back("short", &bytes[..bytes.len() - 3])
        ),
        Err(SealErrorKind::Unopenable)
    );
}

#[test]
fn a_cswap_base64_file_is_foreign() {
    let files = Files::new("cswap");
    let cswap = STANDARD.encode(r#"{"claudeAiOauth":{}}"#);
    for (name, bytes) in [
        ("cswap", cswap.as_bytes()),
        ("empty", b"".as_slice()),
        ("plain", PLAINTEXT.as_bytes()),
    ] {
        let error = open_error(SealPurpose::Credentials, &files.read_back(name, bytes));
        assert_eq!(error.kind(), SealErrorKind::Foreign);
        assert_eq!(error.os_code(), None);
        assert_eq!(error.to_string(), "not a file the account store sealed");
    }
}

#[test]
fn empty_credentials_are_never_sealed() {
    let error = seal::seal(
        SealPurpose::Credentials,
        &ad(),
        &CredentialBlob::new(String::new()),
    )
    .expect_err("empty text is refused");
    assert_eq!(error.kind(), SealErrorKind::Empty);
    assert_eq!(error.to_string(), "empty credentials are never sealed");
}

#[test]
fn credentials_too_large_for_a_64_kib_file_are_never_sealed() {
    for len in [65_537, 65_536 - 8] {
        let error = seal::seal(
            SealPurpose::Credentials,
            &ad(),
            &CredentialBlob::new("x".repeat(len)),
        )
        .expect_err("too large to seal");
        assert_eq!(error.kind(), SealErrorKind::TooLarge, "{len}");
        assert_eq!(error.to_string(), "the credentials are too large to seal");
    }
    seal::seal(
        SealPurpose::Credentials,
        &ad(),
        &CredentialBlob::new("x".repeat(30_000)),
    )
    .expect("well under the cap seals");
}

#[test]
fn canary_passes() {
    seal::canary().expect("sealing works in this process");
}

fn assert_no_secret(text: &str, sealed: &[u8]) {
    for word in SECRET_WORDS {
        assert!(!text.contains(word), "{text:?} must not contain {word:?}");
    }
    for window in sealed.windows(8) {
        assert!(
            !text
                .as_bytes()
                .windows(8)
                .any(|candidate| candidate == window),
            "{text:?} must not contain sealed bytes {window:?}"
        );
    }
}

#[test]
fn debug_and_errors_carry_no_secret() {
    let files = Files::new("no-secret");
    let file = sealed(SealPurpose::Credentials);
    assert_eq!(format!("{file:?}"), "SealedBytes(<redacted>)");

    let bytes = file.as_bytes();
    // Each error is checked against the bytes of the file that produced it.
    let cases = [
        (SealPurpose::StashEntry, files.read_back("whole", bytes)),
        (SealPurpose::StashEntry, rewritten_purpose(&files)),
        (SealPurpose::Credentials, flipped_byte(&files)),
        (SealPurpose::Credentials, files.read_back("7", &bytes[..7])),
        (
            SealPurpose::Credentials,
            files.read_back("16", &bytes[..HEADER_LEN]),
        ),
        (
            SealPurpose::Credentials,
            files.read_back("short", &bytes[..bytes.len() - 3]),
        ),
        (
            SealPurpose::Credentials,
            files.read_back("plain", PLAINTEXT.as_bytes()),
        ),
        (SealPurpose::Credentials, garbage_after_header(&files)),
    ];
    for (purpose, input) in &cases {
        let error = open_error(*purpose, input);
        assert_no_secret(&error.to_string(), input.as_bytes());
        assert_no_secret(&format!("{error:?}"), input.as_bytes());
    }
    let empty = seal::seal(
        SealPurpose::Credentials,
        &ad(),
        &CredentialBlob::new(String::new()),
    )
    .expect_err("empty text is refused");
    assert_no_secret(&empty.to_string(), bytes);
    assert_no_secret(&format!("{empty:?}"), bytes);
}
