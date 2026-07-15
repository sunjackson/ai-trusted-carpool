import {
  ArrowLeft,
  CarFront,
  Check,
  CheckCircle2,
  ChevronDown,
  CircleHelp,
  Clock3,
  Code2,
  Copy,
  Gauge,
  LogOut,
  MonitorUp,
  RefreshCw,
  ShieldCheck,
  SlidersHorizontal,
  Sparkles,
  SquareTerminal,
  Users,
  Wifi,
  X,
} from 'lucide-react';
import { useEffect, useMemo, useState } from 'react';
import {
  detectTools,
  getActiveCar,
  joinCar,
  launchTool,
  leaveCar,
  previewInvite,
  refreshAccountQuotas,
  startCar,
  stopCar,
  updateMemberTokenLimits,
} from './api';
import type {
  AccountQuotaSnapshot,
  CarSession,
  JoinPreview,
  MemberTokenLimitStatus,
  RideAccess,
  Seat,
  SeatUsageSummary,
  SharedCarStatus,
  ToolDetection,
  ToolKind,
  LaunchMode,
} from './types';
import { trustedWebRtc } from './trustedWebRtc';

type Screen = 'welcome' | 'host-setup' | 'host-live' | 'join' | 'ready' | 'ride';

const TOOL_LABEL: Record<ToolKind, string> = { claude: 'Claude', codex: 'Codex' };
type LaunchTarget = { kind: ToolKind; mode: LaunchMode };
const formatInviteCode = (code: string): string => code.match(/.{1,4}/g)?.join('-') ?? code;
const toDateTimeInput = (timestamp: number): string => {
  const date = new Date(timestamp);
  const local = new Date(date.getTime() - date.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 16);
};
const formatDateTime = (timestamp: number): string =>
  new Intl.DateTimeFormat('zh-CN', {
    month: 'numeric',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  }).format(timestamp);
const roundUpToHalfHour = (timestamp: number): number =>
  Math.ceil(timestamp / (30 * 60_000)) * 30 * 60_000;
const durationOptions = [1, 2, 4, 8] as const;
type DurationHours = (typeof durationOptions)[number];
const formatTokens = (value: number): string =>
  value >= 10_000 ? `${(value / 10_000).toFixed(1)}万` : value.toLocaleString('zh-CN');
const formatOfficialCost = (microUsd: number): string =>
  new Intl.NumberFormat('en-US', {
    style: 'currency',
    currency: 'USD',
    minimumFractionDigits: 2,
    maximumFractionDigits: 6,
  }).format(microUsd / 1_000_000);
const initialScreen = (): Screen => {
  if (!import.meta.env.DEV) return 'welcome';
  const candidate = new URLSearchParams(window.location.search).get('screen');
  return candidate === 'host-setup' || candidate === 'join' ? candidate : 'welcome';
};

function ToolMark({ kind }: { kind: ToolKind }) {
  return (
    <span className={`tool-mark tool-mark--${kind}`} aria-hidden="true">
      {kind === 'claude' ? <Sparkles size={27} /> : <Code2 size={27} />}
    </span>
  );
}

function Brand() {
  return (
    <div className="brand" aria-label="可信拼车">
      <span className="brand__mark">
        <CarFront size={24} strokeWidth={2.4} />
      </span>
      <span className="brand__name">可信拼车</span>
      <span className="brand__tagline">共享算力，不共享密钥</span>
    </div>
  );
}

function WindowShell({ children, onHome }: { children: React.ReactNode; onHome: () => void }) {
  return (
    <main className="app-shell">
      <div className="ambient ambient--one" />
      <div className="ambient ambient--two" />
      <header className="titlebar" data-tauri-drag-region>
        <button className="brand-button" onClick={onHome} aria-label="返回首页">
          <Brand />
        </button>
        <div className="titlebar__right">
          <span className="official-pill">
            <ShieldCheck size={14} /> 只连官方地址
          </span>
        </div>
      </header>
      <div className="content-frame">{children}</div>
    </main>
  );
}

function BackButton({ onClick, label = '返回' }: { onClick: () => void; label?: string }) {
  return (
    <button className="back-button" onClick={onClick}>
      <ArrowLeft size={18} /> {label}
    </button>
  );
}

function ErrorBanner({ message, onClose }: { message: string; onClose: () => void }) {
  return (
    <div className="error-banner" role="alert">
      <CircleHelp size={18} />
      <span>{message}</span>
      <button onClick={onClose} aria-label="关闭提示">
        <X size={16} />
      </button>
    </div>
  );
}

function Welcome({ onHost, onJoin }: { onHost: () => void; onJoin: () => void }) {
  return (
    <section className="welcome page-enter">
      <div className="welcome__hero">
        <div className="hero-orbit">
          <span className="hero-orbit__ring" />
          <span className="hero-orbit__icon">
            <CarFront size={44} />
          </span>
        </div>
        <p className="eyebrow">TRUSTED CARPOOL</p>
        <h1>一起用，密钥仍只在你的电脑里</h1>
        <p className="welcome__lead">发车的人保持应用在线，上车的人点一次就能打开工具。</p>
      </div>

      <div className="role-grid">
        <button className="role-card role-card--host" onClick={onHost}>
          <span className="role-card__icon">
            <CarFront size={34} />
          </span>
          <span className="role-card__copy">
            <strong>我要发车</strong>
            <small>选本机账号，分享四个上车码</small>
          </span>
          <span className="role-card__arrow">→</span>
        </button>
        <button className="role-card" onClick={onJoin}>
          <span className="role-card__icon">
            <Users size={34} />
          </span>
          <span className="role-card__copy">
            <strong>我要上车</strong>
            <small>输入上车码，立即打开工具</small>
          </span>
          <span className="role-card__arrow">→</span>
        </button>
      </div>

      <div className="trust-row">
        <span><ShieldCheck size={16} /> 本机数据</span>
        <span><Wifi size={16} /> 自动连接</span>
        <span><LogOut size={16} /> 随时退出</span>
      </div>
    </section>
  );
}

