import { invoke } from '@tauri-apps/api/core';
import type {
  CarSession,
  JoinPreview,
  MemberTokenLimits,
  ModelUsageSummary,
  RideAccess,
  Seat,
  SeatUsageSummary,
  ToolDetection,
  ToolKind,
} from './types';

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;

const demoTools: ToolDetection[] = [
  {
    kind: 'claude',
    name: 'Claude Code',
    installed: true,
    authenticated: true,
    executablePath: '/usr/local/bin/claude',
    configPath: '~/.claude',
    detail: '已就绪',
  },
  {
    kind: 'codex',
    name: 'Codex',
    installed: true,
    authenticated: true,
    executablePath: '/usr/local/bin/codex',
    configPath: '~/.codex/auth.json',
    detail: '已就绪',
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

export async function detectTools(): Promise<ToolDetection[]> {
  return inTauri() ? invoke<ToolDetection[]>('detect_tools') : demoTools;
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
  accessId: string;
  workDir?: string;
}): Promise<void> {
  if (inTauri()) await invoke('launch_tool', { input });
}

export async function leaveCar(accessId: string): Promise<void> {
  if (inTauri()) await invoke('leave_car', { accessId });
}
