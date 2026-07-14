use base64::{engine::general_purpose, Engine as _};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{
    EcdsaKeyPair, KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_ASN1,
    ECDSA_P256_SHA256_ASN1_SIGNING,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use tauri::{AppHandle, Manager};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub peer_id: String,
    pub public_key: String,
    pub private_key_pkcs8: String,
    pub encryption_public_key: String,
    pub encryption_private_key: String,
}

impl DeviceIdentity {
    pub fn public(&self) -> PublicIdentity {
        PublicIdentity {
            peer_id: self.peer_id.clone(),
            public_key: self.public_key.clone(),
            encryption_public_key: self.encryption_public_key.clone(),
        }
    }

    pub fn sign(&self, payload: &[u8]) -> Result<String, String> {
        let private_key = general_purpose::STANDARD
            .decode(self.private_key_pkcs8.trim())
            .map_err(|error| format!("设备签名私钥损坏: {error}"))?;
        let rng = SystemRandom::new();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &private_key, &rng)
            .map_err(|_| "无法加载设备签名私钥".to_string())?;
        let signature = pair
            .sign(&rng, payload)
            .map_err(|_| "设备签名失败".to_string())?;
        Ok(general_purpose::STANDARD.encode(signature.as_ref()))
    }

    pub fn encryption_private_bytes(&self) -> Result<[u8; 32], String> {
        let bytes = general_purpose::STANDARD
            .decode(self.encryption_private_key.trim())
            .map_err(|error| format!("设备加密私钥损坏: {error}"))?;
        bytes
            .try_into()
            .map_err(|_| "设备加密私钥长度无效".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicIdentity {
    pub peer_id: String,
    pub public_key: String,
    pub encryption_public_key: String,
}

pub fn peer_id_from_public_key(public_key: &[u8]) -> String {
    let hash = digest(&SHA256, public_key);
    format!(
        "p2p-{}",
        general_purpose::URL_SAFE_NO_PAD.encode(&hash.as_ref()[..16])
    )
}

pub fn verify(public_key: &str, payload: &[u8], signature: &str) -> Result<bool, String> {
    let public_key = general_purpose::STANDARD
        .decode(public_key.trim())
        .map_err(|error| format!("签名公钥无效: {error}"))?;
    let signature = general_purpose::STANDARD
        .decode(signature.trim())
        .map_err(|error| format!("签名格式无效: {error}"))?;
    Ok(UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
        .verify(payload, &signature)
        .is_ok())
}

fn random_32() -> Result<[u8; 32], String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法生成设备随机密钥".to_string())?;
    Ok(bytes)
}

fn generate() -> Result<DeviceIdentity, String> {
    let rng = SystemRandom::new();
    let signing_pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|_| "无法创建设备签名身份".to_string())?;
    let signing_pair = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        signing_pkcs8.as_ref(),
        &rng,
    )
    .map_err(|_| "无法读取新建设备身份".to_string())?;
    let public_bytes = signing_pair.public_key().as_ref();
    let encryption_secret = StaticSecret::from(random_32()?);
    let encryption_public = X25519PublicKey::from(&encryption_secret);
    Ok(DeviceIdentity {
        peer_id: peer_id_from_public_key(public_bytes),
        public_key: general_purpose::STANDARD.encode(public_bytes),
        private_key_pkcs8: general_purpose::STANDARD.encode(signing_pkcs8.as_ref()),
        encryption_public_key: general_purpose::STANDARD.encode(encryption_public.as_bytes()),
        encryption_private_key: general_purpose::STANDARD.encode(encryption_secret.to_bytes()),
    })
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("无法保存设备身份: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("无法写入设备身份: {error}"))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes).map_err(|error| format!("无法保存设备身份: {error}"))
    }
}

pub fn load_or_create_at(path: &Path) -> Result<DeviceIdentity, String> {
    if path.exists() {
        let bytes = fs::read(path).map_err(|error| format!("无法读取设备身份: {error}"))?;
        let identity: DeviceIdentity =
            serde_json::from_slice(&bytes).map_err(|error| format!("设备身份文件损坏: {error}"))?;
        let expected = general_purpose::STANDARD
            .decode(identity.public_key.trim())
            .map_err(|error| format!("设备公钥损坏: {error}"))?;
        if peer_id_from_public_key(&expected) != identity.peer_id {
            return Err("设备 peer_id 与公钥不匹配".to_string());
        }
        return Ok(identity);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建设备数据目录: {error}"))?;
    }
    let identity = generate()?;
    let bytes = serde_json::to_vec_pretty(&identity)
        .map_err(|error| format!("无法编码设备身份: {error}"))?;
    write_private_file(path, &bytes)?;
    Ok(identity)
}

pub fn load_or_create(app: &AppHandle) -> Result<DeviceIdentity, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录: {error}"))?;
    load_or_create_at(&directory.join("device-identity.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_persistent_and_signatures_verify() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("identity.json");
        let first = load_or_create_at(&path).expect("identity");
        let second = load_or_create_at(&path).expect("identity reload");
        assert_eq!(first.peer_id, second.peer_id);
        let signature = first.sign(b"trusted-carpool").expect("signature");
        assert!(verify(&first.public_key, b"trusted-carpool", &signature).expect("verify"));
        assert!(!verify(&first.public_key, b"tampered", &signature).expect("verify tampered"));
    }
}
