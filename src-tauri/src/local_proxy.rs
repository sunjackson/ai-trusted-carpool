use crate::models::ToolKind;
#[cfg(test)]
use crate::relay::RelayResponse;
use crate::relay::{
    allowed_path, allowed_relay_header, decode_body, sha256_label, sign_request, RelayBridge,
    RelayHeader, RelayRequest, RelayStreamEvent, RelayStreamKind, MAX_RELAY_RESPONSE_BYTES,
    RELAY_START_TIMEOUT_MS,
};
use crate::runtime::RuntimeState;
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_LENGTH};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use ring::hmac;
use std::convert::Infallible;
use std::io;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use uuid::Uuid;

pub const LOCAL_PROXY_PORT: u16 = 25_342;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

type ProxyBody = UnsyncBoxBody<Bytes, io::Error>;

fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn json_response(status: StatusCode, message: &str) -> Response<ProxyBody> {
    let body = serde_json::json!({
        "error": {
            "type": "trusted_carpool_proxy_error",
            "message": message
        }
    })
    .to_string();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(Bytes::from(body)))
        .expect("static response")
}

struct RelayHttpBody {
    request_id: String,
    receiver: mpsc::Receiver<RelayStreamEvent>,
    digest: Option<ring::digest::Context>,
    received: usize,
    finished: bool,
}

impl RelayHttpBody {
    fn new(request_id: String, receiver: mpsc::Receiver<RelayStreamEvent>) -> Self {
        Self {
            request_id,
            receiver,
            digest: Some(ring::digest::Context::new(&ring::digest::SHA256)),
            received: 0,
            finished: false,
        }
    }

    fn fail(
        &mut self,
        message: impl Into<String>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.finished = true;
        Poll::Ready(Some(Err(io::Error::other(message.into()))))
    }
}

impl Body for RelayHttpBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let body = self.get_mut();
        if body.finished {
            return Poll::Ready(None);
        }
        loop {
            let event = match Pin::new(&mut body.receiver).poll_recv(cx) {
                Poll::Ready(Some(event)) => event,
                Poll::Ready(None) => return body.fail("安全连接在流式响应完成前关闭"),
                Poll::Pending => return Poll::Pending,
            };
            if event.request_id != body.request_id {
                return body.fail("车主流式响应与当前请求不匹配");
            }
            match event.kind {
                RelayStreamKind::Chunk => {
                    let remaining = MAX_RELAY_RESPONSE_BYTES.saturating_sub(body.received);
                    let chunk = match decode_body(
                        event.chunk_base64.as_deref().unwrap_or_default(),
                        remaining,
                        "车主响应分块",
                    ) {
                        Ok(chunk) => chunk,
                        Err(error) => return body.fail(error),
                    };
                    body.received = body.received.saturating_add(chunk.len());
                    if let Some(context) = body.digest.as_mut() {
                        context.update(&chunk);
                    }
                    if chunk.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))));
                }
                RelayStreamKind::End => {
                    let Some(context) = body.digest.take() else {
                        return body.fail("流式响应完整性状态无效");
                    };
                    let actual = format!(
                        "sha256:{}",
                        general_purpose::URL_SAFE_NO_PAD.encode(context.finish().as_ref())
                    );
                    if event.body_sha256.as_deref() != Some(actual.as_str()) {
                        return body.fail("车主流式响应完整性校验失败");
                    }
                    body.finished = true;
                    return Poll::Ready(None);
                }
                RelayStreamKind::Error => {
                    return body.fail(format!(
                        "车主执行请求失败: {}",
                        event.error.as_deref().unwrap_or("未知错误")
                    ));
                }
                RelayStreamKind::Start => return body.fail("车主重复发送流式响应头"),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

