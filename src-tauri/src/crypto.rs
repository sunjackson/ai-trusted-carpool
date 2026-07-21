use base64::{engine::general_purpose, Engine as _};
use ring::{aead, hkdf};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::identity::DeviceIdentity;

const ENVELOPE_VERSION: u8 = 1;
const ACCESS_PURPOSE: &str = "carpool_access";
const SIGNAL_PURPOSE: &str = "carpool_signal";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    pub version: u8,
    pub purpose: String,
    pub sender_peer_id: String,
    pub recipient_peer_id: String,
    pub ephemeral_public_key: String,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

struct Aes256KeyLength;

impl hkdf::KeyType for Aes256KeyLength {
    fn len(&self) -> usize {
        32
    }
}

fn decode_32(label: &str, value: &str) -> Result<[u8; 32], String> {
    let bytes = general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|error| format!("{label}格式无效: {error}"))?;
    bytes.try_into().map_err(|_| format!("{label}长度无效"))
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    use ring::rand::{SecureRandom, SystemRandom};

    let mut bytes = [0_u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法生成加密随机数".to_string())?;
    Ok(bytes)
}

fn aad(purpose: &str, sender_peer_id: &str, recipient_peer_id: &str) -> Vec<u8> {
    format!("trusted-carpool/v{ENVELOPE_VERSION}|{purpose}|{sender_peer_id}|{recipient_peer_id}")
        .into_bytes()
}

fn derive_key(purpose: &str, shared_secret: &[u8; 32], salt: &[u8]) -> Result<[u8; 32], String> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(shared_secret);
    let info_label = format!("trusted-carpool/{purpose}/aes-256-gcm");
    let info = [info_label.as_bytes()];
    let okm = prk
        .expand(&info, Aes256KeyLength)
        .map_err(|_| "无法派生加密密钥".to_string())?;
    let mut key = [0_u8; 32];
    okm.fill(&mut key)
        .map_err(|_| "无法读取加密密钥".to_string())?;
    Ok(key)
}

fn encrypt_with_purpose<T: Serialize>(
    purpose: &str,
    sender: &DeviceIdentity,
    recipient_peer_id: &str,
    recipient_encryption_public_key: &str,
    value: &T,
) -> Result<EncryptedEnvelope, String> {
    let recipient_public =
        X25519PublicKey::from(decode_32("对方加密公钥", recipient_encryption_public_key)?);
    let ephemeral_secret = StaticSecret::from(random_bytes::<32>()?);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);
    let shared = ephemeral_secret.diffie_hellman(&recipient_public);
    if !shared.was_contributory() {
        return Err("对方加密公钥无效".to_string());
    }

    let salt = random_bytes::<16>()?;
    let nonce_bytes = random_bytes::<12>()?;
    let key = derive_key(purpose, shared.as_bytes(), &salt)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| "无法创建加密器".to_string())?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext =
        serde_json::to_vec(value).map_err(|error| format!("无法编码加密内容: {error}"))?;
    key.seal_in_place_append_tag(
        nonce,
        aead::Aad::from(aad(purpose, &sender.peer_id, recipient_peer_id)),
        &mut ciphertext,
    )
    .map_err(|_| "无法加密内容".to_string())?;

    Ok(EncryptedEnvelope {
        version: ENVELOPE_VERSION,
        purpose: purpose.to_string(),
        sender_peer_id: sender.peer_id.clone(),
        recipient_peer_id: recipient_peer_id.to_string(),
        ephemeral_public_key: general_purpose::STANDARD.encode(ephemeral_public.as_bytes()),
        salt: general_purpose::STANDARD.encode(salt),
        nonce: general_purpose::STANDARD.encode(nonce_bytes),
        ciphertext: general_purpose::STANDARD.encode(ciphertext),
    })
}

fn decrypt_with_purpose<T: DeserializeOwned>(
    purpose: &str,
    recipient: &DeviceIdentity,
    expected_sender_peer_id: &str,
    envelope: &EncryptedEnvelope,
) -> Result<T, String> {
    if envelope.version != ENVELOPE_VERSION || envelope.purpose != purpose {
        return Err("加密信封版本或用途无效".to_string());
    }
    if envelope.sender_peer_id != expected_sender_peer_id {
        return Err("加密信封发送者身份不匹配".to_string());
    }
    if envelope.recipient_peer_id != recipient.peer_id {
        return Err("加密信封不是发给当前设备的".to_string());
    }

    let ephemeral_public =
        X25519PublicKey::from(decode_32("临时加密公钥", &envelope.ephemeral_public_key)?);
    let recipient_secret = StaticSecret::from(recipient.encryption_private_bytes()?);
    let shared = recipient_secret.diffie_hellman(&ephemeral_public);
    if !shared.was_contributory() {
        return Err("加密信封的临时公钥无效".to_string());
    }
    let salt = general_purpose::STANDARD
        .decode(envelope.salt.trim())
        .map_err(|error| format!("加密盐值无效: {error}"))?;
    let nonce_bytes: [u8; 12] = general_purpose::STANDARD
        .decode(envelope.nonce.trim())
        .map_err(|error| format!("加密随机数无效: {error}"))?
        .try_into()
        .map_err(|_| "加密随机数长度无效".to_string())?;
    let mut ciphertext = general_purpose::STANDARD
        .decode(envelope.ciphertext.trim())
        .map_err(|error| format!("密文无效: {error}"))?;
    let key = derive_key(purpose, shared.as_bytes(), &salt)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| "无法创建解密器".to_string())?;
    let key = aead::LessSafeKey::new(unbound);
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_bytes),
            aead::Aad::from(aad(purpose, expected_sender_peer_id, &recipient.peer_id)),
            &mut ciphertext,
        )
        .map_err(|_| "加密信封校验失败，内容可能被篡改".to_string())?;
    serde_json::from_slice(plaintext).map_err(|error| format!("加密内容无效: {error}"))
}

