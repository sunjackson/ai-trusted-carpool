use crate::identity::{verify, DeviceIdentity, PublicIdentity};
use crate::models::ToolKind;
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::env;

const DEFAULT_COORDINATOR_URL: &str = "https://p2p.cnaigc.ai";
const MESSAGE_TTL_MS: i64 = 120_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicInvitePayload {
    pub version: u8,
    pub code: String,
    pub car_id: String,
    pub car_name: String,
    pub owner_label: String,
    pub owner_peer_id: String,
    pub owner_encryption_public_key: String,
    pub seat_no: u8,
    pub enabled_tools: Vec<ToolKind>,
    pub starts_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InviteRegistration {
    pub code: String,
    pub owner_peer_id: String,
    pub owner_public_key: String,
    pub owner_encryption_public_key: String,
    pub car_id: String,
    pub seat_no: u8,
    pub payload_base64: String,
    pub expires_at_ms: i64,
    pub timestamp_ms: i64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
struct SignableInvite<'a> {
    code: &'a str,
    owner_peer_id: &'a str,
    owner_public_key: &'a str,
    owner_encryption_public_key: &'a str,
    car_id: &'a str,
    seat_no: u8,
    payload_base64: &'a str,
    expires_at_ms: i64,
    timestamp_ms: i64,
}

#[derive(Debug, Deserialize)]
struct ResolveInviteResponse {
    invite: InviteRegistration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorMessage {
    pub id: String,
    pub from_peer_id: String,
    pub to_peer_id: String,
    pub public_key: String,
    pub kind: String,
    pub payload_json: String,
    pub ttl_ms: i64,
    pub signature: String,
    pub timestamp_ms: i64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendMessageInput {
    from_peer_id: String,
    to_peer_id: String,
    public_key: String,
    kind: String,
    payload_json: String,
    ttl_ms: i64,
    timestamp_ms: i64,
    signature: String,
}

#[derive(Debug, Serialize)]
struct SignableMessage<'a> {
    from_peer_id: &'a str,
    to_peer_id: &'a str,
    public_key: &'a str,
    kind: &'a str,
    payload_json: &'a str,
    ttl_ms: i64,
    timestamp_ms: i64,
}

#[derive(Debug, Serialize)]
struct PollInput {
    peer_id: String,
    public_key: String,
    after_ms: Option<i64>,
    limit: Option<i32>,
    timestamp_ms: i64,
    signature: String,
}

#[derive(Debug, Serialize)]
struct SignablePoll<'a> {
    peer_id: &'a str,
    public_key: &'a str,
    after_ms: Option<i64>,
    limit: Option<i32>,
    timestamp_ms: i64,
}

#[derive(Debug, Deserialize)]
struct PollResponse {
    messages: Vec<CoordinatorMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnCredentialsResponse {
    urls: Vec<String>,
    username: String,
    credential: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: Option<String>,
}

#[derive(Clone)]
pub struct CoordinatorClient {
    base_url: String,
    client: reqwest::Client,
}

impl CoordinatorClient {
    pub fn from_environment() -> Result<Self, String> {
        let base_url = env::var("TRUSTED_CARPOOL_COORDINATOR_URL")
            .unwrap_or_else(|_| DEFAULT_COORDINATOR_URL.to_string());
        Self::new(&base_url)
    }

    pub fn new(base_url: &str) -> Result<Self, String> {
        let base_url = base_url.trim().trim_end_matches('/');
        if !base_url.starts_with("https://")
            && !cfg!(test)
            && env::var("TRUSTED_CARPOOL_ALLOW_HTTP").as_deref() != Ok("1")
        {
            return Err("协调服务必须使用 HTTPS".to_string());
        }
        Ok(Self {
            base_url: base_url.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .map_err(|error| format!("无法创建协调服务客户端: {error}"))?,
        })
    }

    async fn response_error(response: reqwest::Response) -> String {
        let status = response.status();
        let detail = response
            .json::<ErrorResponse>()
            .await
            .ok()
            .and_then(|body| body.error)
            .unwrap_or_else(|| status.to_string());
        format!("协调服务请求失败 ({status}): {detail}")
    }

    pub fn build_invite(
        &self,
        identity: &DeviceIdentity,
        payload: &PublicInvitePayload,
        timestamp_ms: i64,
    ) -> Result<InviteRegistration, String> {
        let public = identity.public();
        let payload_base64 = general_purpose::STANDARD.encode(
            serde_json::to_vec(payload).map_err(|error| format!("无法编码上车码信息: {error}"))?,
        );
        let signable = SignableInvite {
            code: &payload.code,
            owner_peer_id: &public.peer_id,
            owner_public_key: &public.public_key,
            owner_encryption_public_key: &public.encryption_public_key,
            car_id: &payload.car_id,
            seat_no: payload.seat_no,
            payload_base64: &payload_base64,
            expires_at_ms: payload.expires_at_ms,
            timestamp_ms,
        };
        let bytes = serde_json::to_vec(&signable)
            .map_err(|error| format!("无法编码上车码签名内容: {error}"))?;
        Ok(InviteRegistration {
            code: payload.code.clone(),
            owner_peer_id: public.peer_id,
            owner_public_key: public.public_key,
            owner_encryption_public_key: public.encryption_public_key,
            car_id: payload.car_id.clone(),
            seat_no: payload.seat_no,
            payload_base64,
            expires_at_ms: payload.expires_at_ms,
            timestamp_ms,
            signature: identity.sign(&bytes)?,
        })
    }

    pub async fn register_invite(&self, invite: &InviteRegistration) -> Result<(), String> {
        let response = self
            .client
            .post(format!("{}/api/v1/carpool/invites", self.base_url))
            .json(invite)
            .send()
            .await
            .map_err(|error| format!("无法连接协调服务: {error}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::response_error(response).await)
        }
    }

    pub async fn resolve_invite(
        &self,
        code: &str,
    ) -> Result<(PublicInvitePayload, PublicIdentity), String> {
        let response = self
            .client
            .get(format!("{}/api/v1/carpool/invites/{}", self.base_url, code))
            .send()
            .await
            .map_err(|error| format!("无法连接协调服务: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }
        let record = response
            .json::<ResolveInviteResponse>()
            .await
            .map_err(|error| format!("协调服务返回了无效上车码: {error}"))?
            .invite;
        let signable = SignableInvite {
            code: &record.code,
            owner_peer_id: &record.owner_peer_id,
            owner_public_key: &record.owner_public_key,
            owner_encryption_public_key: &record.owner_encryption_public_key,
            car_id: &record.car_id,
            seat_no: record.seat_no,
            payload_base64: &record.payload_base64,
            expires_at_ms: record.expires_at_ms,
            timestamp_ms: record.timestamp_ms,
        };
        let signable_bytes = serde_json::to_vec(&signable)
            .map_err(|error| format!("无法验证上车码签名内容: {error}"))?;
        if !verify(&record.owner_public_key, &signable_bytes, &record.signature)? {
            return Err("上车码签名无效，内容可能被篡改".to_string());
        }
        let owner_public_bytes = general_purpose::STANDARD
            .decode(record.owner_public_key.trim())
            .map_err(|error| format!("车主公钥无效: {error}"))?;
        if crate::identity::peer_id_from_public_key(&owner_public_bytes) != record.owner_peer_id {
            return Err("上车码的车主身份与公钥不匹配".to_string());
        }
        let payload_bytes = general_purpose::STANDARD
            .decode(record.payload_base64.trim())
            .map_err(|error| format!("上车码载荷无效: {error}"))?;
        let payload: PublicInvitePayload = serde_json::from_slice(&payload_bytes)
            .map_err(|error| format!("上车码信息无效: {error}"))?;
        if payload.code != record.code
            || payload.car_id != record.car_id
            || payload.seat_no != record.seat_no
            || payload.owner_peer_id != record.owner_peer_id
            || payload.owner_encryption_public_key != record.owner_encryption_public_key
        {
            return Err("上车码公开信息与签名信封不一致".to_string());
        }
        Ok((
            payload,
            PublicIdentity {
                peer_id: record.owner_peer_id,
                public_key: record.owner_public_key,
                encryption_public_key: record.owner_encryption_public_key,
            },
        ))
    }

    pub async fn send_message(
        &self,
        identity: &DeviceIdentity,
        to_peer_id: &str,
        kind: &str,
        payload_json: String,
        timestamp_ms: i64,
    ) -> Result<(), String> {
        let public = identity.public();
        let signable = SignableMessage {
            from_peer_id: &public.peer_id,
            to_peer_id,
            public_key: &public.public_key,
            kind,
            payload_json: &payload_json,
            ttl_ms: MESSAGE_TTL_MS,
            timestamp_ms,
        };
        let signature = identity.sign(
            &serde_json::to_vec(&signable).map_err(|error| format!("无法编码协调消息: {error}"))?,
        )?;
        let input = SendMessageInput {
            from_peer_id: public.peer_id,
            to_peer_id: to_peer_id.to_string(),
            public_key: public.public_key,
            kind: kind.to_string(),
            payload_json,
            ttl_ms: MESSAGE_TTL_MS,
            timestamp_ms,
            signature,
        };
        let response = self
            .client
            .post(format!("{}/api/v1/carpool/messages", self.base_url))
            .json(&input)
            .send()
            .await
            .map_err(|error| format!("无法发送协调消息: {error}"))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Self::response_error(response).await)
        }
    }

    pub fn verify_message(
        message: &CoordinatorMessage,
        expected_sender: Option<&PublicIdentity>,
        expected_recipient_peer_id: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        if message.to_peer_id != expected_recipient_peer_id {
            return Err("协调消息收件人与当前设备不匹配".to_string());
        }
        if let Some(expected) = expected_sender {
            if message.from_peer_id != expected.peer_id || message.public_key != expected.public_key
            {
                return Err("协调消息发送者不是上车码中的车主".to_string());
            }
        }
        if message.ttl_ms < 1_000 || message.ttl_ms > MESSAGE_TTL_MS {
            return Err("协调消息有效期无效".to_string());
        }
        if message.timestamp_ms > now_ms.saturating_add(300_000) {
            return Err("协调消息时间戳来自未来".to_string());
        }
        if now_ms > message.timestamp_ms.saturating_add(message.ttl_ms) {
            return Err("协调消息已经过期".to_string());
        }
        let signable = SignableMessage {
            from_peer_id: &message.from_peer_id,
            to_peer_id: &message.to_peer_id,
            public_key: &message.public_key,
            kind: &message.kind,
            payload_json: &message.payload_json,
            ttl_ms: message.ttl_ms,
            timestamp_ms: message.timestamp_ms,
        };
        let signable_bytes = serde_json::to_vec(&signable)
            .map_err(|error| format!("无法编码协调消息签名内容: {error}"))?;
        if !verify(&message.public_key, &signable_bytes, &message.signature)? {
            return Err("协调消息签名无效，内容可能被篡改".to_string());
        }
        let public_key = general_purpose::STANDARD
            .decode(message.public_key.trim())
            .map_err(|error| format!("协调消息公钥无效: {error}"))?;
        if crate::identity::peer_id_from_public_key(&public_key) != message.from_peer_id {
            return Err("协调消息发送者身份与公钥不匹配".to_string());
        }
        Ok(())
    }

    pub async fn poll_messages(
        &self,
        identity: &DeviceIdentity,
        after_ms: Option<i64>,
        timestamp_ms: i64,
    ) -> Result<Vec<CoordinatorMessage>, String> {
        let public = identity.public();
        let signable = SignablePoll {
            peer_id: &public.peer_id,
            public_key: &public.public_key,
            after_ms,
            limit: Some(64),
            timestamp_ms,
        };
        let signature = identity.sign(
            &serde_json::to_vec(&signable)
                .map_err(|error| format!("无法编码消息轮询签名: {error}"))?,
        )?;
        let input = PollInput {
            peer_id: public.peer_id,
            public_key: public.public_key,
            after_ms,
            limit: Some(64),
            timestamp_ms,
            signature,
        };
        let response = self
            .client
            .post(format!("{}/api/v1/carpool/messages/poll", self.base_url))
            .json(&input)
            .send()
            .await
            .map_err(|error| format!("无法轮询协调消息: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }
        response
            .json::<PollResponse>()
            .await
            .map(|body| body.messages)
            .map_err(|error| format!("协调消息格式无效: {error}"))
    }

    pub async fn ice_servers(&self, peer_id: &str) -> Result<Vec<IceServer>, String> {
        let response = self
            .client
            .get(format!("{}/api/v1/turn-credentials", self.base_url))
            .query(&[("peer_id", peer_id)])
            .send()
            .await
            .map_err(|error| format!("无法获取 TURN 中继凭据: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }
        let credentials = response
            .json::<TurnCredentialsResponse>()
            .await
            .map_err(|error| format!("TURN 中继凭据格式无效: {error}"))?;
        if credentials.urls.is_empty()
            || credentials.urls.iter().any(|url| {
                !url.starts_with("turn:p2p.cnaigc.ai:3478?transport=")
                    && !url.starts_with("turns:p2p.cnaigc.ai:")
            })
        {
            return Err("TURN 中继地址不是受信任的 p2p.cnaigc.ai".to_string());
        }
        Ok(vec![IceServer {
            urls: credentials.urls,
            username: Some(credentials.username),
            credential: Some(credentials.credential),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64
    }

    #[test]
    fn signed_invite_round_trip_has_stable_field_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let identity = crate::identity::load_or_create_at(&directory.path().join("identity.json"))
            .expect("identity");
        let client = CoordinatorClient::new("http://127.0.0.1:1").expect("client");
        let payload = PublicInvitePayload {
            version: 1,
            code: "7G2K5LQ8M4TZ".to_string(),
            car_id: "e776088e-c2ea-4c91-8795-bad502eb2ad1".to_string(),
            car_name: "测试车队".to_string(),
            owner_label: "本机车主".to_string(),
            owner_peer_id: identity.peer_id.clone(),
            owner_encryption_public_key: identity.encryption_public_key.clone(),
            seat_no: 1,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            starts_at_ms: 1_700_000_000_000,
            expires_at_ms: 1_800_000_000_000,
        };
        let registration = client
            .build_invite(&identity, &payload, 1_700_000_000_000)
            .expect("registration");
        let signable = SignableInvite {
            code: &registration.code,
            owner_peer_id: &registration.owner_peer_id,
            owner_public_key: &registration.owner_public_key,
            owner_encryption_public_key: &registration.owner_encryption_public_key,
            car_id: &registration.car_id,
            seat_no: registration.seat_no,
            payload_base64: &registration.payload_base64,
            expires_at_ms: registration.expires_at_ms,
            timestamp_ms: registration.timestamp_ms,
        };
        assert!(verify(
            &registration.owner_public_key,
            &serde_json::to_vec(&signable).expect("json"),
            &registration.signature,
        )
        .expect("verify"));
    }

    #[test]
    fn delivered_messages_are_verified_again_by_the_recipient() {
        let directory = tempfile::tempdir().expect("tempdir");
        let sender = crate::identity::load_or_create_at(&directory.path().join("sender.json"))
            .expect("sender");
        let recipient =
            crate::identity::load_or_create_at(&directory.path().join("recipient.json"))
                .expect("recipient");
        let timestamp_ms = 1_700_000_000_000;
        let payload_json = r#"{"claim_id":"claim-1"}"#.to_string();
        let signable = SignableMessage {
            from_peer_id: &sender.peer_id,
            to_peer_id: &recipient.peer_id,
            public_key: &sender.public_key,
            kind: "carpool_claim",
            payload_json: &payload_json,
            ttl_ms: MESSAGE_TTL_MS,
            timestamp_ms,
        };
        let signature = sender
            .sign(&serde_json::to_vec(&signable).expect("signable"))
            .expect("signature");
        let message = CoordinatorMessage {
            id: "message-1".to_string(),
            from_peer_id: sender.peer_id.clone(),
            to_peer_id: recipient.peer_id.clone(),
            public_key: sender.public_key.clone(),
            kind: "carpool_claim".to_string(),
            payload_json,
            ttl_ms: MESSAGE_TTL_MS,
            signature,
            timestamp_ms,
            created_at_ms: timestamp_ms,
            expires_at_ms: timestamp_ms + MESSAGE_TTL_MS,
        };
        CoordinatorClient::verify_message(
            &message,
            Some(&sender.public()),
            &recipient.peer_id,
            timestamp_ms + 1,
        )
        .expect("verified");

        let mut tampered = message;
        tampered.payload_json = r#"{"claim_id":"attacker"}"#.to_string();
        assert!(CoordinatorClient::verify_message(
            &tampered,
            Some(&sender.public()),
            &recipient.peer_id,
            timestamp_ms + 1,
        )
        .is_err());
    }

    struct NodeCoordinator(std::process::Child);

    impl Drop for NodeCoordinator {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    async fn start_node_coordinator() -> (NodeCoordinator, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("project root")
            .join("deploy/coordinator/server.js");
        let child = std::process::Command::new("node")
            .arg(script)
            .env("HOST", "127.0.0.1")
            .env("PORT", port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("start node coordinator");
        let coordinator = NodeCoordinator(child);
        let base_url = format!("http://127.0.0.1:{port}");
        for _ in 0..50 {
            if reqwest::get(format!("{base_url}/api/v1/health"))
                .await
                .map(|response| response.status().is_success())
                .unwrap_or(false)
            {
                return (coordinator, base_url);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("node coordinator did not start")
    }

    #[tokio::test]
    #[ignore = "requires Node.js; run explicitly for cross-runtime protocol verification"]
    async fn coordinator_node_round_trip_keeps_access_device_bound() {
        let (_server, base_url) = start_node_coordinator().await;
        let client = CoordinatorClient::new(&base_url).expect("client");
        let directory = tempfile::tempdir().expect("tempdir");
        let owner = crate::identity::load_or_create_at(&directory.path().join("owner.json"))
            .expect("owner");
        let passenger =
            crate::identity::load_or_create_at(&directory.path().join("passenger.json"))
                .expect("passenger");
        let code = "7G2K5LQ8M4TZ".to_string();
        let car_id = uuid::Uuid::new_v4().to_string();
        let expires_at_ms = now_ms() + 60_000;
        let invite_payload = PublicInvitePayload {
            version: crate::protocol::PROTOCOL_VERSION,
            code: code.clone(),
            car_id: car_id.clone(),
            car_name: "跨进程测试车队".to_string(),
            owner_label: "可信车主".to_string(),
            owner_peer_id: owner.peer_id.clone(),
            owner_encryption_public_key: owner.encryption_public_key.clone(),
            seat_no: 1,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            starts_at_ms: now_ms(),
            expires_at_ms,
        };
        client
            .register_invite(
                &client
                    .build_invite(&owner, &invite_payload, now_ms())
                    .expect("signed invite"),
            )
            .await
            .expect("register invite");
        let (resolved, resolved_owner) = client.resolve_invite(&code).await.expect("resolve");
        assert_eq!(resolved.car_id, car_id);

        let requested_at_ms = now_ms();
        let claim = crate::protocol::CarpoolClaim {
            version: crate::protocol::PROTOCOL_VERSION,
            claim_id: uuid::Uuid::new_v4().to_string(),
            code: code.clone(),
            car_id: car_id.clone(),
            seat_no: 1,
            owner_peer_id: owner.peer_id.clone(),
            passenger_peer_id: passenger.peer_id.clone(),
            passenger_encryption_public_key: passenger.encryption_public_key.clone(),
            nickname: "跨进程乘客".to_string(),
            requested_at_ms,
            expires_at_ms: requested_at_ms + crate::protocol::CLAIM_TTL_MS,
        };
        client
            .send_message(
                &passenger,
                &owner.peer_id,
                "carpool_claim",
                serde_json::to_string(&claim).expect("claim json"),
                now_ms(),
            )
            .await
            .expect("send claim");
        let claims = client
            .poll_messages(&owner, None, now_ms())
            .await
            .expect("poll claim");
        assert_eq!(claims.len(), 1);
        CoordinatorClient::verify_message(&claims[0], None, &owner.peer_id, now_ms())
            .expect("verify claim again");

        let grant = crate::protocol::AccessGrant {
            version: crate::protocol::PROTOCOL_VERSION,
            claim_id: claim.claim_id.clone(),
            code,
            car_id,
            seat_no: 1,
            owner_peer_id: owner.peer_id.clone(),
            passenger_peer_id: passenger.peer_id.clone(),
            access_id: uuid::Uuid::new_v4().to_string(),
            session_secret: crate::protocol::new_session_secret().expect("session secret"),
            local_proxy_port: 25342,
            enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
            issued_at_ms: now_ms(),
            expires_at_ms,
        };
        let envelope = crate::crypto::encrypt_access(
            &owner,
            &passenger.peer_id,
            &passenger.encryption_public_key,
            &grant,
        )
        .expect("encrypt grant");
        client
            .send_message(
                &owner,
                &passenger.peer_id,
                "carpool_access",
                serde_json::to_string(&envelope).expect("envelope json"),
                now_ms(),
            )
            .await
            .expect("send access");
        let messages = client
            .poll_messages(&passenger, None, now_ms())
            .await
            .expect("poll access");
        assert_eq!(messages.len(), 1);
        CoordinatorClient::verify_message(
            &messages[0],
            Some(&resolved_owner),
            &passenger.peer_id,
            now_ms(),
        )
        .expect("verify owner access");
        let received_envelope: crate::crypto::EncryptedEnvelope =
            serde_json::from_str(&messages[0].payload_json).expect("envelope");
        let received: crate::protocol::AccessGrant =
            crate::crypto::decrypt_access(&passenger, &owner.peer_id, &received_envelope)
                .expect("decrypt grant");
        received
            .validate_for_claim(&claim, now_ms())
            .expect("claim-bound grant");
        assert_eq!(received.session_secret, grant.session_secret);
    }
}