function HostSetup({
  tools,
  loadingTools,
  onRefresh,
  onBack,
  onStarted,
  onError,
}: {
  tools: ToolDetection[];
  loadingTools: boolean;
  onRefresh: () => void;
  onBack: () => void;
  onStarted: (car: CarSession) => void;
  onError: (message: string) => void;
}) {
  const [selected, setSelected] = useState<ToolKind[]>(() =>
    tools.filter(tool => tool.installed && tool.authenticated).map(tool => tool.kind)
  );
  const [carName, setCarName] = useState('我的高效车队');
  const [startMode, setStartMode] = useState<'now' | 'scheduled'>('now');
  const [scheduledAt, setScheduledAt] = useState(() =>
    toDateTimeInput(roundUpToHalfHour(Date.now() + 15 * 60_000))
  );
  const [durationHours, setDurationHours] = useState<DurationHours | 'custom'>(2);
  const [customEndsAt, setCustomEndsAt] = useState(() =>
    toDateTimeInput(roundUpToHalfHour(Date.now() + 15 * 60_000) + 2 * 60 * 60_000)
  );
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setSelected(current =>
      current.length > 0
        ? current
        : tools.filter(tool => tool.installed && tool.authenticated).map(tool => tool.kind)
    );
  }, [tools]);

  const toggleTool = (kind: ToolKind, enabled: boolean) => {
    if (!enabled) return;
    setSelected(current =>
      current.includes(kind) ? current.filter(item => item !== kind) : [...current, kind]
    );
  };

  const previewStart =
    startMode === 'now' ? Date.now() : new Date(scheduledAt).getTime();
  const previewEnd =
    durationHours === 'custom'
      ? new Date(customEndsAt).getTime()
      : previewStart + durationHours * 60 * 60_000;
  const hasValidPreview =
    Number.isFinite(previewStart) && Number.isFinite(previewEnd) && previewEnd > previewStart;

  const chooseDuration = (duration: DurationHours | 'custom') => {
    setDurationHours(duration);
    if (duration !== 'custom') return;
    const currentEnd = new Date(customEndsAt).getTime();
    if (!Number.isFinite(currentEnd) || currentEnd <= previewStart + 15 * 60_000) {
      setCustomEndsAt(toDateTimeInput(previewStart + 2 * 60 * 60_000));
    }
  };

  const updateScheduledAt = (value: string) => {
    setScheduledAt(value);
    if (durationHours !== 'custom') return;
    const nextStart = new Date(value).getTime();
    const currentEnd = new Date(customEndsAt).getTime();
    if (Number.isFinite(nextStart) && currentEnd <= nextStart + 15 * 60_000) {
      setCustomEndsAt(toDateTimeInput(nextStart + 2 * 60 * 60_000));
    }
  };

  const submit = async () => {
    if (selected.length === 0) {
      onError('至少选择一个已就绪的工具');
      return;
    }
    const startTimestamp =
      startMode === 'now' ? Date.now() : new Date(scheduledAt).getTime();
    const endTimestamp =
      durationHours === 'custom'
        ? new Date(customEndsAt).getTime()
        : startTimestamp + durationHours * 60 * 60_000;
    if (!Number.isFinite(startTimestamp) || !Number.isFinite(endTimestamp) || endTimestamp <= startTimestamp) {
      onError('请选择正确的开始和结束时间');
      return;
    }
    setBusy(true);
    try {
      const nextCar = await startCar({
        carName: carName.trim() || '我的车队',
        enabledTools: selected,
        startsAt: startTimestamp,
        endsAt: endTimestamp,
      });
      try {
        await trustedWebRtc.startHost();
      } catch (error) {
        await stopCar().catch(() => undefined);
        throw error;
      }
      onStarted(nextCar);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="flow-page page-enter">
      <BackButton onClick={onBack} />
      <div className="flow-heading">
        <p className="eyebrow">一步发车</p>
        <h1>选择要共享的工具</h1>
        <p>只读取本机登录状态，不上传账号配置或密钥。</p>
      </div>

      <div className="stepper" aria-label="发车步骤">
        <span className="stepper__item stepper__item--active"><b>1</b> 选择工具</span>
        <i />
        <span className="stepper__item"><b>2</b> 设置车队</span>
        <i />
        <span className="stepper__item"><b>3</b> 开始发车</span>
      </div>

      <div className="tool-detection-header">
        <span>自动检测到以下可用工具</span>
        <button onClick={onRefresh} disabled={loadingTools}>
          <RefreshCw size={15} className={loadingTools ? 'spin' : ''} /> 重新检测
        </button>
      </div>
      <div className="tool-grid">
        {tools.map(tool => {
          const enabled = tool.installed && tool.authenticated;
          const checked = selected.includes(tool.kind);
          return (
            <button
              className={`tool-card ${checked ? 'tool-card--selected' : ''}`}
              key={tool.kind}
              onClick={() => toggleTool(tool.kind, enabled)}
              disabled={!enabled}
            >
              <ToolMark kind={tool.kind} />
              <span className="tool-card__body">
                <strong>{tool.name}</strong>
                <small className={enabled ? 'status-ok' : 'status-off'}>
                  <span /> {enabled ? '已就绪' : tool.detail}
                </small>
              </span>
              <span className={`check-box ${checked ? 'check-box--on' : ''}`}>
                {checked && <Check size={15} />}
              </span>
            </button>
          );
        })}
      </div>

      <div className="setup-stack">
        <label className="setup-field">
          <span>车队名称</span>
          <input value={carName} onChange={event => setCarName(event.target.value)} maxLength={32} />
        </label>
        <div className="schedule-card">
          <div className="schedule-card__heading">
            <span className="schedule-card__icon"><Clock3 size={18} /></span>
            <div>
              <strong>发车时间</strong>
              <small>先选什么时候开始，再选共享多久</small>
            </div>
          </div>

          <div className="schedule-row">
            <span className="schedule-row__label">什么时候开始</span>
            <div className="segmented-control" role="group" aria-label="什么时候开始">
              <button
                type="button"
                className={startMode === 'now' ? 'is-active' : ''}
                aria-pressed={startMode === 'now'}
                onClick={() => setStartMode('now')}
              >
                立即开始
              </button>
              <button
                type="button"
                className={startMode === 'scheduled' ? 'is-active' : ''}
                aria-pressed={startMode === 'scheduled'}
                onClick={() => setStartMode('scheduled')}
              >
                预约开始
              </button>
            </div>
          </div>

          {startMode === 'scheduled' && (
            <label className="schedule-datetime schedule-datetime--start">
              <span>预约开始时间</span>
              <input
                aria-label="预约开始时间"
                type="datetime-local"
                min={toDateTimeInput(Date.now())}
                max={toDateTimeInput(Date.now() + 30 * 24 * 60 * 60_000)}
                value={scheduledAt}
                onChange={event => updateScheduledAt(event.target.value)}
              />
            </label>
          )}

          <div className="schedule-row schedule-row--duration">
            <span className="schedule-row__label">共享多久</span>
            <div className="duration-options" role="group" aria-label="共享时长">
              {durationOptions.map(duration => (
                <button
                  type="button"
                  key={duration}
                  className={durationHours === duration ? 'is-active' : ''}
                  aria-pressed={durationHours === duration}
                  onClick={() => chooseDuration(duration)}
                >
                  {duration} 小时
                  {duration === 2 && <small>推荐</small>}
                </button>
              ))}
              <button
                type="button"
                className={durationHours === 'custom' ? 'is-active' : ''}
                aria-pressed={durationHours === 'custom'}
                onClick={() => chooseDuration('custom')}
              >
                自定义
              </button>
            </div>
          </div>

          {durationHours === 'custom' && (
            <label className="schedule-datetime schedule-datetime--end">
              <span>自定义结束时间</span>
              <input
                aria-label="自定义结束时间"
                type="datetime-local"
                min={toDateTimeInput(previewStart + 15 * 60_000)}
                max={toDateTimeInput(previewStart + 24 * 60 * 60_000)}
                value={customEndsAt}
                onChange={event => setCustomEndsAt(event.target.value)}
              />
            </label>
          )}

          <div className={`schedule-summary ${hasValidPreview ? '' : 'schedule-summary--invalid'}`}>
            <CheckCircle2 size={17} />
            <div>
              <strong>
                {startMode === 'now' ? '发车后立即可上车' : `${formatDateTime(previewStart)} 开放上车`}
              </strong>
              <small>
                {hasValidPreview
                  ? `${formatDateTime(previewEnd)} 自动结束${durationHours === 'custom' ? '' : ` · 共 ${durationHours} 小时`}`
                  : '请补充正确的时间范围'}
              </small>
            </div>
          </div>
        </div>
      </div>

      <button className="primary-button" onClick={submit} disabled={busy || selected.length === 0}>
        {busy ? <><RefreshCw className="spin" size={19} /> 正在准备...</> : <><CarFront size={20} /> 开始发车</>}
      </button>
      <p className="quiet-note"><ShieldCheck size={15} /> 最多 4 人同时使用，你的密钥不会离开电脑</p>
    </section>
  );
}