fn parse_route(path: &str) -> Result<(String, ToolKind, String), String> {
    let mut parts = path.splitn(5, '/');
    if parts.next() != Some("") || parts.next() != Some("access") {
        return Err("本地中转地址无效".to_string());
    }
    let access_id = parts
        .next()
        .filter(|value| Uuid::parse_str(value).is_ok())
        .ok_or_else(|| "上车授权编号无效".to_string())?
        .to_string();
    let tool = match parts.next() {
        Some("claude") => ToolKind::Claude,
        Some("codex") => ToolKind::Codex,
        _ => return Err("本地中转工具地址无效".to_string()),
    };
    let remaining = parts.next().unwrap_or_default();
    let upstream_path = format!("/{remaining}");
    if !allowed_path(tool, &upstream_path) {
        return Err("请求地址不在官方 API 白名单内".to_string());
    }
    Ok((access_id, tool, upstream_path))
}

fn bearer_token(value: &str) -> Option<&str> {
    value
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| value.trim().strip_prefix("bearer "))
        .map(str::trim)
}

fn supplied_secret(headers: &HeaderMap, tool: ToolKind) -> Option<String> {
    match tool {
        ToolKind::Claude => headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .or_else(|| {
                headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(bearer_token)
                    .map(str::to_string)
            }),
        ToolKind::Codex => headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_token)
            .map(str::to_string),
    }
}

#[cfg(test)]
async fn process_buffered_request<F, Fut>(
    method: String,
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
    state: RuntimeState,
    dispatch: F,
) -> Response<ProxyBody>
where
    F: FnOnce(RelayRequest) -> Fut,
    Fut: std::future::Future<Output = Result<RelayResponse, String>>,
{
    let (access_id, tool, upstream_path) = match parse_route(&path_and_query) {
        Ok(route) => route,
        Err(error) => return json_response(StatusCode::NOT_FOUND, &error),
    };
    let supplied = supplied_secret(&headers, tool).unwrap_or_default();
    let session_secret = {
        let runtime = match state.inner.lock() {
            Ok(runtime) => runtime,
            Err(_) => return json_response(StatusCode::SERVICE_UNAVAILABLE, "运行状态暂时不可用"),
        };
        let Some(secret) = runtime.access_secrets.get(&access_id) else {
            return json_response(StatusCode::UNAUTHORIZED, "上车授权不存在或已经失效");
        };
        if !secret_matches(secret, &supplied) {
            return json_response(StatusCode::UNAUTHORIZED, "本地会话认证失败");
        }
        if !runtime.passenger_contexts.contains_key(&access_id) {
            return json_response(StatusCode::UNAUTHORIZED, "上车设备绑定信息已经失效");
        }
        secret.clone()
    };
    if !matches!(method.as_str(), "GET" | "POST") {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "可信拼车只允许 GET/POST 官方 API 请求",
        );
    }
    if body.len() > MAX_REQUEST_BYTES {
        return json_response(StatusCode::PAYLOAD_TOO_LARGE, "模型请求过大，已安全中止");
    }
    let relay_headers = headers
        .iter()
        .filter(|(name, _)| allowed_relay_header(name.as_str()))
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| RelayHeader {
                name: name.as_str().to_ascii_lowercase(),
                value: value.chars().take(4096).collect(),
            })
        })
        .collect::<Vec<_>>();
    let mut relay_request = RelayRequest {
        request_id: Uuid::new_v4().to_string(),
        access_id,
        tool,
        method,
        path: upstream_path,
        headers: relay_headers,
        body_base64: general_purpose::STANDARD.encode(&body),
        body_sha256: sha256_label(&body),
        timestamp_ms: now_ms(),
        auth_proof: String::new(),
    };
    if let Err(error) = sign_request(&mut relay_request, &session_secret) {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, &error);
    }
    let expected_request_id = relay_request.request_id.clone();
    let relay_response = match dispatch(relay_request).await {
        Ok(response) => response,
        Err(error) => return json_response(StatusCode::BAD_GATEWAY, &error),
    };
    if relay_response.request_id != expected_request_id {
        return json_response(StatusCode::BAD_GATEWAY, "车主响应与当前请求不匹配");
    }
    let response_body = match decode_body(&relay_response.body_base64, 16 * 1024 * 1024, "车主响应")
    {
        Ok(body) => body,
        Err(error) => return json_response(StatusCode::BAD_GATEWAY, &error),
    };
    if sha256_label(&response_body) != relay_response.body_sha256 {
        return json_response(StatusCode::BAD_GATEWAY, "车主响应完整性校验失败");
    }
    let status =
        StatusCode::from_u16(relay_response.status_code).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for header in relay_response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(header.name),
            HeaderValue::try_from(header.value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(full_body(Bytes::from(response_body)))
        .unwrap_or_else(|_| json_response(StatusCode::BAD_GATEWAY, "无法构造模型响应"))
}

