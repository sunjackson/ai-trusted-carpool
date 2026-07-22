import { Channel } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { invoke } from './tauriInvoke';
import type {
  AccountImportInput,
  AccountImportPreview,
  AccountImportPreviewInput,
  AccountImportResult,
  AccountPreviewAction,
  AccountPreviewItem,
  AccountRestoreMode,
  AccountRestorePreview,
  AccountRestoreResult,
  AccountUpdateInput,
  AppUpdateInfo,
  AppUpdateDownloadProgress,
  AppUpdateDownloadResult,
  SignedAppUpdateInfo,
  CarSession,
  JoinPreview,
  MemberTokenLimits,
  ModelUsageSummary,
  RideAccess,
  Seat,
  SeatUsageSummary,
  ToolDetection,
  ToolInstallProgress,
  ToolKind,
  LaunchMode,
  LocalAccountSummary,
  ClientInstanceSummary,
  ToolLaunchResult,
  AccountRouteHealth,
  CredentialState,
  RouteHealthReason,
  LocalAccountRefreshNotice,
} from './types';

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;
const JOIN_LINK_EVENT = 'trusted-carpool:join-link';
const LOCAL_ACCOUNT_REFRESH_EVENT = 'trusted-carpool:local-account-refresh';
const JOIN_CODE_PATTERN = /^[A-HJ-NP-Z2-9]{12}$/;
const DEFAULT_COORDINATOR_URL = 'https://p2p.cnaigc.ai';

// Self-hosted builds point elsewhere via VITE_TRUSTED_CARPOOL_COORDINATOR_URL
// (see docs/SELF-HOSTING.md); the Rust side follows TRUSTED_CARPOOL_COORDINATOR_URL.
export function coordinatorBaseUrl(): string {
  const configured = import.meta.env.VITE_TRUSTED_CARPOOL_COORDINATOR_URL;
  if (typeof configured === 'string' && configured.trim()) {
    return configured.trim().replace(/\/+$/, '');
  }
  return DEFAULT_COORDINATOR_URL;
}

export function coordinatorHost(): string {
  try {
    return new URL(coordinatorBaseUrl()).host;
  } catch {
    return new URL(DEFAULT_COORDINATOR_URL).host;
  }
}

export function serverJoinUrl(code: string): string {
  const normalized = code.trim().toUpperCase();
  if (!JOIN_CODE_PATTERN.test(normalized)) throw new Error('上车码格式不正确');
  return `${coordinatorBaseUrl()}/api/v1/carpool/join/${normalized}`;
}

export async function takePendingJoinCode(): Promise<string | null> {
  return inTauri() ? invoke<string | null>('take_pending_join_code') : null;
}

export async function listenForJoinLinks(
  onCode: (code: string) => void
): Promise<UnlistenFn> {
  if (!inTauri()) return () => undefined;
  return listen<string>(JOIN_LINK_EVENT, event => {
    if (JOIN_CODE_PATTERN.test(event.payload)) onCode(event.payload);
  });
}

export async function listenForLocalAccountRefresh(
  onNotice: (notice: LocalAccountRefreshNotice) => void
): Promise<UnlistenFn> {
  if (!inTauri()) return () => undefined;
  return listen<LocalAccountRefreshNotice>(LOCAL_ACCOUNT_REFRESH_EVENT, event => {
    if (event.payload.discovered > 0) onNotice(event.payload);
  });
}

