export type ToolKind = 'claude' | 'codex';
export type LaunchMode = 'terminal' | 'desktop';

export type ToolDetection = {
  kind: ToolKind;
  name: string;
  installed: boolean;
  authenticated: boolean;
  executablePath: string | null;
  configPath: string | null;
  detail: string;
  version: string | null;
  npmAvailable: boolean;
  desktopSupported: boolean;
  desktopInstalled: boolean;
  desktopPath: string | null;
  desktopDetail: string;
};

export type ModelUsageSummary = {
  tool: ToolKind;
  model: string;
  requestCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  cacheWrite5mTokens: number;
  cacheWrite1hTokens: number;
  officialCostMicrousd: number | null;
  unpricedRequestCount: number;
  pricingSource: string | null;
  lastUsedAt: number;
};

export type SeatUsageSummary = {
  requestCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  cacheWrite5mTokens: number;
  cacheWrite1hTokens: number;
  totalTokens: number;
  officialCostMicrousd: number;
  unpricedRequestCount: number;
  lastUsedAt: number | null;
  models: ModelUsageSummary[];
};

export type MemberTokenLimits = {
  fiveHourTokens: number | null;
  dailyTokens: number | null;
  weeklyTokens: number | null;
};

export type TokenWindowStatus = {
  limitTokens: number | null;
  usedTokens: number;
  remainingTokens: number | null;
  resetsAt: number | null;
  exhausted: boolean;
};

export type MemberTokenLimitStatus = {
  fiveHour: TokenWindowStatus;
  daily: TokenWindowStatus;
  weekly: TokenWindowStatus;
};

export type Seat = {
  seatNo: number;
  code: string;
  nickname: string | null;
  state: 'waiting' | 'connected' | 'using' | 'blocked';
  tool: ToolKind | null;
  usage: SeatUsageSummary;
  tokenLimits: MemberTokenLimits;
  tokenLimitStatus: MemberTokenLimitStatus;
};

export type AccountQuotaWindow = {
  label: string;
  usedPercent: number;
  remainingPercent: number;
  resetsAt: number | null;
};

export type AccountQuotaSnapshot = {
  tool: ToolKind;
  state: 'pending' | 'available' | 'unsupported' | 'error';
  planName: string | null;
  fetchedAt: number | null;
  source: string;
  message: string | null;
  windows: AccountQuotaWindow[];
};

export type CarSession = {
  carId: string;
  carName: string;
  ownerPeerId: string;
  startedAt: number;
  expiresAt: number;
  enabledTools: ToolKind[];
  seats: Seat[];
  accountQuotas: AccountQuotaSnapshot[];
};

export type SharedMemberStatus = Omit<Seat, 'code'>;

export type SharedCarStatus = {
  carId: string;
  carName: string;
  startedAt: number;
  expiresAt: number;
  enabledTools: ToolKind[];
  accountQuotas: AccountQuotaSnapshot[];
  member: SharedMemberStatus;
};

export type JoinPreview = {
  carId: string;
  carName: string;
  ownerLabel: string;
  seatNo: number;
  enabledTools: ToolKind[];
  startsAt: number;
  expiresAt: number;
};

export type RideAccess = JoinPreview & {
  accessId: string;
  ownerPeerId: string;
  localProxyPort: number;
  connectionState: 'connecting' | 'connected' | 'degraded';
};

export type IceServer = {
  urls: string[];
  username: string | null;
  credential: string | null;
};

export type CoordinatorMessage = {
  id: string;
  fromPeerId: string;
  toPeerId: string;
  publicKey: string;
  kind: 'webrtc_offer' | 'webrtc_answer' | 'ice_candidate' | 'hangup';
  payloadJson: string;
  timestampMs: number;
};

export type RelayHeader = {
  name: string;
  value: string;
};

export type RelayRequest = {
  requestId: string;
  accessId: string;
  tool: ToolKind;
  method: string;
  path: string;
  headers: RelayHeader[];
  bodyBase64: string;
  bodySha256: string;
  timestampMs: number;
  authProof: string;
};

export type RelayResponse = {
  requestId: string;
  statusCode: number;
  headers: RelayHeader[];
  bodyBase64: string;
  bodySha256: string;
  latencyMs: number;
};

export type RelayStreamEvent = {
  requestId: string;
  kind: 'start' | 'chunk' | 'end' | 'error';
  statusCode?: number;
  headers?: RelayHeader[];
  chunkBase64?: string;
  bodySha256?: string;
  latencyMs?: number;
  error?: string;
};

export type RelayBridgeRequestEvent = {
  requestId: string;
  accessId: string;
  ownerPeerId: string;
  payloadJson: string;
  timeoutMs: number;
};