async fn process_streaming_request<F, Fut>(
    method: String,
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
    state: RuntimeState,
    dispatch: F,
) -> Response<ProxyBody>
where
    F: FnOnce(RelayRequest) -> Fut,
    Fut: std::future::Future<Output = Result<mpsc::Receiver<RelayStreamEvent>, String>>,
{
    let (access_id, tool, upstream_path) = match parse_route(&path_and_query) {
        Ok(route) => route,
        Err(error) => return json_response(StatusCode::NOT_FOUND, &error),
    };
    let supplied = supplied_secret(&headers, tool).unwrap_or_default();
    let session_secret = {
        let runtime = match state.inner.lock() {
            Ok(runtime) => runtime,
            Err(_) => return json_response(StatusCode::SERVICE_UNAVAILABLE, "运行状态暂时不可用"),
        };
        let Some(secret) = runtime.access_secrets.get(&access_id) else {
            return json_response(StatusCode::UNAUTHORIZED, "上车授权不存在或已经失效");
        };
        if !secret_matches(secret, &supplied) {
            return json_response(StatusCode::UNAUTHORIZED, "本地会话认证失败");
        }
        if !runtime.passenger_contexts.contains_key(&access_id) {
            return json_response(StatusCode::UNAUTHORIZED, "上车设备绑定信息已经失效");
        }
        secret.clone()
    };
    if !matches!(method.as_str(), "GET" | "POST") {
        return json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "可信拼车只允许 GET/POST 官方 API 请求",
        );
    }
    if body.len() > MAX_REQUEST_BYTES {
        return json_response(StatusCode::PAYLOAD_TOO_LARGE, "模型请求过大，已安全中止");
    }
    let relay_headers = headers
        .iter()
        .filter(|(name, _)| allowed_relay_header(name.as_str()))
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| RelayHeader {
                name: name.as_str().to_ascii_lowercase(),
                value: value.chars().take(4096).collect(),
            })
        })
        .collect::<Vec<_>>();
    let mut relay_request = RelayRequest {
        request_id: Uuid::new_v4().to_string(),
        access_id,
        tool,
        method,
        path: upstream_path,
        headers: relay_headers,
        body_base64: general_purpose::STANDARD.encode(&body),
        body_sha256: sha256_label(&body),
        timestamp_ms: now_ms(),
        auth_proof: String::new(),
    };
    if let Err(error) = sign_request(&mut relay_request, &session_secret) {
        return json_response(StatusCode::INTERNAL_SERVER_ERROR, &error);
    }
    let expected_request_id = relay_request.request_id.clone();
    let mut receiver = match dispatch(relay_request).await {
        Ok(receiver) => receiver,
        Err(error) => return json_response(StatusCode::BAD_GATEWAY, &error),
    };
    let first = match tokio::time::timeout(
        std::time::Duration::from_millis(RELAY_START_TIMEOUT_MS),
        receiver.recv(),
    )
    .await
    {
        Ok(Some(event)) => event,
        Ok(None) => return json_response(StatusCode::BAD_GATEWAY, "安全连接在响应前关闭"),
        Err(_) => return json_response(StatusCode::GATEWAY_TIMEOUT, "等待车主响应超时"),
    };
    if first.request_id != expected_request_id {
        return json_response(StatusCode::BAD_GATEWAY, "车主响应与当前请求不匹配");
    }
    if first.kind == RelayStreamKind::Error {
        return json_response(
            StatusCode::BAD_GATEWAY,
            first.error.as_deref().unwrap_or("车主执行请求失败"),
        );
    }
    if first.kind != RelayStreamKind::Start {
        return json_response(StatusCode::BAD_GATEWAY, "车主没有先发送流式响应头");
    }
    let status = first
        .status_code
        .and_then(|value| StatusCode::from_u16(value).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for header in first.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(header.name),
            HeaderValue::try_from(header.value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(RelayHttpBody::new(expected_request_id, receiver).boxed_unsync())
        .unwrap_or_else(|_| json_response(StatusCode::BAD_GATEWAY, "无法构造流式模型响应"))
}

fn secret_matches(expected: &str, supplied: &str) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, b"trusted-carpool-local-secret-compare");
    let expected_tag = hmac::sign(&key, expected.as_bytes());
    hmac::verify(&key, supplied.as_bytes(), expected_tag.as_ref()).is_ok()
}