const demoTools: ToolDetection[] = [
  {
    kind: 'claude',
    name: 'Claude Code',
    installed: true,
    authenticated: true,
    executablePath: '/usr/local/bin/claude',
    configPath: '~/.claude',
    detail: '已就绪',
    version: 'v2.1.178',
    npmAvailable: true,
    managedByApp: false,
    latestVersion: null,
    updateAvailable: false,
    desktopSupported: true,
    desktopInstalled: true,
    desktopPath: '/Applications/Claude.app',
    desktopDetail: '已安装，可使用拼车配置独立启动',
  },
  {
    kind: 'codex',
    name: 'Codex',
    installed: true,
    authenticated: true,
    executablePath: '/usr/local/bin/codex',
    configPath: '~/.codex/auth.json',
    detail: '已就绪',
    version: 'v0.140.0',
    npmAvailable: true,
    managedByApp: false,
    latestVersion: null,
    updateAvailable: false,
    desktopSupported: true,
    desktopInstalled: true,
    desktopPath: '/Applications/ChatGPT.app',
    desktopDetail: '已找到 ChatGPT.app（Codex 客户端），可使用拼车配置独立启动',
  },
];

const demoModels = (seatIndex: number): ModelUsageSummary[] => {
  const now = Date.now();
  if (seatIndex === 0) {
    return [
      {
        tool: 'claude',
        model: 'claude-sonnet-4-6',
        requestCount: 12,
        inputTokens: 8400,
        outputTokens: 2500,
        cacheReadTokens: 5200,
        cacheWriteTokens: 1500,
        cacheWrite5mTokens: 1100,
        cacheWrite1hTokens: 400,
        officialCostMicrousd: 70785,
        unpricedRequestCount: 0,
        pricingSource: 'https://platform.claude.com/docs/en/about-claude/pricing',
        lastUsedAt: now,
      },
      {
        tool: 'claude',
        model: 'claude-haiku-4-5',
        requestCount: 6,
        inputTokens: 4000,
        outputTokens: 1100,
        cacheReadTokens: 2200,
        cacheWriteTokens: 0,
        cacheWrite5mTokens: 0,
        cacheWrite1hTokens: 0,
        officialCostMicrousd: 9720,
        unpricedRequestCount: 0,
        pricingSource: 'https://platform.claude.com/docs/en/about-claude/pricing',
        lastUsedAt: now,
      },
    ];
  }
  if (seatIndex === 1) {
    return [
      {
        tool: 'codex',
        model: 'gpt-5.6-luna',
        requestCount: 9,
        inputTokens: 6800,
        outputTokens: 2100,
        cacheReadTokens: 4100,
        cacheWriteTokens: 1200,
        cacheWrite5mTokens: 1200,
        cacheWrite1hTokens: 0,
        officialCostMicrousd: 21310,
        unpricedRequestCount: 0,
        pricingSource: 'https://developers.openai.com/api/docs/pricing',
        lastUsedAt: now,
      },
    ];
  }
  return [];
};

const demoUsage = (seatIndex: number): SeatUsageSummary => {
  const models = demoModels(seatIndex);
  return {
    requestCount: models.reduce((total, model) => total + model.requestCount, 0),
    inputTokens: models.reduce((total, model) => total + model.inputTokens, 0),
    outputTokens: models.reduce((total, model) => total + model.outputTokens, 0),
    cacheReadTokens: models.reduce((total, model) => total + model.cacheReadTokens, 0),
    cacheWriteTokens: models.reduce((total, model) => total + model.cacheWriteTokens, 0),
    cacheWrite5mTokens: models.reduce((total, model) => total + model.cacheWrite5mTokens, 0),
    cacheWrite1hTokens: models.reduce((total, model) => total + model.cacheWrite1hTokens, 0),
    totalTokens: models.reduce(
      (total, model) =>
        total +
        model.inputTokens +
        model.outputTokens +
        model.cacheReadTokens +
        model.cacheWriteTokens,
      0
    ),
    officialCostMicrousd: models.reduce(
      (total, model) => total + (model.officialCostMicrousd ?? 0),
      0
    ),
    unpricedRequestCount: models.reduce(
      (total, model) => total + model.unpricedRequestCount,
      0
    ),
    lastUsedAt: models.length > 0 ? Math.max(...models.map(model => model.lastUsedAt)) : null,
    models,
  };
};

const unlimitedWindow = () => ({
  limitTokens: null,
  usedTokens: 0,
  remainingTokens: null,
  resetsAt: null,
  exhausted: false,
});