const windowLabel: Record<keyof MemberTokenLimitStatus, string> = {
  fiveHour: '5 小时',
  daily: '24 小时',
  weekly: '7 天',
};

const MAX_MEMBER_TOKEN_LIMIT = 1_000_000_000_000;

function AccountQuotaPanel({
  quotas,
  onRefresh,
  refreshing = false,
}: {
  quotas: AccountQuotaSnapshot[];
  onRefresh?: () => void;
  refreshing?: boolean;
}) {
  return (
    <section className="account-quota-panel" aria-label="车队账号额度">
      <div className="account-quota-panel__header">
        <div>
          <span><Gauge size={17} /> 车队账号额度</span>
          <small>读取车主本机官方 OAuth 套餐，车主和所有成员看到相同结果</small>
        </div>
        {onRefresh && (
          <button onClick={onRefresh} disabled={refreshing}>
            <RefreshCw size={14} className={refreshing ? 'spin' : ''} /> 刷新
          </button>
        )}
      </div>
      <div className="account-quota-grid">
        {quotas.map(quota => (
          <article className="account-quota-card" key={quota.tool}>
            <div className="account-quota-card__title">
              <strong>{TOOL_LABEL[quota.tool]}</strong>
              <span>{quota.planName || (quota.state === 'available' ? '官方额度' : '额度状态')}</span>
            </div>
            {quota.windows.length > 0 ? (
              <>
                <div className="quota-window-list">
                  {quota.windows.map(item => (
                    <div className="quota-window" key={item.label}>
                      <div><span>{item.label}</span><strong>剩余 {item.remainingPercent.toFixed(0)}%</strong></div>
                      <div className="quota-progress" aria-label={`${TOOL_LABEL[quota.tool]} ${item.label} 剩余 ${item.remainingPercent.toFixed(0)}%`}>
                        <i style={{ width: `${item.remainingPercent}%` }} />
                      </div>
                      <small>{item.resetsAt ? `${formatDateTime(item.resetsAt)} 重置` : '重置时间以官方为准'}</small>
                    </div>
                  ))}
                </div>
                {quota.message && <p className="quota-stale-message">{quota.message}</p>}
              </>
            ) : (
              <p className={`quota-message quota-message--${quota.state}`}>
                {quota.message || '暂时没有可用的官方额度数据'}
              </p>
            )}
          </article>
        ))}
      </div>
    </section>
  );
}

