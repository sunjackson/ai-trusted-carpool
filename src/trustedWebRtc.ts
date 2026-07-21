import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  CoordinatorMessage,
  IceServer,
  RelayBridgeRequestEvent,
  RelayRequest,
  RelayStreamEvent,
  RideAccess,
  SharedCarStatus,
} from './types';
import {
  acceptChunkFrame,
  createChunkFrames,
  type Assembly,
  type WireMessage,
} from './webrtcWire';

type Role = 'host' | 'passenger';
export type P2pConnectionState = 'connecting' | 'connected' | 'reconnecting' | 'down';
type SignalPayload = {
  sdp?: RTCSessionDescriptionInit;
  candidate?: RTCIceCandidateInit;
};
type Connection = {
  peerId: string;
  pc: RTCPeerConnection;
  channel: RTCDataChannel | null;
  pendingCandidates: RTCIceCandidateInit[];
  assemblies: Map<string, Assembly>;
  receiveChain: Promise<void>;
  sendChain: Promise<void>;
};

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;
const MAX_BUFFERED_AMOUNT = 1024 * 1024;
const POLL_INTERVAL_MS = 700;
const POLL_MAX_BACKOFF_MS = 5_000;
// The host broadcasts a status snapshot every 2s; missing six in a row means
// the link is effectively dead even if ICE has not noticed yet.
const HEARTBEAT_TIMEOUT_MS = 12_000;
const FAST_RECONNECT_ATTEMPTS = 6;
const RECONNECT_MAX_BACKOFF_MS = 15_000;
const RECONNECT_IDLE_BACKOFF_MS = 30_000;

const errorMessage = (error: unknown): string => {
  if (error instanceof Error) return error.message;
  if (typeof error === 'string') return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
};

class TrustedWebRtcRuntime {
  private role: Role | null = null;
  private accessId: string | null = null;
  private ownerPeerId: string | null = null;
  private connections = new Map<string, Connection>();
  private iceServers: RTCIceServer[] = [];
  private pollTimer: number | null = null;
  private pollGeneration = 0;
  private statusTimer: number | null = null;
  private unlisten: UnlistenFn[] = [];
  private pendingBridge = new Map<string, RelayBridgeRequestEvent>();
  private hostRequests = new Map<string, Connection>();
  private statusListeners = new Set<(status: SharedCarStatus) => void>();
  private lastStatus: SharedCarStatus | null = null;
  private connectionStateListeners = new Set<(state: P2pConnectionState) => void>();
  private p2pState: P2pConnectionState = 'connected';
  private lastSnapshotAt = 0;
  private healthTimer: number | null = null;
  private reconnecting = false;
  private stopping = false;

  subscribeCarStatus(listener: (status: SharedCarStatus) => void): () => void {
    this.statusListeners.add(listener);
    if (this.lastStatus) listener(this.lastStatus);
    return () => this.statusListeners.delete(listener);
  }

  subscribeConnectionState(listener: (state: P2pConnectionState) => void): () => void {
    this.connectionStateListeners.add(listener);
    listener(this.p2pState);
    return () => this.connectionStateListeners.delete(listener);
  }

  private setP2pState(next: P2pConnectionState): void {
    if (this.p2pState === next) return;
    this.p2pState = next;
    for (const listener of this.connectionStateListeners) listener(next);
  }

  async initialize(): Promise<void> {
    if (!inTauri() || this.unlisten.length > 0) return;
    const requestUnlisten = await listen<RelayBridgeRequestEvent>(
      'trusted-carpool:relay-request',
      event => void this.handleBridgeRequest(event.payload).catch(() => undefined)
    );
    const streamUnlisten = await listen<RelayStreamEvent>(
      'trusted-carpool:relay-stream-event',
      event => void this.handleHostStreamEvent(event.payload).catch(() => undefined)
    );
    this.unlisten = [requestUnlisten, streamUnlisten];
  }

  async startHost(): Promise<void> {
    if (!inTauri()) return;
    await this.initialize();
    this.closeConnections();
    this.stopping = false;
    this.role = 'host';
    this.accessId = null;
    this.ownerPeerId = null;
    this.setP2pState('connected');
    await this.loadIceServers();
    this.startPolling();
    this.startStatusBroadcast();
  }