async fn handle(
    request: Request<Incoming>,
    state: RuntimeState,
) -> Result<Response<ProxyBody>, Infallible> {
    if request.uri().path() == "/health" {
        return Ok(Response::new(full_body(Bytes::from_static(b"OK"))));
    }
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or_else(|| request.uri().path())
        .to_string();
    if let Some(length) = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
    {
        if length > MAX_REQUEST_BYTES {
            return Ok(json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "模型请求过大，已安全中止",
            ));
        }
    }
    let method = request.method().as_str().to_ascii_uppercase();
    let headers = request.headers().clone();
    let body = match request.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &format!("无法读取模型请求: {error}"),
            ))
        }
    };
    let dispatch_state = state.clone();
    let response = process_streaming_request(
        method,
        path_and_query,
        headers,
        body,
        state,
        move |relay_request| async move {
            let access_id = relay_request.access_id.clone();
            let owner_peer_id = {
                // The passenger context is checked again inside the bridge-facing dispatch.
                let runtime = dispatch_state
                    .inner
                    .lock()
                    .map_err(|_| "运行状态暂时不可用".to_string())?;
                runtime
                    .passenger_contexts
                    .get(&access_id)
                    .map(|context| context.owner_peer_id.clone())
                    .ok_or_else(|| "上车设备绑定信息已经失效".to_string())?
            };
            RelayBridge::global()
                .relay_stream(access_id, owner_peer_id, &relay_request)
                .await
        },
    )
    .await;
    Ok(response)
}

