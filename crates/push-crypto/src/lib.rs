//! RFC 8291 `aes128gcm` push message encryption and decryption: one record
//! (RFC 8188) keyed from an ECDH agreement with the subscription's P-256 key
//! and its authentication secret.
//!
//! One crate so the daemon, which encrypts a notice for a subscription, and the
//! phone app's notification extension, which decrypts it, share one
//! implementation, proved by the one RFC 8291 Appendix A vector.

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
use sha2::Sha256;

/// The record size written into the header: the one record a push message
/// carries (RFC 8291 §4).
const RECORD_SIZE: u32 = 4096;

/// The header's octets: the 16-octet salt, the 4-octet record size, the
/// `idlen` octet and the 65-octet `keyid` (RFC 8188 §2.1, RFC 8291 §4).
const HEADER_LEN: usize = 86;

/// The least a message's record can take after the header: the AES-GCM tag and
/// the last-record delimiter (RFC 8188 §2, RFC 8291 §4).
const MIN_RECORD_LEN: usize = 17;

/// The `idlen` a Web Push message writes: the 65 octets of the sender's
/// uncompressed P-256 public key (RFC 8291 §4).
const KEYID_LEN: u8 = 65;

/// The octet that ends the one record a push message carries (RFC 8188 §2).
const LAST_RECORD: u8 = 0x02;