const demoTokenLimits = (seatIndex: number): MemberTokenLimits =>
  seatIndex === 0
    ? { fiveHourTokens: 60_000, dailyTokens: 180_000, weeklyTokens: 800_000 }
    : { fiveHourTokens: null, dailyTokens: null, weeklyTokens: null };

const demoTokenLimitStatus = (seatIndex: number) => {
  const usage = demoUsage(seatIndex).totalTokens;
  const limits = demoTokenLimits(seatIndex);
  const status = (limitTokens: number | null, ratio: number) => ({
    limitTokens,
    usedTokens: Math.round(usage * ratio),
    remainingTokens: limitTokens === null ? null : Math.max(0, limitTokens - Math.round(usage * ratio)),
    resetsAt: limitTokens === null ? null : Date.now() + 2 * 60 * 60 * 1000,
    exhausted: limitTokens !== null && Math.round(usage * ratio) >= limitTokens,
  });
  return {
    fiveHour: limits.fiveHourTokens === null ? unlimitedWindow() : status(limits.fiveHourTokens, 0.55),
    daily: limits.dailyTokens === null ? unlimitedWindow() : status(limits.dailyTokens, 0.8),
    weekly: limits.weeklyTokens === null ? unlimitedWindow() : status(limits.weeklyTokens, 1),
  };
};

const demoCar = (
  enabledTools: ToolKind[],
  carName: string,
  startsAt: number,
  endsAt: number
): CarSession => ({
  carId: crypto.randomUUID(),
  carName,
  ownerPeerId: 'p2p-demo-owner',
  startedAt: startsAt,
  expiresAt: endsAt,
  enabledTools,
  seats: Array.from({ length: 4 }, (_, index) => ({
    seatNo: index + 1,
    code: ['7G2K5LQ8M4TZ', 'M9Q3TP7W6KXR', 'CR8W4N2HJ7KM', 'K2J7HX9P4WQM'][index],
    nickname: index < 2 ? ['阿杰', '小雨'][index] : null,
    state: index < 2 ? 'using' : 'waiting',
    tool: index === 0 ? 'claude' : index === 1 ? 'codex' : null,
    usage: demoUsage(index),
    tokenLimits: demoTokenLimits(index),
    tokenLimitStatus: demoTokenLimitStatus(index),
  })),
  accountQuotas: enabledTools.map(kind => ({
    tool: kind,
    state: 'available' as const,
    planName: kind === 'claude' ? 'Max' : 'Plus',
    fetchedAt: Date.now(),
    source:
      kind === 'claude'
        ? 'https://api.anthropic.com/api/oauth/usage'
        : 'https://chatgpt.com/backend-api/wham/usage',
    message: null,
    windows:
      kind === 'claude'
        ? [
            { label: '5 小时', usedPercent: 36, remainingPercent: 64, resetsAt: Date.now() + 90 * 60 * 1000 },
            { label: '7 天', usedPercent: 22, remainingPercent: 78, resetsAt: Date.now() + 4 * 24 * 60 * 60 * 1000 },
          ]
        : [
            { label: '5 小时', usedPercent: 42, remainingPercent: 58, resetsAt: Date.now() + 2 * 60 * 60 * 1000 },
            { label: '7 天', usedPercent: 18, remainingPercent: 82, resetsAt: Date.now() + 5 * 24 * 60 * 60 * 1000 },
          ],
  })),
});

let demoActiveCar: CarSession | null = null;
let demoAccounts: LocalAccountSummary[] = [];
const demoImportPreviews = new Map<string, LocalAccountSummary[]>();

const EMPTY_ROUTE_HEALTH: AccountRouteHealth = {
  status: 'normal',
  reason: null,
  cooldownUntilMs: null,
  consecutiveFailures: 0,
  lastAttemptAtMs: null,
  lastSuccessAtMs: null,
  lastFailureAtMs: null,
};

const nullableNumber = (value: unknown): number | null =>
  value === null || value === undefined || !Number.isFinite(Number(value)) ? null : Number(value);