function MemberLimitBars({ status }: { status: MemberTokenLimitStatus }) {
  return (
    <div className="member-limit-bars">
      {(Object.keys(windowLabel) as (keyof MemberTokenLimitStatus)[]).map(key => {
        const item = status[key];
        const usedPercent = item.limitTokens
          ? Math.min(100, (item.usedTokens / item.limitTokens) * 100)
          : 0;
        return (
          <div className="member-limit-row" key={key}>
            <span>{windowLabel[key]}</span>
            <div className="member-limit-track">
              <i className={item.exhausted ? 'is-exhausted' : ''} style={{ width: item.limitTokens ? `${usedPercent}%` : '0%' }} />
            </div>
            <strong>
              {item.limitTokens === null
                ? '不限额'
                : `${formatTokens(item.remainingTokens ?? 0)} 剩余`}
            </strong>
            <small>{item.resetsAt ? `${formatDateTime(item.resetsAt)} 重置` : '—'}</small>
          </div>
        );
      })}
    </div>
  );
}

function MemberUsageDetails({ usage }: { usage: SeatUsageSummary }) {
  return (
    <>
      <div className="usage-overview" aria-label="成员用量汇总">
        <span><small>输入</small><strong>{formatTokens(usage.inputTokens)}</strong></span>
        <span><small>输出</small><strong>{formatTokens(usage.outputTokens)}</strong></span>
        <span><small>缓存读</small><strong>{formatTokens(usage.cacheReadTokens)}</strong></span>
        <span><small>缓存写</small><strong>{formatTokens(usage.cacheWriteTokens)}</strong></span>
      </div>
      <div className="model-usage-list">
        {usage.models.length === 0 && <div className="empty-usage">还没有模型调用记录</div>}
        {usage.models.map(model => (
          <div className="model-usage" key={`${model.tool}:${model.model}`}>
            <div className="model-usage__title">
              <span>{TOOL_LABEL[model.tool]} · {model.requestCount} 次</span>
              <strong>{model.model}</strong>
              <em title={model.pricingSource ?? undefined}>
                {model.officialCostMicrousd === null
                  ? '暂无官价'
                  : `官价估算 ${formatOfficialCost(model.officialCostMicrousd)}`}
              </em>
            </div>
            <div className="model-usage__tokens">
              <span>输入 {formatTokens(model.inputTokens)}</span>
              <span>输出 {formatTokens(model.outputTokens)}</span>
              <span>缓存读 {formatTokens(model.cacheReadTokens)}</span>
              <span>缓存写 {formatTokens(model.cacheWriteTokens)}</span>
            </div>
            {model.tool === 'claude' && (
              <small>
                缓存写入：5 分钟 {formatTokens(model.cacheWrite5mTokens)} · 1 小时 {formatTokens(model.cacheWrite1hTokens)}
              </small>
            )}
            {model.unpricedRequestCount > 0 && (
              <small>{model.unpricedRequestCount} 次请求没有可用官方价，未计入估算</small>
            )}
          </div>
        ))}
      </div>
    </>
  );
}

