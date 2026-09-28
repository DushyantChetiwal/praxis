//! The cryptography of Praxis Remote, exactly as
//! `docs/src/ai/praxis-remote-protocol.md` specifies it. The Android app
//! implements the same thing, and both are checked against the test vectors
//! in that document.

use anyhow::{Context as _, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ring::{
    aead, agreement, digest, hkdf,
    rand::{SecureRandom as _, SystemRandom},
};

pub(super) const PROTOCOL: &str = "praxis-remote/v2";
pub(super) const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const PUBLIC_KEY_LEN: usize = 65;

/// The AES-256-GCM key shared with one phone.
pub(super) type Key = [u8; KEY_LEN];

/// This computer's key pair for one pairing. The private half is used once,
/// for that pairing, and then dropped.
pub(super) struct PairingKey {
    private: agreement::EphemeralPrivateKey,
    public: Vec<u8>,
}

/// What a pairing agrees: the key for the phone, and the code both screens
/// show so the user can check that nobody stood in between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PairingSecrets {
    pub key: Key,
    pub code: String,
}

impl PairingKey {
    pub(super) fn generate() -> Result<Self> {
        let rng = SystemRandom::new();
        let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
            .map_err(|_| anyhow!("could not create a pairing key"))?;
        let public = private
            .compute_public_key()
            .map_err(|_| anyhow!("could not create a pairing key"))?
            .as_ref()
            .to_vec();
        Ok(Self { private, public })
    }

    pub(super) fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Agrees the key and code with the phone that revealed `phone_public`.
    pub(super) fn agree_with_phone(
        self,
        channel: &str,
        phone_id: &str,
        phone_public: &[u8],
    ) -> Result<PairingSecrets> {
        let transcript = transcript_hash(channel, phone_id, phone_public, &self.public);
        self.agree(phone_public, &transcript)
    }

    fn agree(self, peer_public: &[u8], transcript: &[u8]) -> Result<PairingSecrets> {
        if peer_public.len() != PUBLIC_KEY_LEN {
            bail!("the other side's key is not a P-256 public key");
        }
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, peer_public);
        agreement::agree_ephemeral(self.private, &peer, |shared| derive(shared, transcript))
            .map_err(|_| anyhow!("the other side's key is not a valid P-256 public key"))?
    }
}

/// What a phone commits to before it sees this computer's key.
pub(super) fn commitment(phone_public: &[u8]) -> Vec<u8> {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"praxis-remote/v2/commit\0");
    context.update(phone_public);
    context.finish().as_ref().to_vec()
}

/// Whether a phone's revealed key is the one it committed to.
pub(super) fn matches_commitment(phone_public: &[u8], commit: &[u8]) -> bool {
    let expected = commitment(phone_public);
    commit.len() == expected.len()
        && expected
            .iter()
            .zip(commit)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

fn transcript_hash(
    channel: &str,
    phone_id: &str,
    phone_public: &[u8],
    desktop_public: &[u8],
) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(b"praxis-remote/v2/pair\0");
    context.update(channel.as_bytes());
    context.update(b"\0");
    context.update(phone_id.as_bytes());
    context.update(b"\0");
    context.update(phone_public);
    context.update(desktop_public);
    let mut hash = [0; 32];
    hash.copy_from_slice(context.finish().as_ref());
    hash
}

struct OutputLen(usize);

impl hkdf::KeyType for OutputLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn derive(shared: &[u8], transcript: &[u8]) -> Result<PairingSecrets> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript).extract(shared);
    let expand = |info: &[u8], out: &mut [u8]| -> Result<()> {
        prk.expand(&[info], OutputLen(out.len()))
            .and_then(|okm| okm.fill(out))
            .map_err(|_| anyhow!("could not derive the pairing key"))
    };
    let mut key = [0; KEY_LEN];
    expand(b"praxis-remote/v2/key", &mut key)?;
    let mut code = [0; 4];
    expand(b"praxis-remote/v2/code", &mut code)?;
    Ok(PairingSecrets {
        key,
        code: format!("{:06}", u32::from_be_bytes(code) % 1_000_000),
    })
}