const normalizeRouteHealth = (value: unknown): AccountRouteHealth => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const reason = String(record.reason ?? '');
  const allowedReasons: RouteHealthReason[] = [
    'network',
    'authentication',
    'rateLimited',
    'upstream',
    'expired',
  ];
  return {
    status: record.status === 'cooling' ? 'cooling' : 'normal',
    reason: allowedReasons.includes(reason as RouteHealthReason)
      ? (reason as RouteHealthReason)
      : null,
    cooldownUntilMs: nullableNumber(record.cooldownUntilMs ?? record.cooldown_until_ms),
    consecutiveFailures: Math.max(0, Number(record.consecutiveFailures ?? record.consecutive_failures ?? 0) || 0),
    lastAttemptAtMs: nullableNumber(record.lastAttemptAtMs ?? record.last_attempt_at_ms),
    lastSuccessAtMs: nullableNumber(record.lastSuccessAtMs ?? record.last_success_at_ms),
    lastFailureAtMs: nullableNumber(record.lastFailureAtMs ?? record.last_failure_at_ms),
  };
};

const normalizeAccountSummary = (value: unknown): LocalAccountSummary => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const authKind = String(record.authKind ?? record.auth_kind ?? 'apiKey').toLowerCase();
  const source = String(record.source ?? 'unknown');
  const rawCredentialState = String(
    record.credentialState ?? record.credential_state ?? 'normal'
  );
  const credentialState: CredentialState =
    rawCredentialState === 'expired' || rawCredentialState === 'reimportRequired'
      ? rawCredentialState
      : 'normal';
  return {
    id: String(record.id ?? crypto.randomUUID()),
    tool: record.tool === 'claude' ? 'claude' : 'codex',
    name: String(record.name ?? '未命名账号'),
    authKind: authKind === 'oauth' || authKind === 'o_auth' ? 'oauth' : 'apiKey',
    enabled: record.enabled !== false,
    priority: Number.isFinite(Number(record.priority)) ? Number(record.priority) : 100,
    source,
    createdAtMs: Number(record.createdAtMs ?? record.created_at_ms ?? Date.now()),
    updatedAtMs: Number(record.updatedAtMs ?? record.updated_at_ms ?? Date.now()),
    credentialState,
    routeHealth: normalizeRouteHealth(record.routeHealth ?? record.route_health),
  };
};

const normalizeAccountList = (value: unknown): LocalAccountSummary[] => {
  if (Array.isArray(value)) return value.map(normalizeAccountSummary);
  if (value && typeof value === 'object') {
    const record = value as Record<string, unknown>;
    if (Array.isArray(record.accounts)) return record.accounts.map(normalizeAccountSummary);
    if (record.account) return [normalizeAccountSummary(record.account)];
  }
  return [];
};

const normalizeImportCount = (value: unknown, fallback: number): number => {
  const count = typeof value === 'number' ? value : Number(value);
  return Number.isSafeInteger(count) && count >= 0 ? count : fallback;
};

const normalizeAccountImportResult = (value: unknown): AccountImportResult => {
  if (Array.isArray(value)) {
    const accounts = value.map(normalizeAccountSummary);
    return { imported: accounts.length, updated: 0, accounts };
  }
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const accounts = normalizeAccountList(value);
  return {
    imported: normalizeImportCount(record.imported, accounts.length),
    updated: normalizeImportCount(record.updated, 0),
    accounts,
  };
};

const normalizeAccountPreviewItem = (value: unknown, index: number): AccountPreviewItem => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const rawAction = String(record.action ?? 'conflict');
  const action: AccountPreviewAction =
    rawAction === 'new' || rawAction === 'update' ? rawAction : 'conflict';
  const rawSource = String(record.source ?? 'json');
  return {
    itemId: String(record.itemId ?? record.item_id ?? `item-${index + 1}`),
    tool: record.tool === 'claude' ? 'claude' : 'codex',
    authKind: record.authKind === 'oauth' || record.auth_kind === 'oauth' ? 'oauth' : 'apiKey',
    name: String(record.name ?? '未命名账号'),
    source: rawSource === 'local' || rawSource === 'file' ? rawSource : 'json',
    action,
  };
};