  async startPassenger(access: RideAccess): Promise<void> {
    if (!inTauri()) return;
    if (typeof RTCPeerConnection === 'undefined') {
      throw new Error('当前系统 WebView 不支持 WebRTC，无法安全上车');
    }
    await this.initialize();
    this.closeConnections();
    this.stopping = false;
    this.role = 'passenger';
    this.accessId = access.accessId;
    this.ownerPeerId = access.ownerPeerId;
    this.lastStatus = null;
    this.setP2pState('connecting');
    await this.loadIceServers();
    this.startPolling();
    try {
      await this.dialOwner(access.ownerPeerId);
    } catch (error) {
      this.setP2pState('down');
      throw error;
    }
    this.setP2pState('connected');
    this.startHealthWatch();
  }

  /// Creates a fresh connection to the owner, sends an offer, and waits for
  /// the data channel to open. Shared by the first join and reconnects.
  private async dialOwner(ownerPeerId: string): Promise<void> {
    this.removeConnection(ownerPeerId);
    const connection = this.createConnection(ownerPeerId);
    const channel = connection.pc.createDataChannel('trusted-carpool-v1', { ordered: true });
    this.bindChannel(connection, channel);
    const offer = await connection.pc.createOffer();
    await connection.pc.setLocalDescription(offer);
    await this.sendSignal(ownerPeerId, 'webrtc_offer', {
      sdp: connection.pc.localDescription ?? offer,
    });
    await this.waitForOpen(connection, 20_000);
  }

  async stop(): Promise<void> {
    if (!inTauri()) return;
    this.stopping = true;
    const peers = [...this.connections.keys()];
    await Promise.allSettled(
      peers.map(peerId => this.sendSignal(peerId, 'hangup', {}))
    );
    this.closeConnections();
    this.role = null;
    this.accessId = null;
    this.ownerPeerId = null;
    this.setP2pState('connected');
  }

  private async loadIceServers(): Promise<void> {
    const servers = await invoke<IceServer[]>('get_ice_servers');
    this.iceServers = servers.map(server => ({
      urls: server.urls,
      username: server.username ?? undefined,
      credential: server.credential ?? undefined,
    }));
  }

  private closeConnections(): void {
    if (this.pollTimer !== null) window.clearTimeout(this.pollTimer);
    if (this.statusTimer !== null) window.clearInterval(this.statusTimer);
    if (this.healthTimer !== null) window.clearInterval(this.healthTimer);
    this.pollTimer = null;
    this.pollGeneration += 1;
    this.statusTimer = null;
    this.healthTimer = null;
    for (const connection of this.connections.values()) {
      connection.channel?.close();
      connection.pc.close();
    }
    this.connections.clear();
    this.pendingBridge.clear();
    this.hostRequests.clear();
    this.lastStatus = null;
    this.lastSnapshotAt = 0;
    this.reconnecting = false;
  }

  /// Closes and forgets a peer, dropping any in-flight relay bookkeeping
  /// that pointed at the dead connection.
  private removeConnection(peerId: string): void {
    const connection = this.connections.get(peerId);
    if (!connection) return;
    connection.channel?.close();
    connection.pc.close();
    this.connections.delete(peerId);
    for (const [requestId, owner] of this.hostRequests) {
      if (owner === connection) this.hostRequests.delete(requestId);
    }
  }

  private createConnection(peerId: string): Connection {
    const existing = this.connections.get(peerId);
    if (existing) return existing;
    const limit = this.role === 'host' ? 4 : 1;
    if (this.connections.size >= limit) {
      throw new Error(`连接人数已达上限 ${limit}`);
    }
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    const connection: Connection = {
      peerId,
      pc,
      channel: null,
      pendingCandidates: [],
      assemblies: new Map(),
      receiveChain: Promise.resolve(),
      sendChain: Promise.resolve(),
    };
    this.connections.set(peerId, connection);
    pc.onicecandidate = event => {
      if (event.candidate) {
        void this.sendSignal(peerId, 'ice_candidate', {
          candidate: event.candidate.toJSON(),
        }).catch(() => undefined);
      }
    };
    pc.ondatachannel = event => this.bindChannel(connection, event.channel);
    pc.onconnectionstatechange = () => {
      if (['failed', 'closed'].includes(pc.connectionState)) {
        connection.channel?.close();
        // Only forget the peer when this pc is still the active one; a
        // reconnect may already have replaced it.
        if (this.connections.get(peerId)?.pc === pc) {
          this.removeConnection(peerId);
        }
        this.scheduleReconnect(peerId);
      } else if (pc.connectionState === 'disconnected') {
        // Give ICE a moment to self-heal before tearing down.
        window.setTimeout(() => {
          if (
            this.connections.get(peerId)?.pc === pc &&
            pc.connectionState === 'disconnected'
          ) {
            this.removeConnection(peerId);
            this.scheduleReconnect(peerId);
          }
        }, 3_000);
      }
    };
    return connection;
  }

