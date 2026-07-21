import { Bug, Copy, Trash2, X } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  clearBackendDebugLogs,
  clearDebugLogs,
  debugLog,
  getBackendDebugLogs,
  getDebugLogs,
  subscribeToDebugLogs,
  type DebugLogEntry,
  type DebugLogLevel,
} from './debugLog';

type LogFilter = 'all' | 'warn' | 'error';

const FILTERS: { value: LogFilter; label: string }[] = [
  { value: 'all', label: '全部' },
  { value: 'warn', label: '警告' },
  { value: 'error', label: '错误' },
];

const LEVEL_LABEL: Record<DebugLogLevel, string> = {
  debug: '调试',
  info: '信息',
  warn: '警告',
  error: '错误',
};

const formatTime = (timestamp: number): string =>
  new Intl.DateTimeFormat('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    fractionalSecondDigits: 3,
    hour12: false,
  }).format(timestamp);

const matchesFilter = (entry: DebugLogEntry, filter: LogFilter): boolean =>
  filter === 'all' || entry.level === filter;

export function DebugPanel({ onClose }: { onClose: () => void }) {
  const [frontendLogs, setFrontendLogs] = useState(getDebugLogs);
  const [backendLogs, setBackendLogs] = useState<DebugLogEntry[]>([]);
  const [filter, setFilter] = useState<LogFilter>('all');
  const [copyLabel, setCopyLabel] = useState('复制日志');

  const refreshBackend = useCallback(async () => {
    try {
      setBackendLogs(await getBackendDebugLogs());
    } catch (error) {
      debugLog('warn', 'Debug', '读取 Rust 日志失败', error);
    }
  }, []);

  useEffect(() => {
    const unsubscribe = subscribeToDebugLogs(() => setFrontendLogs(getDebugLogs()));
    void refreshBackend();
    const timer = window.setInterval(() => void refreshBackend(), 1000);
    return () => {
      unsubscribe();
      window.clearInterval(timer);
    };
  }, [refreshBackend]);

  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', closeOnEscape);
    return () => window.removeEventListener('keydown', closeOnEscape);
  }, [onClose]);

  const visibleLogs = useMemo(
    () =>
      [...frontendLogs, ...backendLogs]
        .sort((left, right) => left.timestamp - right.timestamp)
        .filter(entry => matchesFilter(entry, filter)),
    [backendLogs, filter, frontendLogs]
  );

  const copyLogs = async () => {
    const output = visibleLogs
      .map(
        entry =>
          `${new Date(entry.timestamp).toISOString()} [${entry.level.toUpperCase()}] [${entry.source}] ${entry.message}`
      )
      .join('\n');
    try {
      await navigator.clipboard.writeText(output || '暂无日志');
      setCopyLabel('已复制');
      window.setTimeout(() => setCopyLabel('复制日志'), 1400);
    } catch (error) {
      debugLog('error', 'Debug', '复制日志失败', error);
    }
  };

  const clearLogs = async () => {
    clearDebugLogs();
    setBackendLogs([]);
    try {
      await clearBackendDebugLogs();
    } catch (error) {
      debugLog('warn', 'Debug', '清空 Rust 日志失败', error);
    }
  };

  return (
    <div className="debug-backdrop" role="presentation">
      <section className="debug-panel" role="dialog" aria-modal="true" aria-labelledby="debug-title">
        <header className="debug-panel__header">
          <span className="debug-panel__title-icon"><Bug size={18} /></span>
          <div>
            <h2 id="debug-title">调试日志</h2>
            <span>{visibleLogs.length} 条记录</span>
          </div>
          <button className="debug-icon-button" onClick={onClose} aria-label="关闭调试模式" title="关闭">
            <X size={18} />
          </button>
        </header>

        <div className="debug-toolbar">
          <div className="debug-filter" role="group" aria-label="日志级别">
            {FILTERS.map(option => (
              <button
                key={option.value}
                className={filter === option.value ? 'is-active' : ''}
                aria-pressed={filter === option.value}
                onClick={() => setFilter(option.value)}
              >
                {option.label}
              </button>
            ))}
          </div>
          <div className="debug-toolbar__actions">
            <button onClick={() => void copyLogs()} title="复制当前筛选的日志">
              <Copy size={15} /> {copyLabel}
            </button>
            <button onClick={() => void clearLogs()} title="清空日志">
              <Trash2 size={15} /> 清空
            </button>
          </div>
        </div>

        <div className="debug-log-list" aria-live="polite">
          {visibleLogs.length === 0 ? (
            <p className="debug-log-empty">暂无符合条件的日志</p>
          ) : (
            visibleLogs.map(entry => (
              <article className={`debug-log debug-log--${entry.level}`} key={entry.id}>
                <div className="debug-log__meta">
                  <time dateTime={new Date(entry.timestamp).toISOString()}>{formatTime(entry.timestamp)}</time>
                  <span>{LEVEL_LABEL[entry.level]}</span>
                  <strong>{entry.source}</strong>
                </div>
                <pre>{entry.message}</pre>
              </article>
            ))
          )}
        </div>
      </section>
    </div>
  );
}