const normalizeAccountImportPreview = (value: unknown): AccountImportPreview => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  const items = Array.isArray(record.items)
    ? record.items.map(normalizeAccountPreviewItem)
    : [];
  return {
    sessionId: String(record.sessionId ?? record.session_id ?? ''),
    expiresAtMs: Number(record.expiresAtMs ?? record.expires_at_ms ?? Date.now()),
    items,
  };
};

const normalizeAccountRestorePreview = (value: unknown): AccountRestorePreview => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  return {
    ...normalizeAccountImportPreview(value),
    mode: record.mode === 'replace' ? 'replace' : 'merge',
    removeCount: normalizeImportCount(record.removeCount ?? record.remove_count, 0),
  };
};

const normalizeAccountRestoreResult = (value: unknown): AccountRestoreResult => {
  const record = (value && typeof value === 'object' ? value : {}) as Record<string, unknown>;
  return {
    ...normalizeAccountImportResult(value),
    removed: normalizeImportCount(record.removed, 0),
  };
};

const inferDemoAccount = (
  content: string,
  tool: ToolKind | undefined,
  name: string | undefined,
  index: number
): LocalAccountSummary => {
  const normalized = content.trim();
  let parsed: Record<string, unknown> | null = null;
  try {
    const value = JSON.parse(normalized) as unknown;
    if (value && typeof value === 'object' && !Array.isArray(value)) {
      parsed = value as Record<string, unknown>;
    }
  } catch {
    // Raw API keys are supported alongside JSON credentials.
  }
  const serialized = parsed ? JSON.stringify(parsed) : normalized;
  const inferredTool =
    tool ??
    (serialized.includes('ANTHROPIC_API_KEY') || normalized.startsWith('sk-ant-')
      ? 'claude'
      : 'codex');
  const authKind: LocalAccountSummary['authKind'] =
    serialized.includes('access_token') || serialized.includes('accessToken') ? 'oauth' : 'apiKey';
  const now = Date.now();
  return {
    id: crypto.randomUUID(),
    tool: inferredTool,
    name: name?.trim() || `${inferredTool === 'claude' ? 'Claude' : 'Codex'} 账号${index > 0 ? ` ${index + 1}` : ''}`,
    authKind,
    enabled: true,
    priority: 100,
    source: '手动导入',
    createdAtMs: now,
    updatedAtMs: now,
    credentialState: 'normal',
    routeHealth: { ...EMPTY_ROUTE_HEALTH },
  };
};

const demoImportItems = (input: AccountImportInput): LocalAccountSummary[] => {
  const normalized = input.content.trim();
  if (!normalized) throw new Error('请输入 API Key 或账号 JSON');
  try {
    const parsed = JSON.parse(normalized) as unknown;
    if (Array.isArray(parsed)) {
      if (parsed.length === 0) throw new Error('账号 JSON 中没有可导入的记录');
      return parsed.map((item, index) =>
        inferDemoAccount(JSON.stringify(item), input.tool, input.name, index)
      );
    }
  } catch (error) {
    if (error instanceof Error && error.message.includes('没有可导入')) throw error;
  }
  return [inferDemoAccount(normalized, input.tool, input.name, 0)];
};

export async function listAccounts(): Promise<LocalAccountSummary[]> {
  return inTauri()
    ? invoke<unknown>('list_accounts').then(normalizeAccountList)
    : structuredClone(demoAccounts);
}

export async function retryAccountRoute(id: string): Promise<LocalAccountSummary> {
  if (inTauri()) {
    return invoke<unknown>('retry_account_route', { id }).then(normalizeAccountSummary);
  }
  const account = demoAccounts.find(candidate => candidate.id === id);
  if (!account) throw new Error('账号不存在');
  account.routeHealth = { ...EMPTY_ROUTE_HEALTH };
  return structuredClone(account);
}

