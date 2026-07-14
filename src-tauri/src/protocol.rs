use base64::{engine::general_purpose, Engine as _};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::ToolKind;

pub const PROTOCOL_VERSION: u8 = 1;
pub const CLAIM_TTL_MS: i64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CarpoolClaim {
    pub version: u8,
    pub claim_id: String,
    pub code: String,
    pub car_id: String,
    pub seat_no: u8,
    pub owner_peer_id: String,
    pub passenger_peer_id: String,
    pub passenger_encryption_public_key: String,
    pub nickname: String,
    pub requested_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessGrant {
    pub version: u8,
    pub claim_id: String,
    pub code: String,
    pub car_id: String,
    pub seat_no: u8,
    pub owner_peer_id: String,
    pub passenger_peer_id: String,
    pub access_id: String,
    pub session_secret: String,
    pub local_proxy_port: u16,
    pub enabled_tools: Vec<ToolKind>,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaveNotice {
    pub version: u8,
    pub code: String,
    pub car_id: String,
    pub access_id: String,
    pub passenger_peer_id: String,
    pub timestamp_ms: i64,
}

pub fn new_session_secret() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "无法生成会话授权密钥".to_string())?;
    Ok(general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

impl CarpoolClaim {
    pub fn validate(&self, now_ms: i64) -> Result<(), String> {
        if self.version != PROTOCOL_VERSION {
            return Err("认领协议版本不受支持".to_string());
        }
        Uuid::parse_str(&self.claim_id).map_err(|_| "认领编号无效".to_string())?;
        if self.code.len() != 12
            || !self
                .code
                .bytes()
                .all(|byte| matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'Z' | b'2'..=b'9'))
        {
            return Err("认领上车码无效".to_string());
        }
        if self.seat_no == 0 || self.seat_no > 4 {
            return Err("认领座位号无效".to_string());
        }
        if self.nickname.trim().is_empty() || self.nickname.chars().count() > 20 {
            return Err("认领昵称应为 1 到 20 个字符".to_string());
        }
        if self.requested_at_ms > now_ms.saturating_add(300_000)
            || self.expires_at_ms <= now_ms
            || self.expires_at_ms > self.requested_at_ms.saturating_add(CLAIM_TTL_MS)
        {
            return Err("认领请求已经过期或有效期无效".to_string());
        }
        let encryption_key = general_purpose::STANDARD
            .decode(self.passenger_encryption_public_key.trim())
            .map_err(|error| format!("乘客加密公钥无效: {error}"))?;
        if encryption_key.len() != 32 {
            return Err("乘客加密公钥长度无效".to_string());
        }
        Ok(())
    }
}

impl AccessGrant {
    pub fn validate_for_claim(&self, claim: &CarpoolClaim, now_ms: i64) -> Result<(), String> {
        if self.version != PROTOCOL_VERSION
            || self.claim_id != claim.claim_id
            || self.code != claim.code
            || self.car_id != claim.car_id
            || self.seat_no != claim.seat_no
            || self.owner_peer_id != claim.owner_peer_id
            || self.passenger_peer_id != claim.passenger_peer_id
        {
            return Err("授权与当前设备的认领请求不匹配".to_string());
        }
        Uuid::parse_str(&self.access_id).map_err(|_| "授权编号无效".to_string())?;
        if self.session_secret.len() < 40 {
            return Err("授权会话密钥长度无效".to_string());
        }
        if self.issued_at_ms > now_ms.saturating_add(300_000) || self.expires_at_ms <= now_ms {
            return Err("授权已经过期".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_rejects_short_or_ambiguous_codes() {
        let claim = CarpoolClaim {
            version: PROTOCOL_VERSION,
            claim_id: Uuid::new_v4().to_string(),
            code: "ABC123".to_string(),
            car_id: Uuid::new_v4().to_string(),
            seat_no: 1,
            owner_peer_id: "p2p-owner".to_string(),
            passenger_peer_id: "p2p-passenger".to_string(),
            passenger_encryption_public_key: general_purpose::STANDARD.encode([7_u8; 32]),
            nickname: "测试乘客".to_string(),
            requested_at_ms: 1_000,
            expires_at_ms: 61_000,
        };
        assert!(claim.validate(2_000).is_err());
    }

    #[test]
    fn session_secrets_have_256_bits_of_random_material() {
        let first = new_session_secret().expect("secret");
        let second = new_session_secret().expect("secret");
        assert_ne!(first, second);
        assert_eq!(
            general_purpose::URL_SAFE_NO_PAD
                .decode(first)
                .expect("decode")
                .len(),
            32
        );
    }
}