/// The largest plaintext one 4096-octet record holds: 4096 less the 86-octet
/// header, the padding delimiter and the AES-GCM tag (RFC 8291 §4).
pub const MAX_PLAINTEXT: usize = 3993;

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
pub struct Subscriber {
    /// The user agent's uncompressed P-256 public key.
    pub p256dh: [u8; 65],
    /// The user agent's 16-octet authentication secret.
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
    pub fn from_b64url(p256dh: &str, auth: &str) -> anyhow::Result<Self> {
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
pub fn encrypt(sub: &Subscriber, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
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
    record.push(LAST_RECORD);
    let ciphertext = Aes128Gcm::new(&cek.into())
        .encrypt(&nonce.into(), record.as_slice())
        .map_err(|_| anyhow::anyhow!("AES-128-GCM refused the record"))?;

    let mut body = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(KEYID_LEN);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

/// Decrypts a push message body written for the subscription whose private key
/// is `ua_private_key` (the raw 32-octet P-256 scalar) and whose
/// authentication secret is `auth`: the user agent's side of RFC 8291 §3.4.
///
/// # Errors
///
/// `body` is shorter than a header and its record, its `idlen` octet is not
/// 65, its `keyid` is not a P-256 point, its record is longer than the header's
/// record size, `ua_private_key` is not a valid P-256 scalar, the record does
/// not open under that key and secret, or the decrypted record's last non-zero
/// octet is not the `0x02` last-record delimiter.
pub fn decrypt(body: &[u8], ua_private_key: &[u8; 32], auth: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let header = Header::parse(body)?;
    let (cek, nonce) = header.derive(ua_private_key, auth)?;
    let record = Aes128Gcm::new(&cek.into())
        .decrypt(&nonce.into(), header.ciphertext)
        .map_err(|_| {
            anyhow::anyhow!(
                "the record did not open: the key, secret or body is not the one it was written with"
            )
        })?;
    Ok(strip_padding(&record)?.to_vec())
}

/// The `aes128gcm` header of one message, and the record read after it
/// (RFC 8188 §2.1).
struct Header<'a> {
    salt: [u8; 16],
    keyid: &'a [u8],
    ciphertext: &'a [u8],
}

impl<'a> Header<'a> {
    /// Splits a message body into the header's fields and its record.
    ///
    /// # Errors
    ///
    /// The body is shorter than a header and its record, the header has no
    /// 16-octet salt, record size, `idlen` octet or keyid, the `idlen` octet is
    /// not 65, or the record is longer than the header's record size.
    fn parse(body: &'a [u8]) -> anyhow::Result<Self> {
        let shortest = HEADER_LEN + MIN_RECORD_LEN;
        if body.len() < shortest {
            bail!(
                "a push message body of {} octets is shorter than the {shortest} a header and its record take",
                body.len()
            );
        }
        let (header, ciphertext) = body
            .split_at_checked(HEADER_LEN)
            .context("a push message body has no header")?;
        let salt = header
            .get(..16)
            .and_then(|octets| <[u8; 16]>::try_from(octets).ok())
            .context("the message's header has no 16-octet salt")?;
        let record_size = header
            .get(16..20)
            .and_then(|octets| <[u8; 4]>::try_from(octets).ok())
            .map(u32::from_be_bytes)
            .context("the message's header has no record size")?;
        let idlen = *header
            .get(20)
            .context("the message's header has no idlen octet")?;
        if idlen != KEYID_LEN {
            bail!("the message's idlen octet is {idlen}, not {KEYID_LEN}");
        }
        let keyid = header
            .get(21..HEADER_LEN)
            .context("the message's header has no keyid")?;
        let record_len = u32::try_from(ciphertext.len())
            .context("the message's record is longer than any record size")?;
        if record_len > record_size {
            bail!("the message's record of {record_len} octets is over its {record_size}");
        }
        Ok(Self {
            salt,
            keyid,
            ciphertext,
        })
    }

    /// Agrees on the content key and nonce with the sender's ephemeral key
    /// (RFC 8291 §3.4).
    ///
    /// # Errors
    ///
    /// `ua_private_key` is not a valid P-256 scalar, the `keyid` is not a
    /// P-256 point, or HKDF refuses one of the derived lengths.
    fn derive(
        &self,
        ua_private_key: &[u8; 32],
        auth: &[u8; 16],
    ) -> anyhow::Result<([u8; 16], [u8; 12])> {
        let ua_secret = SecretKey::from_slice(ua_private_key)
            .map_err(|_| anyhow::anyhow!("the user agent's private key is not a P-256 scalar"))?;
        let as_public = PublicKey::from_sec1_bytes(self.keyid)
            .context("the message's keyid is not a P-256 point")?;
        let ecdh = ua_secret.diffie_hellman(&as_public);
        let ua_public = ua_secret.public_key().to_sec1_point(false);

        let mut key_info = Vec::with_capacity(14 + 65 + 65);
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(ua_public.as_bytes());
        key_info.extend_from_slice(self.keyid);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), ecdh.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .map_err(|_| anyhow::anyhow!("HKDF refused the key-combining length"))?;

        let prk = Hkdf::<Sha256>::new(Some(&self.salt), &ikm);
        let mut cek = [0u8; 16];
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .map_err(|_| anyhow::anyhow!("HKDF refused the content key length"))?;
        let mut nonce = [0u8; 12];
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
            .map_err(|_| anyhow::anyhow!("HKDF refused the nonce length"))?;
        Ok((cek, nonce))
    }
}

/// Strips a decrypted record's zero padding and its last-record delimiter,
/// returning the plaintext before them (RFC 8188 §2).
///
/// # Errors
///
/// The record is all zero padding, or its last non-zero octet is not the
/// `0x02` last-record delimiter.
fn strip_padding(record: &[u8]) -> anyhow::Result<&[u8]> {
    let Some(last) = record.iter().rposition(|octet| *octet != 0) else {
        bail!("the decrypted record is all zero padding, with no delimiter");
    };
    match record.get(last).copied() {
        Some(LAST_RECORD) => record
            .get(..last)
            .context("the decrypted record has no plaintext"),
        Some(0x01) => bail!("the decrypted record is not the last record (delimiter 0x01)"),
        Some(_) => bail!(
            "the decrypted record's last non-zero octet is not the 0x02 last-record delimiter"
        ),
        None => bail!("the decrypted record has no octet at its delimiter position"),
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use super::{
        Aead, Aes128Gcm, HEADER_LEN, Header, KEYID_LEN, KeyInit, MAX_PLAINTEXT, RECORD_SIZE,
        SecretKey, Subscriber, ToSec1Point, decrypt, encrypt, encrypt_with,
    };
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    fn b64(text: &str) -> Vec<u8> {
        URL_SAFE_NO_PAD.decode(text).expect("valid base64url")
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

    fn rfc_ua_secret() -> SecretKey {
        SecretKey::from_slice(&b64(UA_PRIVATE)).expect("the RFC's receiver key")
    }

    /// The RFC's receiver key as the notification extension holds it.
    fn rfc_ua_private() -> [u8; 32] {
        b64(UA_PRIVATE).try_into().expect("32 octets")
    }

    fn rfc_auth() -> [u8; 16] {
        b64(AUTH_SECRET).try_into().expect("16 octets")
    }

    fn assert_no_plaintext(err: &anyhow::Error) {
        let message = err.to_string();
        let plaintext = std::str::from_utf8(PLAINTEXT).expect("the RFC's plaintext is UTF-8");
        assert!(
            !message.contains(plaintext),
            "the error prints the plaintext: {message}"
        );
    }

    /// Seals `record` as RFC 8291's sender would, from the RFC's fixed sender
    /// key and salt, with no delimiter added: the receiver's own agreement
    /// derives the sender's key, since both sides agree on the same secret.
    fn seal_raw(record: &[u8]) -> Vec<u8> {
        let salt: [u8; 16] = b64(SALT).try_into().expect("16 octets");
        let keyid = b64(AS_PUBLIC);
        let header = Header {
            salt,
            keyid: &keyid,
            ciphertext: &[],
        };
        let (cek, nonce) = header
            .derive(&rfc_ua_private(), &rfc_auth())
            .expect("the RFC's keys agree");
        let ciphertext = Aes128Gcm::new(&cek.into())
            .encrypt(&nonce.into(), record)
            .expect("seals the record");
        let mut body = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        body.extend_from_slice(&salt);
        body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
        body.push(KEYID_LEN);
        body.extend_from_slice(&keyid);
        body.extend_from_slice(&ciphertext);
        body
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
    fn rfc8291_appendix_a_vector_decrypts() {
        let plaintext =
            decrypt(&b64(BODY), &rfc_ua_private(), &rfc_auth()).expect("the RFC's body opens");
        assert_eq!(plaintext, PLAINTEXT);
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
        let ua_private = rfc_ua_private();
        let auth = sub.auth;
        assert_eq!(
            decrypt(&b64(BODY), &ua_private, &auth).expect("the RFC's body opens"),
            PLAINTEXT
        );

        let first = encrypt(&sub, PLAINTEXT).expect("encrypts");
        let second = encrypt(&sub, PLAINTEXT).expect("encrypts");
        assert_ne!(first, second, "a fresh key and salt each time");
        assert_eq!(
            decrypt(&first, &ua_private, &auth).expect("the first body opens"),
            PLAINTEXT
        );
        assert_eq!(
            decrypt(&second, &ua_private, &auth).expect("the second body opens"),
            PLAINTEXT
        );
    }

    #[test]
    fn decrypt_refuses_a_short_body() {
        let body = b64(BODY);
        let short = body.get(..102).expect("the RFC's body is over 102 octets");
        let err = decrypt(short, &rfc_ua_private(), &rfc_auth())
            .expect_err("a 102-octet body is refused");
        assert_no_plaintext(&err);
    }

    #[test]
    fn decrypt_refuses_a_tampered_record() {
        let mut body = b64(BODY);
        let octet = body.get_mut(HEADER_LEN).expect("the body has a record");
        *octet ^= 0x01;
        let err = decrypt(&body, &rfc_ua_private(), &rfc_auth())
            .expect_err("a flipped octet does not open");
        assert_no_plaintext(&err);
    }

    #[test]
    fn decrypt_refuses_the_wrong_auth_secret() {
        let err = decrypt(&b64(BODY), &rfc_ua_private(), &[0xab; 16])
            .expect_err("the wrong auth secret does not open");
        assert_no_plaintext(&err);
    }

    #[test]
    fn decrypt_refuses_an_idlen_other_than_65() {
        let mut body = b64(BODY);
        let idlen = body.get_mut(20).expect("the body has an idlen octet");
        *idlen = 64;
        let err =
            decrypt(&body, &rfc_ua_private(), &rfc_auth()).expect_err("an idlen of 64 is refused");
        assert_no_plaintext(&err);
    }

    #[test]
    fn decrypt_refuses_a_record_without_a_delimiter_and_does_not_print_it() {
        let body = seal_raw(b"When I grow up, I want to be a watermelZ");
        let err = decrypt(&body, &rfc_ua_private(), &rfc_auth())
            .expect_err("a record with no delimiter is refused");
        let message = err.to_string();
        assert!(
            !message.contains("0x5a"),
            "the error prints the octet: {message}"
        );
        assert!(
            !message.contains('Z'),
            "the error prints the plaintext: {message}"
        );
    }

    #[test]
    fn decrypt_refuses_a_record_that_is_not_the_last() {
        let mut record = PLAINTEXT.to_vec();
        record.push(0x01);
        let body = seal_raw(&record);
        let err = decrypt(&body, &rfc_ua_private(), &rfc_auth())
            .expect_err("a 0x01 delimiter is refused");
        assert!(err.to_string().contains("not the last record"), "{err}");
        assert_no_plaintext(&err);
    }
}