export async function importLocalAccounts(): Promise<AccountImportResult> {
  return inTauri()
    ? invoke<unknown>('import_local_accounts').then(normalizeAccountImportResult)
    : { imported: 0, updated: 0, accounts: [] };
}

export async function importAccounts(
  input: AccountImportInput
): Promise<AccountImportResult> {
  const { displayName, ...rest } = input;
  const commandInput = {
    ...rest,
    ...(rest.name === undefined && displayName !== undefined ? { name: displayName } : {}),
  };
  if (inTauri()) {
    return invoke<unknown>('import_accounts', { input: commandInput }).then(normalizeAccountImportResult);
  }
  const imported = demoImportItems(commandInput);
  demoAccounts = [...demoAccounts, ...imported];
  return { imported: imported.length, updated: 0, accounts: structuredClone(imported) };
}

export async function previewAccountImport(
  input: AccountImportPreviewInput
): Promise<AccountImportPreview> {
  if (inTauri()) {
    return invoke<unknown>('preview_account_import', { input }).then(normalizeAccountImportPreview);
  }
  const accounts = input.local
    ? []
    : (input.contents?.length ? input.contents : [input.content ?? ''])
        .flatMap(content =>
          demoImportItems({
            content,
            tool: input.tool,
            name: input.name,
            source: input.source,
          })
        );
  const sessionId = crypto.randomUUID();
  demoImportPreviews.set(sessionId, accounts);
  return {
    sessionId,
    expiresAtMs: Date.now() + 10 * 60_000,
    items: accounts.map((account, index) => ({
      itemId: `item-${index + 1}`,
      tool: account.tool,
      authKind: account.authKind,
      name: account.name,
      source: input.local ? 'local' : input.source === 'file' ? 'file' : 'json',
      action: 'new',
    })),
  };
}

export async function commitAccountImport(sessionId: string): Promise<AccountImportResult> {
  if (inTauri()) {
    return invoke<unknown>('commit_account_import', { sessionId }).then(
      normalizeAccountImportResult
    );
  }
  const accounts = demoImportPreviews.get(sessionId);
  if (!accounts) throw new Error('账号预览已过期或不存在，请重新预览');
  demoImportPreviews.delete(sessionId);
  demoAccounts = [...demoAccounts, ...accounts];
  return { imported: accounts.length, updated: 0, accounts: structuredClone(accounts) };
}

export async function cancelAccountImport(sessionId: string): Promise<boolean> {
  if (inTauri()) return invoke<boolean>('cancel_account_import', { sessionId });
  return demoImportPreviews.delete(sessionId);
}

export async function exportAccountBackup(passphrase: string): Promise<string> {
  if (!inTauri()) throw new Error('加密账号备份仅在桌面应用中可用');
  return invoke<string>('export_account_backup', { passphrase });
}

export async function previewAccountRestore(input: {
  content: string;
  passphrase: string;
  mode: AccountRestoreMode;
}): Promise<AccountRestorePreview> {
  if (!inTauri()) throw new Error('账号备份恢复仅在桌面应用中可用');
  return invoke<unknown>('preview_account_restore', { input }).then(
    normalizeAccountRestorePreview
  );
}

export async function commitAccountRestore(
  sessionId: string,
  mode: AccountRestoreMode,
  confirmReplace: boolean
): Promise<AccountRestoreResult> {
  if (!inTauri()) throw new Error('账号备份恢复仅在桌面应用中可用');
  return invoke<unknown>('commit_account_restore', { sessionId, mode, confirmReplace }).then(
    normalizeAccountRestoreResult
  );
}

export async function cancelAccountRestore(sessionId: string): Promise<boolean> {
  if (!inTauri()) return false;
  return invoke<boolean>('cancel_account_restore', { sessionId });
}