  /// Passenger-side automatic recovery: fast retries with exponential
  /// backoff, then slow persistent retries until the user leaves the car.
  private scheduleReconnect(peerId: string): void {
    if (
      this.role !== 'passenger' ||
      this.stopping ||
      this.reconnecting ||
      peerId !== this.ownerPeerId
    ) {
      return;
    }
    this.reconnecting = true;
    this.setP2pState('reconnecting');
    void (async () => {
      for (let attempt = 0; !this.stopping && this.role === 'passenger'; attempt += 1) {
        if (attempt >= FAST_RECONNECT_ATTEMPTS) this.setP2pState('down');
        const backoff =
          attempt >= FAST_RECONNECT_ATTEMPTS
            ? RECONNECT_IDLE_BACKOFF_MS
            : Math.min(1_000 * 2 ** attempt, RECONNECT_MAX_BACKOFF_MS);
        await new Promise(resolve => window.setTimeout(resolve, backoff));
        if (this.stopping || this.role !== 'passenger' || !this.ownerPeerId) break;
        try {
          // TURN credentials are time-limited; always fetch fresh ones.
          await this.loadIceServers();
          await this.dialOwner(this.ownerPeerId);
          this.lastSnapshotAt = Date.now();
          this.setP2pState('connected');
          break;
        } catch {
          // Keep retrying; the host may simply be offline for a while.
        }
      }
      this.reconnecting = false;
    })();
  }

  /// Uses the host's 2-second status broadcast as an application-level
  /// heartbeat, catching silent link death that ICE reports too late.
  private startHealthWatch(): void {
    if (this.healthTimer !== null) window.clearInterval(this.healthTimer);
    this.lastSnapshotAt = Date.now();
    this.healthTimer = window.setInterval(() => {
      if (this.role !== 'passenger' || this.stopping || this.reconnecting) return;
      if (this.p2pState !== 'connected') return;
      if (Date.now() - this.lastSnapshotAt <= HEARTBEAT_TIMEOUT_MS) return;
      const ownerPeerId = this.ownerPeerId;
      if (!ownerPeerId) return;
      this.removeConnection(ownerPeerId);
      this.scheduleReconnect(ownerPeerId);
    }, 2_000);
  }

  private bindChannel(connection: Connection, channel: RTCDataChannel): void {
    connection.channel = channel;
    channel.binaryType = 'arraybuffer';
    channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
    channel.onopen = () => {
      for (const event of [...this.pendingBridge.values()]) {
        if (event.ownerPeerId === connection.peerId) {
          this.pendingBridge.delete(event.requestId);
          void this.queueWire(connection, {
            type: 'relay_request',
            bridgeRequestId: event.requestId,
            payloadJson: event.payloadJson,
          }).catch(error => void this.failBridge(event.requestId, errorMessage(error)));
        }
      }
      if (this.role === 'host') {
        void this.sendCarStatus(connection).catch(() => undefined);
      } else if (this.role === 'passenger') {
        void this.queueWire(connection, {
          type: 'car_status_request',
          bridgeRequestId: crypto.randomUUID(),
          payloadJson: '{}',
        }).catch(() => undefined);
      }
    };
    channel.onmessage = event => {
      const raw = String(event.data);
      connection.receiveChain = connection.receiveChain
        .then(() => this.handleFrame(connection, raw))
        .catch(() => undefined);
    };
    channel.onclose = () => {
      if (connection.pc.connectionState === 'closed') {
        this.connections.delete(connection.peerId);
      }
    };
  }

  private waitForOpen(connection: Connection, timeoutMs: number): Promise<void> {
    if (connection.channel?.readyState === 'open') return Promise.resolve();
    return new Promise((resolve, reject) => {
      const deadline = window.setTimeout(() => {
        cleanup();
        reject(new Error('连接车主超时，请确认车主应用保持打开'));
      }, timeoutMs);
      const check = window.setInterval(() => {
        if (connection.channel?.readyState === 'open') {
          cleanup();
          resolve();
        } else if (['failed', 'closed'].includes(connection.pc.connectionState)) {
          cleanup();
          reject(new Error('无法建立安全连接'));
        }
      }, 100);
      const cleanup = () => {
        window.clearTimeout(deadline);
        window.clearInterval(check);
      };
    });
  }