pub fn encrypt_access<T: Serialize>(
    sender: &DeviceIdentity,
    recipient_peer_id: &str,
    recipient_encryption_public_key: &str,
    value: &T,
) -> Result<EncryptedEnvelope, String> {
    encrypt_with_purpose(
        ACCESS_PURPOSE,
        sender,
        recipient_peer_id,
        recipient_encryption_public_key,
        value,
    )
}

pub fn decrypt_access<T: DeserializeOwned>(
    recipient: &DeviceIdentity,
    expected_sender_peer_id: &str,
    envelope: &EncryptedEnvelope,
) -> Result<T, String> {
    decrypt_with_purpose(ACCESS_PURPOSE, recipient, expected_sender_peer_id, envelope)
}

/// End-to-end encrypts a WebRTC signaling payload (SDP/ICE), so the
/// coordinator relays sealed envelopes and never sees session descriptions
/// or candidate IP addresses.
pub fn encrypt_signal(
    sender: &DeviceIdentity,
    recipient_peer_id: &str,
    recipient_encryption_public_key: &str,
    payload_json: &str,
) -> Result<EncryptedEnvelope, String> {
    encrypt_with_purpose(
        SIGNAL_PURPOSE,
        sender,
        recipient_peer_id,
        recipient_encryption_public_key,
        &payload_json.to_string(),
    )
}

pub fn decrypt_signal(
    recipient: &DeviceIdentity,
    expected_sender_peer_id: &str,
    envelope: &EncryptedEnvelope,
) -> Result<String, String> {
    decrypt_with_purpose(SIGNAL_PURPOSE, recipient, expected_sender_peer_id, envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Grant {
        access_id: String,
    }

    fn identity(path: &std::path::Path) -> DeviceIdentity {
        crate::identity::load_or_create_at(path).expect("identity")
    }

    #[test]
    fn access_envelope_is_device_bound_and_tamper_evident() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = identity(&directory.path().join("owner.json"));
        let passenger = identity(&directory.path().join("passenger.json"));
        let stranger = identity(&directory.path().join("stranger.json"));
        let grant = Grant {
            access_id: "access-1".to_string(),
        };
        let envelope = encrypt_access(
            &owner,
            &passenger.peer_id,
            &passenger.encryption_public_key,
            &grant,
        )
        .expect("encrypt");

        assert_eq!(
            decrypt_access::<Grant>(&passenger, &owner.peer_id, &envelope).expect("decrypt"),
            grant
        );
        assert!(decrypt_access::<Grant>(&stranger, &owner.peer_id, &envelope).is_err());

        let mut tampered = envelope;
        tampered.ciphertext.push('A');
        assert!(decrypt_access::<Grant>(&passenger, &owner.peer_id, &tampered).is_err());
    }

    #[test]
    fn signal_envelopes_hide_sdp_from_the_coordinator_and_stay_domain_separated() {
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = identity(&directory.path().join("owner.json"));
        let passenger = identity(&directory.path().join("passenger.json"));
        let payload = r#"{"sdp":{"type":"offer","sdp":"v=0 c=IN IP4 192.168.1.20"}}"#;
        let envelope = encrypt_signal(
            &passenger,
            &owner.peer_id,
            &owner.encryption_public_key,
            payload,
        )
        .expect("encrypt signal");

        let serialized = serde_json::to_string(&envelope).expect("serialize");
        assert!(
            !serialized.contains("192.168.1.20") && !serialized.contains("offer"),
            "coordinator-visible envelope must not leak SDP or candidate IPs"
        );
        assert_eq!(
            decrypt_signal(&owner, &passenger.peer_id, &envelope).expect("decrypt"),
            payload
        );

        // Purpose domain separation: a signal envelope can never be replayed
        // into the access-grant decryptor, and vice versa.
        assert!(decrypt_access::<String>(&owner, &passenger.peer_id, &envelope).is_err());
        let access_envelope = encrypt_access(
            &passenger,
            &owner.peer_id,
            &owner.encryption_public_key,
            &payload.to_string(),
        )
        .expect("encrypt access");
        assert!(decrypt_signal(&owner, &passenger.peer_id, &access_envelope).is_err());
    }
}
