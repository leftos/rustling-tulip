//! RFC 8291 message encryption for Web Push: one `aes128gcm` record
//! (RFC 8188) keyed from an ECDH agreement with the subscription's P-256 key
//! and its authentication secret.

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{Context, bail};
use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use hkdf::Hkdf;
use p256::elliptic_curve::Generate;
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use sha2_011::Sha256;

/// The record size written into the header: the one record a push message
/// carries (RFC 8291 §4).
const RECORD_SIZE: u32 = 4096;

/// The largest plaintext one 4096-octet record holds: 4096 less the 86-octet
/// header, the padding delimiter and the AES-GCM tag (RFC 8291 §4).
pub(crate) const MAX_PLAINTEXT: usize = 3993;

/// URL-safe base64 that reads keys with or without `=` padding: browsers
/// differ in how a page's encoding of the subscription keys ends.
const URL_SAFE_ANY_PADDING: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The keys a subscription hands the application server: the user agent's
/// uncompressed P-256 public key and its 16-octet authentication secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subscriber {
    pub p256dh: [u8; 65],
    pub auth: [u8; 16],
}

impl Subscriber {
    /// Reads a subscription's `p256dh` and `auth` keys, each URL-safe base64
    /// with or without `=` padding, as the page sends them.
    ///
    /// # Errors
    ///
    /// Either key is not base64url, `p256dh` is not 65 octets or not an
    /// uncompressed point on P-256, or `auth` is not 16 octets.
    pub(crate) fn from_b64url(p256dh: &str, auth: &str) -> anyhow::Result<Self> {
        let p256dh_bytes = URL_SAFE_ANY_PADDING
            .decode(p256dh)
            .context("the subscription's p256dh key is not base64url")?;
        let auth_bytes = URL_SAFE_ANY_PADDING
            .decode(auth)
            .context("the subscription's auth secret is not base64url")?;
        let p256dh: [u8; 65] = p256dh_bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "the subscription's p256dh key is {} octets, not 65",
                p256dh_bytes.len()
            )
        })?;
        let auth: [u8; 16] = auth_bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "the subscription's auth secret is {} octets, not 16",
                auth_bytes.len()
            )
        })?;
        if p256dh[0] != 0x04 || PublicKey::from_sec1_bytes(&p256dh).is_err() {
            bail!("the subscription's p256dh key is not an uncompressed P-256 point");
        }
        Ok(Self { p256dh, auth })
    }
}

/// Encrypts `plaintext` for `sub` with a fresh ephemeral key and salt.
///
/// # Errors
///
/// `plaintext` is longer than [`MAX_PLAINTEXT`], `sub`'s key is not a P-256
/// point, or the system's random source failed.
pub(crate) fn encrypt(sub: &Subscriber, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let as_secret = SecretKey::try_generate()
        .map_err(|err| anyhow::anyhow!("no random ephemeral key: {err}"))?;
    let salt =
        <[u8; 16]>::try_generate().map_err(|err| anyhow::anyhow!("no random salt: {err}"))?;
    encrypt_with(sub, plaintext, &as_secret, salt)
}