export async function updateAccount(input: AccountUpdateInput): Promise<LocalAccountSummary> {
  const { displayName, ...rest } = input;
  const commandInput = {
    ...rest,
    ...(rest.name === undefined && displayName !== undefined ? { name: displayName } : {}),
  };
  if (inTauri()) {
    return invoke<unknown>('update_account', { input: commandInput }).then(value => {
      if (value && typeof value === 'object' && 'account' in value) {
        return normalizeAccountSummary((value as { account: unknown }).account);
      }
      return normalizeAccountSummary(value);
    });
  }
  const index = demoAccounts.findIndex(account => account.id === commandInput.id);
  if (index < 0) throw new Error('账号不存在');
  const current = demoAccounts[index];
  const updated = {
    ...current,
    ...(commandInput.name === undefined ? {} : { name: commandInput.name }),
    ...(commandInput.enabled === undefined ? {} : { enabled: commandInput.enabled }),
    ...(commandInput.priority === undefined ? {} : { priority: commandInput.priority }),
    updatedAtMs: Date.now(),
  };
  demoAccounts[index] = updated;
  return structuredClone(updated);
}

export async function deleteAccount(id: string): Promise<void> {
  if (inTauri()) {
    await invoke('delete_account', { id });
    return;
  }
  demoAccounts = demoAccounts.filter(account => account.id !== id);
}

export async function detectTools(): Promise<ToolDetection[]> {
  return inTauri() ? invoke<ToolDetection[]>('detect_tools') : demoTools;
}

export async function installTool(kind: ToolKind): Promise<ToolDetection> {
  if (inTauri()) return invoke<ToolDetection>('install_tool', { kind });
  const detection = demoTools.find(tool => tool.kind === kind);
  if (!detection) throw new Error('未知工具');
  return { ...detection, installed: true, detail: '已就绪' };
}

export async function cancelToolInstall(kind: ToolKind): Promise<boolean> {
  return inTauri() ? invoke<boolean>('cancel_tool_install', { kind }) : false;
}

export async function checkAppUpdate(): Promise<AppUpdateInfo | null> {
  return inTauri() ? invoke<AppUpdateInfo | null>('check_app_update') : null;
}

export async function checkSignedAppUpdate(): Promise<SignedAppUpdateInfo | null> {
  return inTauri() ? invoke<SignedAppUpdateInfo | null>('check_signed_app_update') : null;
}

export async function downloadAppUpdate(
  onProgress: (progress: AppUpdateDownloadProgress) => void
): Promise<AppUpdateDownloadResult> {
  if (!inTauri()) throw new Error('应用更新仅在桌面应用中可用');
  const progress = new Channel<AppUpdateDownloadProgress>(onProgress);
  return invoke<AppUpdateDownloadResult>('download_app_update', { progress });
}

export async function installAppUpdate(): Promise<void> {
  if (!inTauri()) throw new Error('应用更新仅在桌面应用中可用');
  await invoke('install_app_update');
}

export async function restartAfterAppUpdate(): Promise<void> {
  if (!inTauri()) throw new Error('应用更新仅在桌面应用中可用');
  await invoke('restart_after_app_update');
}

export async function openReleasesPage(): Promise<void> {
  if (inTauri()) await invoke('open_releases_page');
}

const TOOL_INSTALL_PROGRESS_EVENT = 'trusted-carpool:tool-install-progress';

export async function listenForToolInstallProgress(
  onProgress: (progress: ToolInstallProgress) => void
): Promise<UnlistenFn> {
  if (!inTauri()) return () => undefined;
  return listen<ToolInstallProgress>(TOOL_INSTALL_PROGRESS_EVENT, event =>
    onProgress(event.payload)
  );
}

export async function startCar(input: {
  carName: string;
  enabledTools: ToolKind[];
  startsAt: number;
  endsAt: number;
}): Promise<CarSession> {
  if (inTauri()) return invoke<CarSession>('start_car', { input });
  demoActiveCar = demoCar(input.enabledTools, input.carName, input.startsAt, input.endsAt);
  return demoActiveCar;
}

