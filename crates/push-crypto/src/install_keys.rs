//! The phone install's push keys as the Keychain holds them: one generic
//! password item, shared by the app, which writes it, and its notification
//! extension, which reads it.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::SecretKey;
use p256::elliptic_curve::Generate as _;
use p256::elliptic_curve::sec1::ToSec1Point as _;

/// The Keychain service of the generic password item that holds an install's
/// push keys.
pub const KEYCHAIN_SERVICE: &str = "dev.leftos.rustling-tulip.push";

/// The Keychain account of the generic password item that holds an install's
/// push keys.
pub const KEYCHAIN_ACCOUNT: &str = "install-keys";

/// The octets the Keychain item holds: the private scalar, then the secret.
const ITEM_LEN: usize = 48;

/// One install's push keys: the raw 32-octet P-256 private scalar its
/// subscription's `p256dh` key derives from, and its 16-octet authentication
/// secret. `Debug` never prints either.
#[derive(Clone, PartialEq, Eq)]
pub struct InstallKeys {
    /// The raw P-256 private scalar.
    pub private_key: [u8; 32],
    /// The authentication secret.
    pub auth: [u8; 16],
}

impl InstallKeys {
    /// A new pair: a random P-256 private scalar and a random 16-octet
    /// authentication secret, from the system's random source.
    ///
    /// # Errors
    ///
    /// The system's random source failed.
    pub fn generate() -> anyhow::Result<Self> {
        let secret = SecretKey::try_generate()
            .map_err(|err| anyhow::anyhow!("no random push key: {err}"))?;
        let auth = <[u8; 16]>::try_generate()
            .map_err(|err| anyhow::anyhow!("no random push secret: {err}"))?;
        let private_key = <[u8; 32]>::try_from(secret.to_bytes().as_slice())
            .map_err(|_| anyhow::anyhow!("the generated push key is not 32 octets"))?;
        Ok(Self { private_key, auth })
    }

    /// The `p256dh` key the wire takes: the uncompressed 65-octet SEC1 public
    /// point of the private scalar, URL-safe base64 without padding.
    ///
    /// # Errors
    ///
    /// The private scalar is not a valid P-256 key (zero, or not below the
    /// curve's order).
    pub fn p256dh_b64url(&self) -> anyhow::Result<String> {
        let secret = SecretKey::from_slice(&self.private_key)
            .map_err(|_| anyhow::anyhow!("the install's push key is not a P-256 key"))?;
        let point = secret.public_key().to_sec1_point(false);
        Ok(URL_SAFE_NO_PAD.encode(point.as_bytes()))
    }

    /// The `auth` secret the wire takes: URL-safe base64 without padding.
    #[must_use]
    pub fn auth_b64url(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.auth)
    }

    /// The Keychain item's 48 octets: the private scalar, then the secret.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; 48] {
        let mut bytes = [0u8; ITEM_LEN];
        let (scalar, secret) = bytes.split_at_mut(32);
        scalar.copy_from_slice(&self.private_key);
        secret.copy_from_slice(&self.auth);
        bytes
    }

    /// Reads the Keychain item's octets as [`Self::to_bytes`] writes them.
    ///
    /// # Errors
    ///
    /// The item is not 48 octets long; the error names the length only.
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.len() != ITEM_LEN {
            anyhow::bail!(
                "the install's push keys item is {} octets, not {ITEM_LEN}",
                bytes.len()
            );
        }
        let (scalar, secret) = bytes.split_at(32);
        let private_key = <[u8; 32]>::try_from(scalar)
            .map_err(|_| anyhow::anyhow!("the install's push keys item has no 32-octet key"))?;
        let auth = <[u8; 16]>::try_from(secret)
            .map_err(|_| anyhow::anyhow!("the install's push keys item has no 16-octet secret"))?;
        Ok(Self { private_key, auth })
    }
}

impl fmt::Debug for InstallKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstallKeys").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::InstallKeys;

    fn keys() -> InstallKeys {
        let mut private_key = [0u8; 32];
        for (octet, value) in private_key.iter_mut().zip(1u8..) {
            *octet = value;
        }
        InstallKeys {
            private_key,
            auth: [0xa5; 16],
        }
    }

    #[test]
    fn install_keys_round_trip() {
        let bytes = keys().to_bytes();
        assert_eq!(bytes.len(), 48);
        assert_eq!(bytes.get(..32), Some(keys().private_key.as_slice()));
        assert_eq!(bytes.get(32..), Some(keys().auth.as_slice()));
        let read = InstallKeys::from_bytes(&bytes).expect("48 octets read back");
        assert_eq!(read.private_key, keys().private_key);
        assert_eq!(read.auth, keys().auth);
    }

    #[test]
    fn install_keys_refuse_any_length_but_48() {
        let bytes = keys().to_bytes();
        for len in [0, 1, 16, 32, 47] {
            let short = bytes.get(..len).expect("a prefix of the item");
            let err = InstallKeys::from_bytes(short).expect_err("a short item is refused");
            assert!(err.to_string().contains(&len.to_string()), "{err}");
        }
        let mut long = bytes.to_vec();
        long.push(0);
        let err = InstallKeys::from_bytes(&long).expect_err("a 49-octet item is refused");
        assert!(err.to_string().contains("49"), "{err}");
        assert!(
            !err.to_string().contains("165"),
            "the error prints an octet of the key: {err}"
        );
    }

    #[test]
    fn generated_keys_round_trip_through_the_subscriber_parse() {
        let keys = InstallKeys::generate().expect("the system's random source works");
        let p256dh = keys
            .p256dh_b64url()
            .expect("a generated key has a public key");
        let auth = keys.auth_b64url();
        assert!(!p256dh.contains('='), "no padding: {p256dh}");
        assert!(!auth.contains('='), "no padding: {auth}");
        let sub =
            crate::Subscriber::from_b64url(&p256dh, &auth).expect("the wire's parse reads it");
        assert_eq!(sub.auth, keys.auth);
        assert_eq!(sub.p256dh.first(), Some(&0x04), "an uncompressed point");
        let body = crate::encrypt(&sub, b"a notice").expect("encrypt to the subscriber");
        let plain =
            crate::decrypt(&body, &keys.private_key, &keys.auth).expect("the keys decrypt it");
        assert_eq!(plain, b"a notice");
    }

    #[test]
    fn generated_keys_differ() {
        let first = InstallKeys::generate().expect("a first pair");
        let second = InstallKeys::generate().expect("a second pair");
        assert_ne!(first.private_key, second.private_key);
        assert_ne!(first.auth, second.auth);
    }

    #[test]
    fn install_keys_debug_hides_the_key() {
        let shown = format!("{:?}", keys());
        assert!(shown.contains("InstallKeys"), "{shown}");
        for needle in ["165", "0xa5", "a5", "[1, 2", "private_key: ["] {
            assert!(!shown.contains(needle), "Debug prints the key: {shown}");
        }
    }
}
