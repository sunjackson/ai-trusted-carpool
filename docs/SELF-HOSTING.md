# 自建部署指南

可信拼车的协调服务与 TURN 中继都可以自建，与官方托管的 `p2p.cnaigc.ai` 完全同协议。自建包含三部分：

1. 协调服务（邀请注册/解析、信令信箱、TURN 时效凭据）——参考实现在 [`deploy/coordinator/`](../deploy/coordinator/)。
2. TURN 中继（NAT 穿透兜底）——推荐 [coturn](https://github.com/coturn/coturn)。
3. 桌面客户端指向自建地址——运行时环境变量 + 重新编译（CSP 与前端链接域名是编译期常量）。

## 1. 服务器端：docker-compose 参考

假设你的域名是 `carpool.example.org`，已解析到服务器公网 IP。

```yaml
# docker-compose.yml
services:
  coordinator:
    image: node:20-alpine
    restart: unless-stopped
    working_dir: /app
    volumes:
      - ./coordinator:/app:ro        # 复制仓库 deploy/coordinator/ 到这里
    command: ["node", "server.js"]
    environment:
      HOST: 0.0.0.0
      PORT: "18081"
      # 反向代理（nginx/caddy）在前面时开启，用 X-Forwarded-For 限速
      TRUSTED_CARPOOL_TRUST_PROXY: "1"
      # 与 coturn 的 static-auth-secret 保持一致
      TRUSTED_CARPOOL_TURN_SECRET: "换成足够长的随机字符串"
      TRUSTED_CARPOOL_TURN_URLS: "turn:carpool.example.org:3478?transport=udp,turn:carpool.example.org:3478?transport=tcp,turns:carpool.example.org:5349"
      TRUSTED_CARPOOL_TURN_TTL_SECONDS: "3600"
    ports:
      - "127.0.0.1:18081:18081"      # 只暴露给本机反向代理

  coturn:
    image: coturn/coturn:latest
    restart: unless-stopped
    network_mode: host               # TURN 需要大段 UDP 端口，host 网络最简单
    volumes:
      - ./turnserver.conf:/etc/coturn/turnserver.conf:ro
      - ./certs:/etc/coturn/certs:ro
```

```ini
# turnserver.conf
listening-port=3478
tls-listening-port=5349
realm=carpool.example.org
# 与协调服务 TRUSTED_CARPOOL_TURN_SECRET 一致（coturn REST API 时效凭据）
use-auth-secret
static-auth-secret=换成足够长的随机字符串
# TLS 证书（turns: 需要）
cert=/etc/coturn/certs/fullchain.pem
pkey=/etc/coturn/certs/privkey.pem
# 中继端口范围，防火墙需放行 UDP 3478、5349 与该范围
min-port=49152
max-port=65535
fingerprint
no-cli
# 只做中继，不允许回环/内网穿透
no-loopback-peers
denied-peer-ip=10.0.0.0-10.255.255.255
denied-peer-ip=172.16.0.0-172.31.255.255
denied-peer-ip=192.168.0.0-192.168.255.255
```

前面再加一层 HTTPS 反向代理（Caddy 示例）：

```
carpool.example.org {
    reverse_proxy 127.0.0.1:18081
}
```

### 协调服务环境变量

| 变量 | 说明 | 默认 |
| --- | --- | --- |
| `HOST` / `PORT` | 监听地址与端口 | `127.0.0.1` / `18081` |
| `TRUSTED_CARPOOL_TRUST_PROXY` | 设为 `1` 时信任 `X-Forwarded-For` 作限速来源 | 关闭 |
| `TRUSTED_CARPOOL_TURN_SECRET` | coturn `static-auth-secret` 共享密钥；不设置则 `/api/v1/turn-credentials` 返回 404 | 空 |
| `TRUSTED_CARPOOL_TURN_URLS` | 逗号分隔的 `turn:`/`turns:` 地址列表，**域名必须与协调服务域名一致**（客户端会校验） | 空 |
| `TRUSTED_CARPOOL_TURN_TTL_SECONDS` | 时效凭据有效期（上限 86400） | `3600` |

公开协调器默认免登录，靠签名与配额控滥用：

- 邀请注册 / 发信 / 轮询 / TURN：按 IP 与 peer 身份限速
- 每个 `owner_peer_id` 最多 16 条未过期邀请
- TURN 必须 `POST /api/v1/turn-credentials`，请求体携带设备身份签名（证明持有对应私钥）；裸 `GET ?peer_id=` 已拒绝

验证：

```bash
curl https://carpool.example.org/api/v1/health
# TURN 需客户端签名；下面仅演示旧式探测会失败（405）
curl -i "https://carpool.example.org/api/v1/turn-credentials?peer_id=p2p-AAAAAAAAAAAAAAAAAAAAAA"
```

客户端发车/上车时会自动签名申请 TURN，无需人工 curl。

## 2. 客户端：指向自建地址

客户端有三处「官方地址」，都必须换成你的域名：

### 2.1 运行时环境变量（Rust 后端）

| 变量 | 作用 |
| --- | --- |
| `TRUSTED_CARPOOL_COORDINATOR_URL` | 协调服务地址（如 `https://carpool.example.org`）。同时决定：HTTPS 上车链接允许的域名、TURN 中继地址必须匹配的域名 |
| `TRUSTED_CARPOOL_ALLOW_HTTP` | 设为 `1` 允许 `http://`（仅限本机调试，生产必须 HTTPS） |

### 2.2 前端编译期变量

前端复制上车链接与「来自 xxx」徽标使用编译期变量，构建前设置：

```bash
export VITE_TRUSTED_CARPOOL_COORDINATOR_URL="https://carpool.example.org"
```

### 2.3 CSP（`src-tauri/tauri.conf.json`）

WebView 的内容安全策略写死了官方域名，需要手工替换 `connect-src` 中的两处：

```json
"csp": "... connect-src 'self' ipc: http://ipc.localhost https://carpool.example.org wss://carpool.example.org"
```

### 2.4 重新编译

```bash
export VITE_TRUSTED_CARPOOL_COORDINATOR_URL="https://carpool.example.org"
npm ci && npm run tauri build
```

运行时（车主与乘客两侧都要）：

```bash
TRUSTED_CARPOOL_COORDINATOR_URL="https://carpool.example.org" ./可信拼车
```

macOS GUI 启动不继承 shell 环境变量，可用 `launchctl setenv TRUSTED_CARPOOL_COORDINATOR_URL https://carpool.example.org` 或从终端启动应用。

## 3. 已知限制

- 一键上车 HTTPS 链接不允许带端口（客户端严格校验），自建域名请使用 443 标准端口（放反向代理后面即可）。
- 参考实现把邀请与信箱保存在内存里，重启即清空；这对短时车队足够，长期运营请自行加持久化。
- 所有乘客与车主必须使用指向同一协调服务编译/配置的客户端，官方客户端不会接受自建域名生成的链接（这是防钓鱼校验，属预期行为）。
