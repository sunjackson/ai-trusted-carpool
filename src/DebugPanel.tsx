import { Bug, Copy, FolderOpen, PackageOpen, Search, Trash2, X } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import {
  clearBackendDebugLogs,
  clearDebugLogs,
  debugLog,
  getBackendDebugLogs,
  getDebugLogs,
  exportDiagnosticBundle,
  openDebugLogDirectory,
  subscribeToDebugLogs,
  type DebugLogEntry,
  type DebugLogLevel,
} from './debugLog';

type LogFilter = 'all' | DebugLogLevel;

const FILTERS: { value: LogFilter; label: string }[] = [
  { value: 'all', label: '全部' },
  { value: 'debug', label: '调试' },
  { value: 'info', label: '信息' },
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
  const [source, setSource] = useState('all');
  const [search, setSearch] = useState('');
  const [copyLabel, setCopyLabel] = useState('复制日志');
  const [actionLabel, setActionLabel] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);

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

  const mergedLogs = useMemo(() => {
    const unique = new Map<string, DebugLogEntry>();
    for (const entry of [...frontendLogs, ...backendLogs]) {
      unique.set(`${entry.timestamp}|${entry.level}|${entry.source}|${entry.message}`, entry);
    }
    return [...unique.values()].sort((left, right) => left.timestamp - right.timestamp);
  }, [backendLogs, frontendLogs]);
  const sources = useMemo(
    () => [...new Set(mergedLogs.map(entry => entry.source))].sort(),
    [mergedLogs]
  );
  const visibleLogs = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    return mergedLogs.filter(
      entry =>
        matchesFilter(entry, filter) &&
        (source === 'all' || entry.source === source) &&
        (!query || `${entry.source}\n${entry.message}`.toLocaleLowerCase().includes(query))
    );
  }, [filter, mergedLogs, search, source]);

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

  const openLogDirectory = async () => {
    try {
      await openDebugLogDirectory();
      setActionLabel('已打开日志目录');
    } catch (error) {
      debugLog('warn', 'Debug', '打开日志目录失败', error);
      setActionLabel('打开日志目录失败');
    }
  };

  const exportBundle = async () => {
    setExporting(true);
    try {
      await exportDiagnosticBundle();
      setActionLabel('诊断包已导出');
    } catch (error) {
      debugLog('error', 'Debug', '导出诊断包失败', error);
      setActionLabel('导出诊断包失败');
    } finally {
      setExporting(false);
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
          <label className="debug-search">
            <Search size={13} />
            <input
              type="search"
              value={search}
              onChange={event => setSearch(event.target.value)}
              placeholder="搜索脱敏日志"
              aria-label="搜索日志"
            />
          </label>
          <label className="debug-source-filter">
            <span>来源</span>
            <select value={source} onChange={event => setSource(event.target.value)} aria-label="日志来源">
              <option value="all">全部来源</option>
              {sources.map(item => <option value={item} key={item}>{item}</option>)}
            </select>
          </label>
          <div className="debug-toolbar__actions">
            <button onClick={() => void copyLogs()} title="复制当前筛选的日志">
              <Copy size={15} /> {copyLabel}
            </button>
            <button onClick={() => void clearLogs()} title="清空日志">
              <Trash2 size={15} /> 清空
            </button>
            <button onClick={() => void openLogDirectory()} title="打开日志目录">
              <FolderOpen size={15} /> 日志目录
            </button>
            <button onClick={() => void exportBundle()} disabled={exporting} title="导出脱敏诊断包">
              <PackageOpen size={15} /> {exporting ? '导出中' : '导出诊断包'}
            </button>
          </div>
        </div>
        <div className="debug-action-status" role="status">{actionLabel ?? ''}</div>

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