/// A code as the screens show it: "807 021".
pub(super) fn display_code(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

pub(super) fn state_aad(channel: &str, phone_id: &str) -> String {
    format!("{PROTOCOL}/state/{channel}/{phone_id}")
}

pub(super) fn request_aad(channel: &str, phone_id: &str) -> String {
    format!("{PROTOCOL}/request/{channel}/{phone_id}")
}

pub(super) fn response_aad(channel: &str, phone_id: &str, request_id: &str) -> String {
    format!("{PROTOCOL}/response/{channel}/{phone_id}/{request_id}")
}

/// Encrypts `plaintext` into a blob: `base64(nonce || ciphertext || tag)`.
pub(super) fn seal(key: &Key, aad: &str, plaintext: &[u8]) -> Result<String> {
    let mut nonce = [0; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("could not create a nonce"))?;
    seal_with_nonce(key, nonce, aad, plaintext)
}

fn seal_with_nonce(
    key: &Key,
    nonce: [u8; NONCE_LEN],
    aad: &str,
    plaintext: &[u8],
) -> Result<String> {
    let mut sealed = plaintext.to_vec();
    aead_key(key)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_bytes()),
            &mut sealed,
        )
        .map_err(|_| anyhow!("could not encrypt the message"))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + sealed.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&sealed);
    Ok(BASE64.encode(blob))
}

/// Decrypts a blob, refusing anything tampered with or meant for elsewhere.
pub(super) fn open(key: &Key, aad: &str, blob: &str) -> Result<Vec<u8>> {
    let bytes = BASE64
        .decode(blob.trim())
        .context("the message is not base64")?;
    if bytes.len() < NONCE_LEN + TAG_LEN {
        bail!("the message is too short");
    }
    let (nonce, sealed) = bytes.split_at(NONCE_LEN);
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce)
        .map_err(|_| anyhow!("the message has no nonce"))?;
    let mut sealed = sealed.to_vec();
    let plaintext = aead_key(key)?
        .open_in_place(nonce, aead::Aad::from(aad.as_bytes()), &mut sealed)
        .map_err(|_| anyhow!("the message could not be decrypted"))?;
    Ok(plaintext.to_vec())
}

fn aead_key(key: &Key) -> Result<aead::LessSafeKey> {
    aead::UnboundKey::new(&aead::AES_256_GCM, key)
        .map(aead::LessSafeKey::new)
        .map_err(|_| anyhow!("the key is not an AES-256 key"))
}