/// Encrypts `plaintext` for `sub` with the application server's ephemeral
/// key `as_secret` and `salt`, returning the whole message body: the 86-octet
/// header followed by the one record (RFC 8291 §3.4).
fn encrypt_with(
    sub: &Subscriber,
    plaintext: &[u8],
    as_secret: &SecretKey,
    salt: [u8; 16],
) -> anyhow::Result<Vec<u8>> {
    if plaintext.len() > MAX_PLAINTEXT {
        bail!(
            "a push payload of {} octets is over the {MAX_PLAINTEXT} one record holds",
            plaintext.len()
        );
    }
    let ua_public = PublicKey::from_sec1_bytes(&sub.p256dh)
        .context("the subscription's p256dh key is not a P-256 point")?;
    let as_public = as_secret.public_key().to_sec1_point(false);
    let ecdh = as_secret.diffie_hellman(&ua_public);

    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(&sub.p256dh);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&sub.auth), ecdh.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("HKDF refused the key-combining length"))?;

    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow::anyhow!("HKDF refused the content key length"))?;
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow::anyhow!("HKDF refused the nonce length"))?;

    let mut record = Vec::with_capacity(plaintext.len() + 1 + 16);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ciphertext = Aes128Gcm::new(&cek.into())
        .encrypt(&nonce.into(), record.as_slice())
        .map_err(|_| anyhow::anyhow!("AES-128-GCM refused the record"))?;

    let mut body = Vec::with_capacity(86 + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(65);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        Aead, Aes128Gcm, Hkdf, KeyInit, MAX_PLAINTEXT, PublicKey, SecretKey, Sha256, Subscriber,
        ToSec1Point, encrypt, encrypt_with,
    };
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).expect("valid base64url")
    }

    /// The user agent's side of RFC 8291 §3.4: reads the header, agrees on
    /// the secret with `ua_secret`, and opens the one record.
    fn decrypt(body: &[u8], ua_secret: &SecretKey, auth: &[u8; 16]) -> Vec<u8> {
        let (header, ciphertext) = body.split_at(86);
        let salt = &header[..16];
        assert_eq!(header[16..20], 4096u32.to_be_bytes());
        assert_eq!(header[20], 65);
        let keyid = &header[21..86];
        let as_public = PublicKey::from_sec1_bytes(keyid).expect("keyid is a P-256 point");
        let ecdh = ua_secret.diffie_hellman(&as_public);
        let ua_public = ua_secret.public_key().to_sec1_point(false);

        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(ua_public.as_bytes());
        key_info.extend_from_slice(keyid);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), ecdh.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .expect("expands");
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .expect("expands");
        let mut nonce = [0u8; 12];
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .expect("expands");

        let mut record = Aes128Gcm::new(&cek.into())
            .decrypt(&nonce.into(), ciphertext)
            .expect("the record opens");
        assert_eq!(record.pop(), Some(0x02), "the padding delimiter");
        record
    }

    fn rfc_ua_secret() -> SecretKey {
        SecretKey::from_slice(&b64(UA_PRIVATE)).expect("the RFC's receiver key")
    }

    // RFC 8291 Appendix A, its whitespace removed.
    const PLAINTEXT_B64: &str = "V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24";
    const AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const AS_PRIVATE: &str = "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
    const UA_PRIVATE: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const AUTH_SECRET: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const HEADER: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const CIPHERTEXT: &str =
        "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ";
    // RFC 8291 §5, the body the appendix's header and ciphertext concatenate to.
    const BODY: &str = "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN";

    fn rfc_subscriber() -> Subscriber {
        Subscriber {
            p256dh: b64(UA_PUBLIC).try_into().expect("65 octets"),
            auth: b64(AUTH_SECRET).try_into().expect("16 octets"),
        }
    }

    fn rfc_as_secret() -> SecretKey {
        SecretKey::from_slice(&b64(AS_PRIVATE)).expect("the RFC's sender key")
    }

    const PLAINTEXT: &[u8] = b"When I grow up, I want to be a watermelon";

    #[test]
    fn rfc8291_appendix_a_vector() {
        assert_eq!(b64(PLAINTEXT_B64), PLAINTEXT);
        let salt: [u8; 16] = b64(SALT).try_into().expect("16 octets");
        let as_secret = rfc_as_secret();
        assert_eq!(
            as_secret.public_key().to_sec1_point(false).as_bytes(),
            b64(AS_PUBLIC)
        );
        assert_eq!(
            rfc_ua_secret().public_key().to_sec1_point(false).as_bytes(),
            b64(UA_PUBLIC)
        );

        let body = encrypt_with(&rfc_subscriber(), PLAINTEXT, &as_secret, salt).expect("encrypts");

        let mut expected = b64(HEADER);
        expected.extend(b64(CIPHERTEXT));
        assert_eq!(expected, b64(BODY));
        assert_eq!(body, b64(BODY));
    }

    #[test]
    fn padded_subscriber_keys_are_read() {
        let p256dh = format!("{UA_PUBLIC}=");
        let auth = format!("{AUTH_SECRET}==");
        assert_eq!(
            Subscriber::from_b64url(&p256dh, &auth).expect("padded keys decode"),
            rfc_subscriber()
        );
    }

    #[test]
    fn subscriber_keys_are_read_and_checked() {
        assert_eq!(
            Subscriber::from_b64url(UA_PUBLIC, AUTH_SECRET).expect("the RFC's keys"),
            rfc_subscriber()
        );

        let short_p256dh = URL_SAFE_NO_PAD.encode(&b64(UA_PUBLIC)[..64]);
        let err = Subscriber::from_b64url(&short_p256dh, AUTH_SECRET)
            .expect_err("a 64-octet p256dh is refused");
        assert!(err.to_string().contains("p256dh"), "{err}");

        let short_auth = URL_SAFE_NO_PAD.encode(&b64(AUTH_SECRET)[..15]);
        let err = Subscriber::from_b64url(UA_PUBLIC, &short_auth)
            .expect_err("a 15-octet auth is refused");
        assert!(err.to_string().contains("auth"), "{err}");

        let mut off_curve = [0u8; 65];
        off_curve[0] = 0x04;
        let err = Subscriber::from_b64url(&URL_SAFE_NO_PAD.encode(off_curve), AUTH_SECRET)
            .expect_err("a point off the curve is refused");
        assert!(
            err.to_string().contains("p256dh") && err.to_string().contains("P-256 point"),
            "{err}"
        );
    }

    #[test]
    fn plaintext_over_one_record_is_refused() {
        let sub = rfc_subscriber();
        let full = encrypt(&sub, &[b'a'; MAX_PLAINTEXT]).expect("3993 octets fit");
        assert_eq!(full.len(), 4096);
        let err = encrypt(&sub, &[b'a'; MAX_PLAINTEXT + 1]).expect_err("3994 octets do not");
        assert!(err.to_string().contains("3994"), "{err}");
    }

    #[test]
    fn encrypt_output_decrypts_back() {
        let sub = rfc_subscriber();
        let ua_secret = rfc_ua_secret();
        assert_eq!(decrypt(&b64(BODY), &ua_secret, &sub.auth), PLAINTEXT);

        let first = encrypt(&sub, PLAINTEXT).expect("encrypts");
        let second = encrypt(&sub, PLAINTEXT).expect("encrypts");
        assert_ne!(first, second, "a fresh key and salt each time");
        assert_eq!(decrypt(&first, &ua_secret, &sub.auth), PLAINTEXT);
        assert_eq!(decrypt(&second, &ua_secret, &sub.auth), PLAINTEXT);
    }
}