export async function stopCar(): Promise<void> {
  if (inTauri()) await invoke('stop_car');
  else demoActiveCar = null;
}

export async function getActiveCar(): Promise<CarSession | null> {
  return inTauri() ? invoke<CarSession | null>('get_active_car') : demoActiveCar;
}

export async function refreshAccountQuotas(): Promise<CarSession['accountQuotas']> {
  if (inTauri()) return invoke<CarSession['accountQuotas']>('refresh_account_quotas');
  return demoActiveCar?.accountQuotas ?? [];
}

export async function updateMemberTokenLimits(input: {
  seatNo: number;
  fiveHourTokens: number | null;
  dailyTokens: number | null;
  weeklyTokens: number | null;
}): Promise<Seat> {
  if (inTauri()) return invoke<Seat>('update_member_token_limits', { input });
  if (!demoActiveCar) throw new Error('当前没有正在发车的车队');
  const seat = demoActiveCar.seats.find(item => item.seatNo === input.seatNo);
  if (!seat) throw new Error('成员座位不存在');
  seat.tokenLimits = {
    fiveHourTokens: input.fiveHourTokens,
    dailyTokens: input.dailyTokens,
    weeklyTokens: input.weeklyTokens,
  };
  const updateWindow = (key: keyof MemberTokenLimits, windowKey: 'fiveHour' | 'daily' | 'weekly') => {
    const limitTokens = seat.tokenLimits[key];
    const usedTokens = seat.tokenLimitStatus[windowKey].usedTokens;
    seat.tokenLimitStatus[windowKey] = {
      ...seat.tokenLimitStatus[windowKey],
      limitTokens,
      remainingTokens: limitTokens === null ? null : Math.max(0, limitTokens - usedTokens),
      exhausted: limitTokens !== null && usedTokens >= limitTokens,
    };
  };
  updateWindow('fiveHourTokens', 'fiveHour');
  updateWindow('dailyTokens', 'daily');
  updateWindow('weeklyTokens', 'weekly');
  return structuredClone(seat);
}

export async function previewInvite(code: string): Promise<JoinPreview> {
  if (inTauri()) return invoke<JoinPreview>('preview_invite', { code });
  if (code.trim().length < 12) throw new Error('上车码应为 12 位');
  return {
    carId: 'demo-car',
    carName: '我的高效车队',
    ownerLabel: '阿杰',
    seatNo: 3,
    enabledTools: ['claude', 'codex'],
    startsAt: Date.now() - 60_000,
    expiresAt: Date.now() + 60 * 60 * 1000,
  };
}

export async function joinCar(code: string, nickname: string): Promise<RideAccess> {
  if (inTauri()) return invoke<RideAccess>('join_car', { code, nickname });
  return {
    ...(await previewInvite(code)),
    accessId: crypto.randomUUID(),
    ownerPeerId: 'p2p-demo-owner',
    localProxyPort: 25342,
    connectionState: 'connected',
  };
}

export async function launchTool(input: {
  kind: ToolKind;
  mode: LaunchMode;
  accessId: string;
  workDir?: string;
}): Promise<ToolLaunchResult> {
  if (inTauri()) return invoke<ToolLaunchResult>('launch_tool', { input });
  return {
    instanceId: `${input.mode}-${crypto.randomUUID()}`,
    status: 'ready',
    reused: false,
    readyAtMs: Date.now(),
  };
}

export async function listClientInstances(): Promise<ClientInstanceSummary[]> {
  return inTauri() ? invoke<ClientInstanceSummary[]>('list_client_instances') : [];
}

export async function focusClientInstance(instanceId: string): Promise<void> {
  if (inTauri()) await invoke('focus_client_instance', { instanceId });
}

export async function closeClientInstance(instanceId: string): Promise<boolean> {
  return inTauri() ? invoke<boolean>('close_client_instance', { instanceId }) : true;
}

export async function leaveCar(accessId: string): Promise<void> {
  if (inTauri()) await invoke('leave_car', { accessId });
}