  private async waitForBuffer(channel: RTCDataChannel): Promise<void> {
    if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return;
    await new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        channel.removeEventListener('bufferedamountlow', onLow);
        reject(new Error('安全连接发送缓冲区超时'));
      }, 10_000);
      const onLow = () => {
        window.clearTimeout(timeout);
        resolve();
      };
      channel.addEventListener('bufferedamountlow', onLow, { once: true });
    });
  }

  private async sendWire(connection: Connection, message: WireMessage): Promise<void> {
    const channel = connection.channel;
    if (!channel || channel.readyState !== 'open') throw new Error('安全连接尚未就绪');
    for (const frame of createChunkFrames(message)) {
      await this.waitForBuffer(channel);
      channel.send(frame);
    }
  }

  private queueWire(connection: Connection, message: WireMessage): Promise<void> {
    const sending = connection.sendChain.then(() => this.sendWire(connection, message));
    connection.sendChain = sending.catch(() => undefined);
    return sending;
  }

  private async handleFrame(connection: Connection, raw: string): Promise<void> {
    const message = acceptChunkFrame(connection.assemblies, raw);
    if (message) await this.handleWireMessage(connection, message);
  }

  private async handleWireMessage(connection: Connection, message: WireMessage): Promise<void> {
    if (message.type === 'relay_request' && this.role === 'host') {
      let request: RelayRequest | null = null;
      try {
        request = JSON.parse(message.payloadJson) as RelayRequest;
        if (!request.requestId || request.requestId !== message.bridgeRequestId) {
          throw new Error('中转请求编号不匹配');
        }
        this.hostRequests.set(request.requestId, connection);
        await invoke<boolean>('start_relay_request', { request });
      } catch (error) {
        if (request?.requestId) this.hostRequests.delete(request.requestId);
        await this.queueWire(connection, {
          type: 'relay_stream_event',
          bridgeRequestId: message.bridgeRequestId,
          payloadJson: JSON.stringify({
            requestId: message.bridgeRequestId,
            kind: 'error',
            error: errorMessage(error),
          } satisfies RelayStreamEvent),
        });
      }
      return;
    }
    if (message.type === 'car_status_request' && this.role === 'host') {
      await this.sendCarStatus(connection, message.bridgeRequestId);
      return;
    }
    if (message.type === 'car_status_snapshot' && this.role === 'passenger') {
      const status = JSON.parse(message.payloadJson) as SharedCarStatus;
      if (!status.carId || !status.member || !Array.isArray(status.accountQuotas)) {
        throw new Error('车队状态格式无效');
      }
      this.lastSnapshotAt = Date.now();
      this.lastStatus = status;
      for (const listener of this.statusListeners) listener(status);
      return;
    }
    if (message.type === 'relay_stream_event' && this.role === 'passenger') {
      const event = JSON.parse(message.payloadJson) as RelayStreamEvent;
      if (event.requestId !== message.bridgeRequestId) {
        throw new Error('流式响应编号不匹配');
      }
      await invoke<boolean>('submit_relay_stream_event', { event });
      return;
    }
    if (message.type === 'relay_response' && this.role === 'passenger') {
      await invoke<boolean>('submit_relay_response', {
        requestId: message.bridgeRequestId,
        payloadJson: message.payloadJson,
      });
    }
  }

  private async handleHostStreamEvent(event: RelayStreamEvent): Promise<void> {
    if (this.role !== 'host') return;
    const connection = this.hostRequests.get(event.requestId);
    if (!connection) return;
    try {
      await this.queueWire(connection, {
        type: 'relay_stream_event',
        bridgeRequestId: event.requestId,
        payloadJson: JSON.stringify(event),
      });
    } finally {
      if (event.kind === 'end' || event.kind === 'error') {
        this.hostRequests.delete(event.requestId);
      }
    }
  }

  private async sendCarStatus(
    connection: Connection,
    requestId: string = crypto.randomUUID()
  ): Promise<void> {
    if (this.role !== 'host' || connection.channel?.readyState !== 'open') return;
    const status = await invoke<SharedCarStatus>('get_shared_car_status', {
      passengerPeerId: connection.peerId,
    });
    await this.queueWire(connection, {
      type: 'car_status_snapshot',
      bridgeRequestId: requestId,
      payloadJson: JSON.stringify(status),
    });
  }

  private startStatusBroadcast(): void {
    if (this.statusTimer !== null) window.clearInterval(this.statusTimer);
    if (this.role !== 'host') return;
    const broadcast = () => {
      for (const connection of this.connections.values()) {
        void this.sendCarStatus(connection).catch(() => undefined);
      }
    };
    this.statusTimer = window.setInterval(broadcast, 2_000);
  }

  private async sendSignal(
    toPeerId: string,
    kind: CoordinatorMessage['kind'],
    payload: SignalPayload
  ): Promise<void> {
    await invoke('send_webrtc_signal', {
      input: { toPeerId, kind, payloadJson: JSON.stringify(payload) },
    });
  }

  private async handleSignal(message: CoordinatorMessage): Promise<void> {
    let payload: SignalPayload;
    try {
      payload = JSON.parse(message.payloadJson) as SignalPayload;
    } catch {
      return;
    }
    if (message.kind === 'hangup') {
      if (this.role === 'passenger' && message.fromPeerId === this.ownerPeerId) {
        // The owner ended the car; reconnecting would be futile.
        this.stopping = true;
        this.setP2pState('down');
      }
      this.removeConnection(message.fromPeerId);
      return;
    }
    if (this.role === 'host' && message.kind === 'webrtc_offer' && payload.sdp) {
      // A fresh offer supersedes any previous session with this passenger
      // (for example after their app crashed and they rejoined quickly).
      this.removeConnection(message.fromPeerId);
      if (this.connections.size >= 4) {
        await this.sendSignal(message.fromPeerId, 'hangup', {});
        return;
      }
      const connection = this.createConnection(message.fromPeerId);
      await connection.pc.setRemoteDescription(payload.sdp);
      await this.flushCandidates(connection);
      const answer = await connection.pc.createAnswer();
      await connection.pc.setLocalDescription(answer);
      await this.sendSignal(message.fromPeerId, 'webrtc_answer', {
        sdp: connection.pc.localDescription ?? answer,
      });
      return;
    }
    if (this.role === 'passenger' && message.kind === 'webrtc_answer' && payload.sdp) {
      const connection = this.connections.get(message.fromPeerId);
      if (!connection) return;
      await connection.pc.setRemoteDescription(payload.sdp);
      await this.flushCandidates(connection);
      return;
    }
    if (message.kind === 'ice_candidate' && payload.candidate) {
      const connection = this.connections.get(message.fromPeerId);
      if (!connection) return;
      if (connection.pc.remoteDescription) {
        await connection.pc.addIceCandidate(payload.candidate);
      } else {
        connection.pendingCandidates.push(payload.candidate);
      }
    }
  }

  private async flushCandidates(connection: Connection): Promise<void> {
    for (const candidate of connection.pendingCandidates.splice(0)) {
      await connection.pc.addIceCandidate(candidate);
    }
  }

  // Self-scheduling loop: polls never overlap, and coordinator outages back
  // off exponentially (700ms up to 5s) instead of hammering at full rate.
  private startPolling(): void {
    if (this.pollTimer !== null) window.clearTimeout(this.pollTimer);
    const generation = (this.pollGeneration += 1);
    let delay = POLL_INTERVAL_MS;
    const loop = async () => {
      if (!this.role || generation !== this.pollGeneration) return;
      try {
        const messages = await invoke<CoordinatorMessage[]>('poll_webrtc_signals', {
          accessId: this.role === 'passenger' ? this.accessId : null,
        });
        delay = POLL_INTERVAL_MS;
        for (const message of messages) {
          await this.handleSignal(message).catch(() => undefined);
        }
      } catch {
        // Polling keeps retrying; passenger connection setup has its own visible timeout.
        delay = Math.min(delay * 2, POLL_MAX_BACKOFF_MS);
      }
      if (!this.role || generation !== this.pollGeneration) return;
      this.pollTimer = window.setTimeout(() => void loop(), delay);
    };
    void loop();
  }

  private async handleBridgeRequest(event: RelayBridgeRequestEvent): Promise<void> {
    if (this.role !== 'passenger' || event.accessId !== this.accessId) {
      await this.failBridge(event.requestId, '当前上车会话没有可用的安全连接');
      return;
    }
    const connection = this.connections.get(event.ownerPeerId);
    if (!connection || connection.channel?.readyState !== 'open') {
      this.pendingBridge.set(event.requestId, event);
      window.setTimeout(() => {
        if (this.pendingBridge.delete(event.requestId)) {
          void this.failBridge(event.requestId, '安全连接尚未就绪，请确认车主保持在线');
        }
      }, Math.min(event.timeoutMs, 15_000));
      return;
    }
    try {
      await this.queueWire(connection, {
        type: 'relay_request',
        bridgeRequestId: event.requestId,
        payloadJson: event.payloadJson,
      });
    } catch (error) {
      await this.failBridge(event.requestId, errorMessage(error));
    }
  }

  private async failBridge(requestId: string, error: string): Promise<void> {
    await invoke<boolean>('submit_relay_response', {
      requestId,
      payloadJson: JSON.stringify({ error }),
    });
  }
}

export const trustedWebRtc = new TrustedWebRtcRuntime();