function MemberDetailDialog({
  seat,
  onClose,
  onSave,
  saving,
  editable = true,
}: {
  seat: Seat;
  onClose: () => void;
  onSave?: (limits: { fiveHourTokens: number | null; dailyTokens: number | null; weeklyTokens: number | null }) => void;
  saving?: boolean;
  editable?: boolean;
}) {
  const [fiveHour, setFiveHour] = useState(seat.tokenLimits.fiveHourTokens?.toString() ?? '');
  const [daily, setDaily] = useState(seat.tokenLimits.dailyTokens?.toString() ?? '');
  const [weekly, setWeekly] = useState(seat.tokenLimits.weeklyTokens?.toString() ?? '');

  useEffect(() => {
    setFiveHour(seat.tokenLimits.fiveHourTokens?.toString() ?? '');
    setDaily(seat.tokenLimits.dailyTokens?.toString() ?? '');
    setWeekly(seat.tokenLimits.weeklyTokens?.toString() ?? '');
  }, [
    seat.seatNo,
    seat.tokenLimits.fiveHourTokens,
    seat.tokenLimits.dailyTokens,
    seat.tokenLimits.weeklyTokens,
  ]);

  const parse = (value: string): number | null | undefined => {
    if (!value.trim()) return null;
    const parsed = Number(value);
    return Number.isSafeInteger(parsed) && parsed > 0 && parsed <= MAX_MEMBER_TOKEN_LIMIT
      ? parsed
      : undefined;
  };

  const parsedLimits = {
    fiveHourTokens: parse(fiveHour),
    dailyTokens: parse(daily),
    weeklyTokens: parse(weekly),
  };
  const hasInvalidLimit = Object.values(parsedLimits).some(value => value === undefined);

  return (
    <div className="dialog-backdrop" role="presentation" onMouseDown={event => event.target === event.currentTarget && onClose()}>
      <section className="member-dialog" role="dialog" aria-modal="true" aria-label={`${seat.nickname ?? '成员'}使用详情`}>
        <header className="member-dialog__header">
          <div className="avatar">{seat.nickname?.slice(0, 1) ?? seat.seatNo}</div>
          <div>
            <small>成员 {seat.seatNo}</small>
            <h2>{seat.nickname ?? '未命名成员'}</h2>
            <span>{seat.tool ? TOOL_LABEL[seat.tool] : '尚未选择工具'} · {seat.usage.requestCount} 次请求 · {formatTokens(seat.usage.totalTokens)} Token</span>
          </div>
          <button className="dialog-close" onClick={onClose} aria-label="关闭成员详情"><X size={18} /></button>
        </header>

        <div className="dialog-section">
          <div className="dialog-section__title"><strong>实时使用明细</strong><span>按工具和模型统计</span></div>
          <MemberUsageDetails usage={seat.usage} />
        </div>

        <div className="dialog-section">
          <div className="dialog-section__title"><strong>成员 Token 限额</strong><span>达到任一窗口限额后停止新请求</span></div>
          <MemberLimitBars status={seat.tokenLimitStatus} />
          {editable && (
            <div className="limit-editor">
              <label><span>5 小时</span><input aria-label="5 小时 Token 限额" type="number" min="1" max={MAX_MEMBER_TOKEN_LIMIT} value={fiveHour} onChange={event => setFiveHour(event.target.value)} placeholder="不限额" /><small>Token</small></label>
              <label><span>24 小时</span><input aria-label="24 小时 Token 限额" type="number" min="1" max={MAX_MEMBER_TOKEN_LIMIT} value={daily} onChange={event => setDaily(event.target.value)} placeholder="不限额" /><small>Token</small></label>
              <label><span>7 天</span><input aria-label="7 天 Token 限额" type="number" min="1" max={MAX_MEMBER_TOKEN_LIMIT} value={weekly} onChange={event => setWeekly(event.target.value)} placeholder="不限额" /><small>Token</small></label>
              {hasInvalidLimit && (
                <p className="limit-editor__error" role="alert">
                  限额必须是 1—1万亿之间的整数，留空表示不限额
                </p>
              )}
              <button
                onClick={() => {
                  if (hasInvalidLimit) return;
                  onSave?.(parsedLimits as {
                    fiveHourTokens: number | null;
                    dailyTokens: number | null;
                    weeklyTokens: number | null;
                  });
                }}
                disabled={saving || hasInvalidLimit}
              >
                {saving ? '保存中...' : '保存成员限额'}
              </button>
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

function HostLive({ car, onStopped, onError }: { car: CarSession; onStopped: () => void; onError: (message: string) => void }) {
  const [liveCar, setLiveCar] = useState(car);
  const [clock, setClock] = useState(Date.now());
  const [copied, setCopied] = useState<string | null>(null);
  const [selectedSeatNo, setSelectedSeatNo] = useState<number | null>(null);
  const [quotaRefreshing, setQuotaRefreshing] = useState(false);
  const [limitSaving, setLimitSaving] = useState(false);

  useEffect(() => {
    const timer = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    const refresh = () => {
      void getActiveCar()
        .then(current => {
          if (current?.carId === car.carId) setLiveCar(current);
        })
        .catch(error => onError(error instanceof Error ? error.message : String(error)));
    };
    refresh();
    const timer = window.setInterval(refresh, 1000);
    return () => window.clearInterval(timer);
  }, [car.carId, onError]);

  const scheduled = clock < liveCar.startedAt;
  const seconds = Math.max(0, Math.floor(Math.abs(clock - liveCar.startedAt) / 1000));
  const elapsed = `${String(Math.floor(seconds / 3600)).padStart(2, '0')}:${String(Math.floor((seconds % 3600) / 60)).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`;

  const copy = async (value: string, label: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(label);
      window.setTimeout(() => setCopied(null), 1400);
    } catch {
      onError('复制失败，请手动记录上车码');
    }
  };

  const stop = async () => {
    try {
      await trustedWebRtc.stop();
      await stopCar();
      onStopped();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    }
  };

  const refreshQuotas = async () => {
    setQuotaRefreshing(true);
    try {
      const accountQuotas = await refreshAccountQuotas();
      setLiveCar(current => ({ ...current, accountQuotas }));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setQuotaRefreshing(false);
    }
  };

  const saveLimits = async (limits: { fiveHourTokens: number | null; dailyTokens: number | null; weeklyTokens: number | null }) => {
    if (selectedSeatNo === null) return;
    setLimitSaving(true);
    try {
      const updated = await updateMemberTokenLimits({ seatNo: selectedSeatNo, ...limits });
      setLiveCar(current => ({
        ...current,
        seats: current.seats.map(seat => seat.seatNo === updated.seatNo ? updated : seat),
      }));
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLimitSaving(false);
    }
  };

  const selectedSeat = selectedSeatNo === null
    ? null
    : liveCar.seats.find(seat => seat.seatNo === selectedSeatNo) ?? null;

  return (
    <section className="live-page page-enter">
      <div className="live-header">
        <div>
          <div className="live-title"><span className="pulse-dot" /> {scheduled ? '等待发车' : '正在发车'}</div>
          <p>{liveCar.carName} · {formatDateTime(liveCar.startedAt)}—{formatDateTime(liveCar.expiresAt)}</p>
        </div>
        <div className="live-header__actions">
          <span className="timer"><Clock3 size={17} /> {scheduled ? `距开始 ${elapsed}` : elapsed}</span>
          <button className="danger-button" onClick={stop}>停止发车</button>
        </div>
      </div>

      <AccountQuotaPanel quotas={liveCar.accountQuotas} onRefresh={refreshQuotas} refreshing={quotaRefreshing} />

      <div className="seat-grid">
        {liveCar.seats.map(seat => (
          <article className={`seat-card seat-card--${seat.state}`} key={seat.seatNo}>
            <span className="seat-number">{seat.seatNo}</span>
            {seat.nickname ? (
              <button
                className="member-summary"
                onClick={() => setSelectedSeatNo(seat.seatNo)}
                aria-label={`查看${seat.nickname}详情`}
              >
                <div className="seat-person">
                  <div className="avatar">{seat.nickname.slice(0, 1)}</div>
                  <div>
                    <strong>{seat.nickname}</strong>
                    <span className="seat-state"><i /> {seat.state === 'using' ? '使用中' : '已连接'}{seat.tool ? ` · ${TOOL_LABEL[seat.tool]}` : ''}</span>
                  </div>
                  <div className="seat-total">
                    <strong>{formatTokens(seat.usage.totalTokens)} Token</strong>
                    <small>{seat.usage.requestCount} 次请求</small>
                  </div>
                </div>
                <div className="member-summary__bottom">
                  <span>
                    {seat.usage.unpricedRequestCount === seat.usage.requestCount && seat.usage.requestCount > 0
                      ? '暂无官价'
                      : `官价估算 ${formatOfficialCost(seat.usage.officialCostMicrousd)}`}
                  </span>
                  <span className={seat.tokenLimitStatus.fiveHour.exhausted ? 'limit-exhausted' : ''}>
                    {seat.tokenLimitStatus.fiveHour.limitTokens === null
                      ? '5 小时不限额'
                      : `5 小时剩余 ${formatTokens(seat.tokenLimitStatus.fiveHour.remainingTokens ?? 0)}`}
                  </span>
                  <strong><SlidersHorizontal size={13} /> 查看详情与限额</strong>
                </div>
              </button>
            ) : (
              <>
                <div className="avatar avatar--empty"><Users size={25} /></div>
                <strong>空座位</strong>
                <span className="seat-state seat-state--waiting">等待上车</span>
                <button className="invite-code" onClick={() => copy(seat.code, seat.code)}>
                  {formatInviteCode(seat.code)} <Copy size={14} />
                </button>
              </>
            )}
          </article>
        ))}
      </div>

      <button className="primary-button primary-button--compact" onClick={() => copy(liveCar.seats.map(seat => formatInviteCode(seat.code)).join('\n'), 'all')}>
        {copied === 'all' ? <><Check size={19} /> 已复制全部上车码</> : <><Copy size={19} /> 复制全部上车码</>}
      </button>
      {copied && copied !== 'all' && <div className="toast-inline">上车码 {formatInviteCode(copied)} 已复制</div>}

      <div className="live-notice">
        <ShieldCheck size={18} />
        <span><strong>熟人共享</strong> · 按人、按模型实时统计输入、输出与缓存，明细仅保存在车主本机；官价为官方 API 标准价估算，不是账单。</span>
      </div>

      {selectedSeat && (
        <MemberDetailDialog
          seat={selectedSeat}
          onClose={() => setSelectedSeatNo(null)}
          onSave={saveLimits}
          saving={limitSaving}
        />
      )}

    </section>
  );
}

function JoinPage({ onBack, onJoined, onError }: { onBack: () => void; onJoined: (access: RideAccess) => void; onError: (message: string) => void }) {
  const [code, setCode] = useState('');
  const [nickname, setNickname] = useState('');
  const [preview, setPreview] = useState<JoinPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [clock, setClock] = useState(Date.now());

  const normalizedCode = code.toUpperCase().replace(/[^A-Z0-9]/g, '').slice(0, 12);

  useEffect(() => {
    if (normalizedCode.length !== 12) {
      setPreview(null);
      return;
    }
    let active = true;
    previewInvite(normalizedCode)
      .then(result => active && setPreview(result))
      .catch(error => active && onError(error instanceof Error ? error.message : String(error)));
    return () => { active = false; };
  }, [normalizedCode, onError]);

  useEffect(() => {
    const timer = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  const waitingForStart = Boolean(preview && preview.startsAt > clock);

  const submit = async () => {
    if (!preview) return;
    if (!nickname.trim()) {
      onError('请输入一个昵称，车主才能识别你');
      return;
    }
    setBusy(true);
    try {
      const nextAccess = await joinCar(normalizedCode, nickname.trim());
      try {
        await trustedWebRtc.startPassenger(nextAccess);
      } catch (error) {
        await leaveCar(nextAccess.accessId).catch(() => undefined);
        throw error;
      }
      onJoined(nextAccess);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="join-page flow-page page-enter">
      <BackButton onClick={onBack} />
      <div className="flow-heading flow-heading--center">
        <p className="eyebrow">一键上车</p>
        <h1>输入上车码</h1>
        <p>向车主要一个 12 位上车码，粘贴后自动确认车队。</p>
      </div>

      <div className="code-input-wrap">
        <div className="code-boxes" aria-label="上车码">
          {Array.from({ length: 3 }, (_, index) => (
            <span key={index}>{normalizedCode.slice(index * 4, index * 4 + 4)}</span>
          ))}
          <input value={normalizedCode} onChange={event => setCode(event.target.value)} autoFocus aria-label="输入上车码" />
        </div>
      </div>

      {preview ? (
        <div className="car-preview page-enter">
          <div className="avatar">车</div>
          <div className="car-preview__main">
            <small>车主 {preview.ownerLabel}</small>
            <strong>{preview.carName}</strong>
            <span>{preview.enabledTools.map(kind => TOOL_LABEL[kind]).join(' · ')}</span>
            <span>{formatDateTime(preview.startsAt)}—{formatDateTime(preview.expiresAt)}</span>
          </div>
          <div className="car-preview__seat"><small>你的座位</small><strong>{preview.seatNo} / 4</strong></div>
        </div>
      ) : (
        <div className="preview-placeholder"><Wifi size={22} /><span>输入完整上车码后，这里会显示车队信息</span></div>
      )}

      <label className="nickname-field">
        <span>你的昵称</span>
        <input value={nickname} onChange={event => setNickname(event.target.value)} placeholder="例如：阿杰" maxLength={20} />
      </label>

      <div className="friends-note"><Users size={18} /> 仅加入你认识并信任的人发起的车队。</div>
      <button className="primary-button" onClick={submit} disabled={!preview || busy || waitingForStart}>
        {busy ? <><RefreshCw className="spin" size={19} /> 正在上车...</> : waitingForStart ? <><Clock3 size={20} /> 等待开放时间</> : <><Users size={20} /> 确认并上车</>}
      </button>
      <p className="quiet-note"><ShieldCheck size={15} /> 上车码只用于找到座位，授权仅绑定当前设备</p>
    </section>
  );
}

function ToolChooser({ access, tools, onOpened, onError }: { access: RideAccess; tools: ToolDetection[]; onOpened: (target: LaunchTarget) => void; onError: (message: string) => void }) {
  const [selected, setSelected] = useState<ToolKind>(access.enabledTools[0] ?? 'claude');
  const initialDetection = tools.find(tool => tool.kind === (access.enabledTools[0] ?? 'claude'));
  const [mode, setMode] = useState<LaunchMode>(initialDetection?.desktopInstalled ? 'desktop' : 'terminal');
  const [workDir, setWorkDir] = useState('');
  const [showDir, setShowDir] = useState(false);
  const [busy, setBusy] = useState(false);
  const detection = tools.find(tool => tool.kind === selected);
  const terminalAvailable = detection?.installed ?? false;
  const desktopAvailable = detection?.desktopSupported === true && detection.desktopInstalled;
  const selectedAvailable = mode === 'desktop' ? desktopAvailable : terminalAvailable;

  const selectTool = (kind: ToolKind) => {
    setSelected(kind);
    const next = tools.find(tool => tool.kind === kind);
    setMode(next?.desktopInstalled ? 'desktop' : 'terminal');
  };

  const open = async () => {
    setBusy(true);
    try {
      await launchTool({
        kind: selected,
        mode,
        accessId: access.accessId,
        workDir: mode === 'terminal' ? workDir.trim() || undefined : undefined,
      });
      onOpened({ kind: selected, mode });
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="chooser-page flow-page page-enter">
      <div className="success-banner"><CheckCircle2 size={35} /><span><strong>上车成功</strong><small>已加入 {access.carName}</small></span></div>
      <div className="flow-heading flow-heading--center compact-heading">
        <h1>选择要打开的工具</h1>
        <p>两个工具都可以打开，之后也能随时切换。</p>
      </div>
      <div className="tool-grid chooser-grid">
        {access.enabledTools.map(kind => (
          <button className={`tool-card chooser-card ${selected === kind ? 'tool-card--selected' : ''}`} key={kind} onClick={() => selectTool(kind)}>
            <ToolMark kind={kind} />
            <span className="tool-card__body"><strong>{TOOL_LABEL[kind]}</strong><small>终端与客户端都能使用</small></span>
            <span className={`radio ${selected === kind ? 'radio--on' : ''}`} />
          </button>
        ))}
      </div>
      <div className="launch-mode-picker" aria-label="启动方式">
        <button className={mode === 'desktop' ? 'launch-mode--selected' : ''} onClick={() => setMode('desktop')} disabled={!desktopAvailable} aria-pressed={mode === 'desktop'}>
          <MonitorUp size={19} /><span><strong>客户端</strong><small>{detection?.desktopDetail ?? '正在检测'}</small></span>
        </button>
        <button className={mode === 'terminal' ? 'launch-mode--selected' : ''} onClick={() => setMode('terminal')} disabled={!terminalAvailable} aria-pressed={mode === 'terminal'}>
          <SquareTerminal size={19} /><span><strong>终端</strong><small>{terminalAvailable ? '已安装，可直接打开' : '未找到命令行工具'}</small></span>
        </button>
      </div>
      {mode === 'terminal' && <>
        <button className="folder-toggle" onClick={() => setShowDir(value => !value)}>
          <span>项目目录（可选）</span><ChevronDown size={17} className={showDir ? 'turn' : ''} />
        </button>
        {showDir && <input className="directory-input page-enter" value={workDir} onChange={event => setWorkDir(event.target.value)} placeholder="留空使用默认目录" />}
      </>}
      <button className="primary-button" onClick={open} disabled={busy || !selectedAvailable}>
        {busy ? <><RefreshCw className="spin" size={19} /> 正在打开...</> : mode === 'desktop' ? <><MonitorUp size={20} /> 打开 {TOOL_LABEL[selected]} 客户端</> : <><SquareTerminal size={20} /> 打开 {TOOL_LABEL[selected]} 终端</>}
      </button>
      <p className="quiet-note">客户端会临时使用本车路由，离车后自动恢复原配置</p>
    </section>
  );
}

function RidePage({ access, tools, initiallyOpened, onLeave, onError }: { access: RideAccess; tools: ToolDetection[]; initiallyOpened: LaunchTarget; onLeave: () => void; onError: (message: string) => void }) {
  const [opened, setOpened] = useState<LaunchTarget[]>([initiallyOpened]);
  const [busy, setBusy] = useState<string | null>(null);
  const [leaving, setLeaving] = useState(false);
  const [sharedStatus, setSharedStatus] = useState<SharedCarStatus | null>(null);
  const [showMemberDetails, setShowMemberDetails] = useState(false);

  useEffect(() => trustedWebRtc.subscribeCarStatus(setSharedStatus), []);

  const open = async (kind: ToolKind, mode: LaunchMode) => {
    const key = `${kind}-${mode}`;
    setBusy(key);
    try {
      await launchTool({ kind, mode, accessId: access.accessId });
      setOpened(current => current.some(target => target.kind === kind && target.mode === mode)
        ? current
        : [...current, { kind, mode }]);
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(null);
    }
  };

  const leave = async () => {
    setLeaving(true);
    try {
      await trustedWebRtc.stop();
      await leaveCar(access.accessId);
      onLeave();
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error));
    } finally {
      setLeaving(false);
    }
  };

  return (
    <section className="ride-page page-enter">
      <div className="connection-bar">
        <span><i /> 已连接</span><strong>{access.carName}</strong><span><ShieldCheck size={17} /> 当前设备已绑定</span>
      </div>
      {sharedStatus && <AccountQuotaPanel quotas={sharedStatus.accountQuotas} />}
      <div className="ride-heading">
        <p className="eyebrow">使用中</p>
        <h1>需要哪个，点哪个</h1>
      </div>
      {sharedStatus && (
        <button className="my-usage-card" onClick={() => setShowMemberDetails(true)} aria-label="查看我的使用详情">
          <div>
            <small>我的使用</small>
            <strong>{formatTokens(sharedStatus.member.usage.totalTokens)} Token</strong>
          </div>
          <div>
            <small>5 小时限额</small>
            <strong>
              {sharedStatus.member.tokenLimitStatus.fiveHour.limitTokens === null
                ? '不限额'
                : `剩余 ${formatTokens(sharedStatus.member.tokenLimitStatus.fiveHour.remainingTokens ?? 0)}`}
            </strong>
          </div>
          <span>查看明细 →</span>
        </button>
      )}
      <div className="ride-tool-grid">
        {access.enabledTools.map(kind => {
          const detection = tools.find(tool => tool.kind === kind);
          const terminalOpen = opened.some(target => target.kind === kind && target.mode === 'terminal');
          const desktopOpen = opened.some(target => target.kind === kind && target.mode === 'desktop');
          const isOpen = terminalOpen || desktopOpen;
          return (
            <article className={`ride-tool ${isOpen ? 'ride-tool--open' : ''}`} key={kind}>
              <ToolMark kind={kind} />
              <div className="ride-tool__meta"><strong>{TOOL_LABEL[kind]}</strong><small><i /> {isOpen ? `${desktopOpen ? '客户端' : ''}${desktopOpen && terminalOpen ? '、' : ''}${terminalOpen ? '终端' : ''}已打开` : '未打开'}</small></div>
              <div className="ride-tool__actions">
                <button onClick={() => open(kind, 'desktop')} disabled={busy === `${kind}-desktop` || !detection?.desktopInstalled} title={detection?.desktopDetail}>
                  <MonitorUp size={15} /> {busy === `${kind}-desktop` ? '打开中' : desktopOpen ? '新客户端' : '客户端'}
                </button>
                <button onClick={() => open(kind, 'terminal')} disabled={busy === `${kind}-terminal` || !detection?.installed}>
                  <SquareTerminal size={15} /> {busy === `${kind}-terminal` ? '打开中' : terminalOpen ? '新终端' : '终端'}
                </button>
              </div>
            </article>
          );
        })}
      </div>
      <button className="leave-button" onClick={leave} disabled={leaving}><LogOut size={17} /> {leaving ? '正在离开...' : '离开车队'}</button>
      {sharedStatus && showMemberDetails && (
        <MemberDetailDialog
          seat={{ ...sharedStatus.member, code: '' }}
          onClose={() => setShowMemberDetails(false)}
          editable={false}
        />
      )}
    </section>
  );
}

export default function App() {
  const [screen, setScreen] = useState<Screen>(initialScreen);
  const [tools, setTools] = useState<ToolDetection[]>([]);
  const [loadingTools, setLoadingTools] = useState(true);
  const [car, setCar] = useState<CarSession | null>(null);
  const [access, setAccess] = useState<RideAccess | null>(null);
  const [openedTarget, setOpenedTarget] = useState<LaunchTarget>({ kind: 'claude', mode: 'terminal' });
  const [error, setError] = useState<string | null>(null);

  const loadTools = async () => {
    setLoadingTools(true);
    try {
      setTools(await detectTools());
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setLoadingTools(false);
    }
  };

  useEffect(() => { void loadTools(); }, []);
  useEffect(() => {
    void trustedWebRtc.initialize().catch(reason =>
      setError(reason instanceof Error ? reason.message : String(reason))
    );
  }, []);

  const goHome = () => {
    setScreen('welcome');
    setError(null);
  };

  const page = useMemo(() => {
    if (screen === 'host-setup') return <HostSetup tools={tools} loadingTools={loadingTools} onRefresh={loadTools} onBack={goHome} onStarted={next => { setCar(next); setScreen('host-live'); }} onError={setError} />;
    if (screen === 'host-live' && car) return <HostLive car={car} onStopped={() => { setCar(null); goHome(); }} onError={setError} />;
    if (screen === 'join') return <JoinPage onBack={goHome} onJoined={next => { setAccess(next); setScreen('ready'); }} onError={setError} />;
    if (screen === 'ready' && access) return <ToolChooser access={access} tools={tools} onOpened={target => { setOpenedTarget(target); setScreen('ride'); }} onError={setError} />;
    if (screen === 'ride' && access) return <RidePage access={access} tools={tools} initiallyOpened={openedTarget} onLeave={() => { setAccess(null); goHome(); }} onError={setError} />;
    return <Welcome onHost={() => setScreen('host-setup')} onJoin={() => setScreen('join')} />;
  }, [screen, tools, loadingTools, car, access, openedTarget]);

  return (
    <WindowShell onHome={goHome}>
      {error && <ErrorBanner message={error} onClose={() => setError(null)} />}
      {page}
    </WindowShell>
  );
}