pub fn start(state: RuntimeState) -> Result<(), String> {
    let listener = StdTcpListener::bind(("127.0.0.1", LOCAL_PROXY_PORT))
        .map_err(|error| format!("本地中转端口 {LOCAL_PROXY_PORT} 无法启动: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("无法配置本地中转端口: {error}"))?;
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                crate::diagnostics::record(
                    "error",
                    "local-proxy",
                    format!("failed to start local proxy runtime: {error}"),
                );
                return;
            }
        };
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(value) => value,
                Err(error) => {
                    crate::diagnostics::record(
                        "error",
                        "local-proxy",
                        format!("local proxy accept failed: {error}"),
                    );
                    continue;
                }
            };
            let state = state.clone();
            tauri::async_runtime::spawn(async move {
                let io = TokioIo::new(stream);
                if let Err(error) = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |request| handle(request, state.clone())),
                    )
                    .await
                {
                    crate::diagnostics::record(
                        "warn",
                        "local-proxy",
                        format!("local proxy connection failed: {error}"),
                    );
                }
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CarSession, Seat, SeatState, SeatUsageSummary};
    use crate::relay::{execute_host_request_stream_with, HostCredential, HostCredentialKind};
    use crate::runtime::{HostSeatBinding, PassengerAccessContext};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn local_routes_keep_claude_and_codex_unambiguous() {
        let access = Uuid::new_v4().to_string();
        assert_eq!(
            parse_route(&format!("/access/{access}/claude/v1/messages")),
            Ok((access.clone(), ToolKind::Claude, "/v1/messages".to_string()))
        );
        assert_eq!(
            parse_route(&format!("/access/{access}/codex/v1/responses")),
            Ok((access.clone(), ToolKind::Codex, "/v1/responses".to_string()))
        );
        assert!(parse_route(&format!("/access/{access}/codex/internal/admin")).is_err());
    }

    #[test]
    fn bearer_parser_does_not_accept_unscoped_values() {
        assert_eq!(
            bearer_token("Bearer session-secret"),
            Some("session-secret")
        );
        assert_eq!(bearer_token("session-secret"), None);
    }

    fn stream_event(request_id: &str, kind: RelayStreamKind) -> RelayStreamEvent {
        RelayStreamEvent {
            request_id: request_id.to_string(),
            kind,
            status_code: None,
            headers: Vec::new(),
            chunk_base64: None,
            body_sha256: None,
            latency_ms: None,
            error: None,
        }
    }

    #[tokio::test]
    async fn passenger_body_delivers_stream_chunks_and_verifies_the_final_digest() {
        let request_id = "stream-body";
        let (sender, receiver) = mpsc::channel(4);
        let mut body = RelayHttpBody::new(request_id.to_string(), receiver);
        let first = b"data: first\n\n";
        let second = b"data: second\n\n";
        let mut combined = first.to_vec();
        combined.extend_from_slice(second);

        let mut event = stream_event(request_id, RelayStreamKind::Chunk);
        event.chunk_base64 = Some(general_purpose::STANDARD.encode(first));
        sender.send(event).await.expect("first chunk");
        let frame = body
            .frame()
            .await
            .expect("first frame")
            .expect("valid first frame");
        assert_eq!(frame.data_ref().expect("frame data").as_ref(), first);

        let mut event = stream_event(request_id, RelayStreamKind::Chunk);
        event.chunk_base64 = Some(general_purpose::STANDARD.encode(second));
        sender.send(event).await.expect("second chunk");
        let frame = body
            .frame()
            .await
            .expect("second frame")
            .expect("valid second frame");
        assert_eq!(frame.data_ref().expect("frame data").as_ref(), second);

        let mut event = stream_event(request_id, RelayStreamKind::End);
        event.body_sha256 = Some(sha256_label(&combined));
        sender.send(event).await.expect("end event");
        assert!(body.frame().await.is_none());
    }

    #[tokio::test]
    async fn passenger_body_reports_a_tampered_stream_digest() {
        let request_id = "tampered-stream";
        let (sender, receiver) = mpsc::channel(4);
        let mut body = RelayHttpBody::new(request_id.to_string(), receiver);
        let mut chunk = stream_event(request_id, RelayStreamKind::Chunk);
        chunk.chunk_base64 = Some(general_purpose::STANDARD.encode(b"trusted"));
        sender.send(chunk).await.expect("chunk");
        body.frame()
            .await
            .expect("chunk frame")
            .expect("valid chunk");
        let mut end = stream_event(request_id, RelayStreamKind::End);
        end.body_sha256 = Some(sha256_label(b"tampered"));
        sender.send(end).await.expect("end");
        let error = body
            .frame()
            .await
            .expect("error frame")
            .expect_err("digest must fail");
        assert!(error.to_string().contains("完整性校验失败"));
    }

    #[tokio::test]
    async fn passenger_http_pipeline_runs_claude_and_codex_concurrently_and_accounts_by_model() {
        let access_id = Uuid::new_v4().to_string();
        let session_secret = crate::protocol::new_session_secret().expect("session secret");
        let code = "7G2K5LQ8M4TZ".to_string();
        let now = now_ms();
        let passenger_state = RuntimeState::default();
        {
            let mut passenger = passenger_state.inner.lock().expect("passenger runtime");
            passenger
                .access_secrets
                .insert(access_id.clone(), session_secret.clone());
            passenger.passenger_contexts.insert(
                access_id.clone(),
                PassengerAccessContext {
                    code: code.clone(),
                    car_id: "car-e2e".to_string(),
                    owner_peer_id: "owner-peer".to_string(),
                    owner_public_key: "owner-public-key".to_string(),
                    owner_encryption_public_key: "owner-encryption-key".to_string(),
                },
            );
        }
        let host_state = RuntimeState::default();
        let history_directory = tempfile::tempdir().expect("history directory");
        let history_path = history_directory.path().join("usage-history.jsonl");
        {
            let mut host = host_state.inner.lock().expect("host runtime");
            host.active_car = Some(CarSession {
                car_id: "car-e2e".to_string(),
                car_name: "熟人双工具车队".to_string(),
                owner_peer_id: "owner-peer".to_string(),
                started_at: now - 1_000,
                expires_at: now + 60_000,
                enabled_tools: vec![ToolKind::Claude, ToolKind::Codex],
                seats: vec![Seat {
                    seat_no: 1,
                    code: code.clone(),
                    nickname: Some("小雨".to_string()),
                    state: SeatState::Connected,
                    tool: None,
                    usage: SeatUsageSummary::default(),
                    token_limits: crate::models::MemberTokenLimits::default(),
                    token_limit_status: crate::models::MemberTokenLimitStatus::default(),
                    token_usage_events: Vec::new(),
                }],
                account_quotas: Vec::new(),
            });
            host.host_bindings.insert(
                code.clone(),
                HostSeatBinding {
                    code,
                    claim_id: "claim-e2e".to_string(),
                    passenger_peer_id: "passenger-peer".to_string(),
                    passenger_encryption_public_key: "passenger-key".to_string(),
                    access_id: access_id.clone(),
                    session_secret: session_secret.clone(),
                    issued_at_ms: now,
                },
            );
            host.usage_history_path = Some(history_path.clone());
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock official listener");
        let address = listener.local_addr().expect("mock official address");
        let server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept official request");
                handlers.push(tokio::spawn(async move {
                    let mut incoming = vec![0_u8; 32 * 1024];
                    let size = stream.read(&mut incoming).await.expect("read official request");
                    let request = String::from_utf8_lossy(&incoming[..size]);
                    let body = if request.contains("POST /v1/messages HTTP/1.1") {
                        assert!(request.contains("x-api-key: sk-owner-test"));
                        r#"{"type":"message","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":40,"ephemeral_1h_input_tokens":60}}}"#
                    } else {
                        assert!(request.contains("POST /v1/responses HTTP/1.1"));
                        assert!(request.contains("authorization: Bearer sk-owner-test"));
                        r#"{"id":"response-e2e","usage":{"input_tokens":1000,"output_tokens":200,"input_tokens_details":{"cached_tokens":700}}}"#
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write official response");
                }));
            }
            for handler in handlers {
                handler.await.expect("official handler");
            }
        });
        let endpoint = format!("http://{address}");
        let credential = HostCredential {
            secret: "sk-owner-test".to_string(),
            account_id: None,
            kind: HostCredentialKind::ApiKey,
            source: "test".to_string(),
        };

        let mut claude_headers = HeaderMap::new();
        claude_headers.insert(
            "x-api-key",
            HeaderValue::from_str(&session_secret).expect("Claude session header"),
        );
        claude_headers.insert("content-type", HeaderValue::from_static("application/json"));
        let mut codex_headers = HeaderMap::new();
        codex_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {session_secret}"))
                .expect("Codex session header"),
        );
        codex_headers.insert("content-type", HeaderValue::from_static("application/json"));

        let claude = process_streaming_request(
            "POST".to_string(),
            format!("/access/{access_id}/claude/v1/messages"),
            claude_headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-6","messages":[]}"#),
            passenger_state.clone(),
            {
                let host_state = host_state.clone();
                let credential = credential.clone();
                let endpoint = endpoint.clone();
                move |request| async move {
                    let request_id = request.request_id.clone();
                    let (sender, receiver) = mpsc::channel(64);
                    tokio::spawn(async move {
                        let event_sender = sender.clone();
                        let result = execute_host_request_stream_with(
                            &host_state,
                            request,
                            &credential,
                            &endpoint,
                            move |event| {
                                event_sender
                                    .try_send(event)
                                    .map_err(|_| "test stream channel closed".to_string())
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            let _ = sender
                                .send(RelayStreamEvent::error(request_id, error))
                                .await;
                        }
                    });
                    Ok(receiver)
                }
            },
        );
        let codex = process_streaming_request(
            "POST".to_string(),
            format!("/access/{access_id}/codex/v1/responses"),
            codex_headers,
            Bytes::from_static(br#"{"model":"gpt-5.6-luna","input":"hello"}"#),
            passenger_state,
            {
                let host_state = host_state.clone();
                let credential = credential.clone();
                let endpoint = endpoint.clone();
                move |request| async move {
                    let request_id = request.request_id.clone();
                    let (sender, receiver) = mpsc::channel(64);
                    tokio::spawn(async move {
                        let event_sender = sender.clone();
                        let result = execute_host_request_stream_with(
                            &host_state,
                            request,
                            &credential,
                            &endpoint,
                            move |event| {
                                event_sender
                                    .try_send(event)
                                    .map_err(|_| "test stream channel closed".to_string())
                            },
                        )
                        .await;
                        if let Err(error) = result {
                            let _ = sender
                                .send(RelayStreamEvent::error(request_id, error))
                                .await;
                        }
                    });
                    Ok(receiver)
                }
            },
        );
        let (claude_response, codex_response) = tokio::join!(claude, codex);
        assert_eq!(claude_response.status(), StatusCode::OK);
        assert_eq!(codex_response.status(), StatusCode::OK);
        let (claude_body, codex_body) = tokio::join!(
            claude_response.into_body().collect(),
            codex_response.into_body().collect()
        );
        assert!(!claude_body.expect("Claude stream").to_bytes().is_empty());
        assert!(!codex_body.expect("Codex stream").to_bytes().is_empty());
        server.await.expect("official server");

        let host = host_state.inner.lock().expect("host runtime");
        let usage = &host.active_car.as_ref().expect("active car").seats[0].usage;
        assert_eq!(usage.request_count, 2);
        assert_eq!(usage.models.len(), 2);
        let claude_usage = usage
            .models
            .iter()
            .find(|model| model.model == "claude-sonnet-4-6")
            .expect("Claude model usage");
        assert_eq!(claude_usage.input_tokens, 100);
        assert_eq!(claude_usage.output_tokens, 20);
        assert_eq!(claude_usage.cache_read_tokens, 300);
        assert_eq!(claude_usage.cache_write_5m_tokens, 40);
        assert_eq!(claude_usage.cache_write_1h_tokens, 60);
        let codex_usage = usage
            .models
            .iter()
            .find(|model| model.model == "gpt-5.6-luna")
            .expect("Codex model usage");
        assert_eq!(codex_usage.input_tokens, 300);
        assert_eq!(codex_usage.output_tokens, 200);
        assert_eq!(codex_usage.cache_read_tokens, 700);
        drop(host);

        let history = crate::usage_history::read_all(&history_path).expect("usage history");
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|record| record.nickname == "小雨"));
        assert!(history
            .iter()
            .all(|record| record.official_cost_microusd.is_some()));
    }

    #[tokio::test]
    async fn passenger_proxy_rejects_a_tampered_owner_response() {
        let access_id = Uuid::new_v4().to_string();
        let secret = crate::protocol::new_session_secret().expect("session secret");
        let state = RuntimeState::default();
        {
            let mut runtime = state.inner.lock().expect("runtime");
            runtime
                .access_secrets
                .insert(access_id.clone(), secret.clone());
            runtime.passenger_contexts.insert(
                access_id.clone(),
                PassengerAccessContext {
                    code: "7G2K5LQ8M4TZ".to_string(),
                    car_id: "car".to_string(),
                    owner_peer_id: "owner".to_string(),
                    owner_public_key: "public".to_string(),
                    owner_encryption_public_key: "encryption".to_string(),
                },
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {secret}")).expect("authorization"),
        );
        let response = process_buffered_request(
            "POST".to_string(),
            format!("/access/{access_id}/codex/v1/responses"),
            headers,
            Bytes::from_static(br#"{"model":"gpt-5.6-luna"}"#),
            state,
            |request| async move {
                Ok(RelayResponse {
                    request_id: request.request_id,
                    status_code: 200,
                    headers: Vec::new(),
                    body_base64: general_purpose::STANDARD.encode(b"tampered"),
                    body_sha256: sha256_label(b"original"),
                    latency_ms: 1,
                })
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("error body")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("完整性校验失败"));
    }
}
