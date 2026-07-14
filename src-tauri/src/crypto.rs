use base64::{engine::general_purpose, Engine as _};
use ring::{aead, hkdf};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::identity::DeviceIdentity;

const ENVELOPE_VERSION: u8 = 1;
const ACCESS_PURPOSE: &str = "carpool_access";

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

fn aad(sender_peer_id: &str, recipient_peer_id: &str) -> Vec<u8> {
    format!(
        "trusted-carpool/v{ENVELOPE_VERSION}|{ACCESS_PURPOSE}|{sender_peer_id}|{recipient_peer_id}"
    )
    .into_bytes()
}

fn derive_key(shared_secret: &[u8; 32], salt: &[u8]) -> Result<[u8; 32], String> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(shared_secret);
    let info = [b"trusted-carpool/access-grant/aes-256-gcm".as_slice()];
    let okm = prk
        .expand(&info, Aes256KeyLength)
        .map_err(|_| "无法派生授权加密密钥".to_string())?;
    let mut key = [0_u8; 32];
    okm.fill(&mut key)
        .map_err(|_| "无法读取授权加密密钥".to_string())?;
    Ok(key)
}

pub fn encrypt_access<T: Serialize>(
    sender: &DeviceIdentity,
    recipient_peer_id: &str,
    recipient_encryption_public_key: &str,
    value: &T,
) -> Result<EncryptedEnvelope, String> {
    let recipient_public =
        X25519PublicKey::from(decode_32("乘客加密公钥", recipient_encryption_public_key)?);
    let ephemeral_secret = StaticSecret::from(random_bytes::<32>()?);
    let ephemeral_public = X25519PublicKey::from(&ephemeral_secret);
    let shared = ephemeral_secret.diffie_hellman(&recipient_public);
    if !shared.was_contributory() {
        return Err("乘客加密公钥无效".to_string());
    }

    let salt = random_bytes::<16>()?;
    let nonce_bytes = random_bytes::<12>()?;
    let key = derive_key(shared.as_bytes(), &salt)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| "无法创建授权加密器".to_string())?;
    let key = aead::LessSafeKey::new(unbound);
    let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
    let mut ciphertext =
        serde_json::to_vec(value).map_err(|error| format!("无法编码授权内容: {error}"))?;
    key.seal_in_place_append_tag(
        nonce,
        aead::Aad::from(aad(&sender.peer_id, recipient_peer_id)),
        &mut ciphertext,
    )
    .map_err(|_| "无法加密授权内容".to_string())?;

    Ok(EncryptedEnvelope {
        version: ENVELOPE_VERSION,
        purpose: ACCESS_PURPOSE.to_string(),
        sender_peer_id: sender.peer_id.clone(),
        recipient_peer_id: recipient_peer_id.to_string(),
        ephemeral_public_key: general_purpose::STANDARD.encode(ephemeral_public.as_bytes()),
        salt: general_purpose::STANDARD.encode(salt),
        nonce: general_purpose::STANDARD.encode(nonce_bytes),
        ciphertext: general_purpose::STANDARD.encode(ciphertext),
    })
}

pub fn decrypt_access<T: DeserializeOwned>(
    recipient: &DeviceIdentity,
    expected_sender_peer_id: &str,
    envelope: &EncryptedEnvelope,
) -> Result<T, String> {
    if envelope.version != ENVELOPE_VERSION || envelope.purpose != ACCESS_PURPOSE {
        return Err("授权信封版本或用途无效".to_string());
    }
    if envelope.sender_peer_id != expected_sender_peer_id {
        return Err("授权信封车主身份不匹配".to_string());
    }
    if envelope.recipient_peer_id != recipient.peer_id {
        return Err("授权信封不是发给当前设备的".to_string());
    }

    let ephemeral_public =
        X25519PublicKey::from(decode_32("临时加密公钥", &envelope.ephemeral_public_key)?);
    let recipient_secret = StaticSecret::from(recipient.encryption_private_bytes()?);
    let shared = recipient_secret.diffie_hellman(&ephemeral_public);
    if !shared.was_contributory() {
        return Err("授权信封的临时公钥无效".to_string());
    }
    let salt = general_purpose::STANDARD
        .decode(envelope.salt.trim())
        .map_err(|error| format!("授权盐值无效: {error}"))?;
    let nonce_bytes: [u8; 12] = general_purpose::STANDARD
        .decode(envelope.nonce.trim())
        .map_err(|error| format!("授权随机数无效: {error}"))?
        .try_into()
        .map_err(|_| "授权随机数长度无效".to_string())?;
    let mut ciphertext = general_purpose::STANDARD
        .decode(envelope.ciphertext.trim())
        .map_err(|error| format!("授权密文无效: {error}"))?;
    let key = derive_key(shared.as_bytes(), &salt)?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| "无法创建授权解密器".to_string())?;
    let key = aead::LessSafeKey::new(unbound);
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce_bytes),
            aead::Aad::from(aad(expected_sender_peer_id, &recipient.peer_id)),
            &mut ciphertext,
        )
        .map_err(|_| "授权信封校验失败，内容可能被篡改".to_string())?;
    serde_json::from_slice(plaintext).map_err(|error| format!("授权内容无效: {error}"))
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
}