pub(super) fn encode(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

pub(super) fn decode(text: &str) -> Result<Vec<u8>> {
    BASE64.decode(text.trim()).context("not base64")
}

pub(super) fn decode_key(text: &str) -> Result<Key> {
    decode(text)?
        .try_into()
        .map_err(|_| anyhow!("a saved phone key has the wrong length"))
}

/// 32 random lowercase hex digits, for a channel or phone id.
pub(super) fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("could not create a random id"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn is_id(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    // From the test vectors in docs/src/ai/praxis-remote-protocol.md.
    const CHANNEL: &str = "0123456789abcdef0123456789abcdef";
    const PHONE_ID: &str = "fedcba9876543210fedcba9876543210";
    const PHONE_PUBLIC: &str = "BGD+1LolWp0xyWHrdMY1bWjASbiSO2H6bOZpYi5g8p+2eQP+EAi4vJmkGunpVii8ZPLxsgwtfp9Rd6PClNRGIpk=";
    const DESKTOP_PUBLIC: &str = "BB79lpmknsBRWuk+JOVoyVb/iHJBHpv3ntw3J4jmMZLPcet9LblSM21sa00e9K3he8jl0LJoH0KUdF0rVly6OYY=";
    const COMMIT: &str = "OUHlaa0veVAxCFLJFkMhNYff3Kr3bKcvpSN+zQPJeLY=";
    const SHARED: &str = "e276d9ef83f4744188147d5ad3d2bc93a5bff1dbb1a079599e29b823154e85c7";
    const TRANSCRIPT: &str = "cfd62945d28a04d0bd609de1268684b94716abb7e19e9585bd8e4b73cc71748c";
    const KEY: &str = "da9dd9a4d0c328b1d923cc9b4635d7be4cba432a964aec4ca8a1300efacc6646";
    const CODE: &str = "807021";
    const PLAINTEXT: &str = r#"{"id":"r1","op":"status","args":{},"sent_at":"2026-01-01T00:00:00Z"}"#;
    const BLOB: &str = "AAECAwQFBgcICQoLUqeO9tn6GdeJsQhhTZTZJ534qUbzV9GRWtzN83LowPpUOoA3QnYETibYcB6JwcZqAKMwPp0/eA4COaYU6ZzNZ95NG59AAfXNKN8SIyHZ302C2I4Z";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
            .collect()
    }

    fn vector_key() -> Key {
        hex(KEY).try_into().expect("32 bytes")
    }

    #[test]
    fn the_pairing_matches_the_specification() {
        let phone_public = decode(PHONE_PUBLIC).expect("base64");
        let desktop_public = decode(DESKTOP_PUBLIC).expect("base64");
        assert_eq!(encode(&commitment(&phone_public)), COMMIT);
        let commit = decode(COMMIT).expect("base64");
        assert!(matches_commitment(&phone_public, &commit));
        assert!(!matches_commitment(&desktop_public, &commit));

        let transcript = transcript_hash(CHANNEL, PHONE_ID, &phone_public, &desktop_public);
        assert_eq!(transcript.to_vec(), hex(TRANSCRIPT));
        let secrets = derive(&hex(SHARED), &transcript).expect("derives");
        assert_eq!(secrets.key.to_vec(), hex(KEY));
        assert_eq!(secrets.code, CODE);
        assert_eq!(display_code(&secrets.code), "807 021");
    }

    #[test]
    fn messages_match_the_specification() {
        let key = vector_key();
        let aad = request_aad(CHANNEL, PHONE_ID);
        let nonce = hex("000102030405060708090a0b");
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("12 bytes");
        let sealed = seal_with_nonce(&key, nonce, &aad, PLAINTEXT.as_bytes());
        assert_eq!(sealed.expect("seals"), BLOB);
        let opened = open(&key, &aad, BLOB).expect("opens");
        assert_eq!(opened, PLAINTEXT.as_bytes());
    }

    #[test]
    fn a_message_is_refused_if_changed_or_moved() {
        let key = vector_key();
        let aad = request_aad(CHANNEL, PHONE_ID);
        let sealed = seal(&key, &aad, b"hello").expect("seals");
        assert_eq!(open(&key, &aad, &sealed).expect("opens"), b"hello");

        let mut bytes = decode(&sealed).expect("base64");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(open(&key, &aad, &encode(&bytes)).is_err(), "tampering");
        assert!(
            open(&key, &state_aad(CHANNEL, PHONE_ID), &sealed).is_err(),
            "a request cannot be passed off as something else"
        );
        assert!(
            open(&key, &request_aad(CHANNEL, CHANNEL), &sealed).is_err(),
            "nor as another phone's"
        );
        assert!(open(&key, &aad, "AAAA").is_err(), "too short");
        assert_ne!(
            seal(&key, &aad, b"hello").expect("seals"),
            sealed,
            "every message gets a fresh nonce"
        );
    }

    #[test]
    fn both_sides_of_a_pairing_agree() {
        let phone = PairingKey::generate().expect("a key");
        let desktop = PairingKey::generate().expect("a key");
        let phone_public = phone.public_key().to_vec();
        let desktop_public = desktop.public_key().to_vec();

        let on_desktop = desktop
            .agree_with_phone(CHANNEL, PHONE_ID, &phone_public)
            .expect("agrees");
        let transcript = transcript_hash(CHANNEL, PHONE_ID, &phone_public, &desktop_public);
        let on_phone = phone.agree(&desktop_public, &transcript).expect("agrees");
        assert_eq!(on_desktop, on_phone);
        assert_eq!(on_desktop.code.len(), 6);

        let stranger = PairingKey::generate().expect("a key");
        assert!(
            stranger
                .agree_with_phone(CHANNEL, PHONE_ID, &phone_public[..64])
                .is_err(),
            "a truncated key is refused"
        );
    }

    #[test]
    fn ids_are_32_lowercase_hex_digits() {
        let id = random_id().expect("an id");
        assert!(is_id(&id));
        assert_ne!(random_id().expect("an id"), id);
        assert!(is_id(CHANNEL));
        assert!(!is_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_id("0123"));
        assert!(!is_id("x123456789abcdef0123456789abcdef"));
    }
}
