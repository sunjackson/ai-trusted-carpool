import {
  ArchiveRestore,
  Check,
  Code2,
  FileJson,
  HardDriveDownload,
  KeyRound,
  LoaderCircle,
  LockKeyhole,
  RotateCcw,
  Save,
  ShieldCheck,
  Sparkles,
  Trash2,
  Upload,
  X,
} from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  cancelAccountImport,
  cancelAccountRestore,
  commitAccountImport,
  commitAccountRestore,
  deleteAccount,
  exportAccountBackup,
  listAccounts,
  previewAccountImport,
  previewAccountRestore,
  retryAccountRoute,
  updateAccount,
} from './api';
import type {
  AccountImportPreview,
  AccountImportResult,
  AccountRestoreMode,
  AccountRestorePreview,
  LocalAccountSummary,
  ToolKind,
} from './types';

type ImportTool = ToolKind | 'auto';
type AccountDraft = { name: string; priority: string };

const MAX_ACCOUNT_PRIORITY = 1_000_000;
const MAX_IMPORT_BYTES = 8 * 1024 * 1024;
const MAX_BACKUP_BYTES = 24 * 1024 * 1024;

const TOOL_LABEL: Record<ToolKind, string> = { claude: 'Claude', codex: 'Codex' };
const AUTH_LABEL: Record<LocalAccountSummary['authKind'], string> = {
  apiKey: 'API Key',
  oauth: 'OAuth',
};
const SOURCE_LABEL: Record<string, string> = {
  file: 'JSON 文件',
  json: '粘贴内容',
  local: '本机配置',
  unknown: '本机账号库',
};
const HEALTH_REASON_LABEL: Record<NonNullable<LocalAccountSummary['routeHealth']['reason']>, string> = {
  network: '网络连接失败',
  authentication: '官方认证失败',
  rateLimited: '官方额度受限',
  upstream: '官方服务异常',
  expired: '凭据已过期',
};
const PREVIEW_ACTION_LABEL = {
  new: '新增',
  update: '更新',
  conflict: '冲突',
} as const;

const accountHealth = (account: LocalAccountSummary) => {
  if (account.credentialState === 'reimportRequired') {
    return { label: '需从官方客户端重新导入', tone: 'blocked', retryable: false } as const;
  }
  if (account.credentialState === 'expired') {
    return { label: '凭据过期', tone: 'blocked', retryable: false } as const;
  }
  if (account.routeHealth.status === 'cooling') {
    return { label: '冷却中', tone: 'cooling', retryable: true } as const;
  }
  return { label: '正常', tone: 'healthy', retryable: false } as const;
};

const messageFrom = (reason: unknown): string =>
  reason instanceof Error ? reason.message : String(reason);

const summarizeImport = ({ imported, updated }: Pick<AccountImportResult, 'imported' | 'updated'>): string => {
  const parts: string[] = [];
  if (imported > 0) parts.push(`新增 ${imported} 个账号`);
  if (updated > 0) parts.push(`更新 ${updated} 个账号`);
  return parts.join('，') || '没有新增或更新账号';
};

function AccountToolIcon({ tool }: { tool: ToolKind }) {
  return (
    <span className={`account-tool-icon account-tool-icon--${tool}`} aria-hidden="true">
      {tool === 'claude' ? <Sparkles size={17} /> : <Code2 size={17} />}
    </span>
  );
}

export function AccountManager({ onClose }: { onClose: () => void }) {
  const [accounts, setAccounts] = useState<LocalAccountSummary[]>([]);
  const [drafts, setDrafts] = useState<Record<string, AccountDraft>>({});
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [content, setContent] = useState('');
  const [importName, setImportName] = useState('');
  const [importTool, setImportTool] = useState<ImportTool>('auto');
  const [importPreview, setImportPreview] = useState<AccountImportPreview | null>(null);
  const [backupPassphrase, setBackupPassphrase] = useState('');
  const [backupPassphraseConfirm, setBackupPassphraseConfirm] = useState('');
  const [restorePassphrase, setRestorePassphrase] = useState('');
  const [restoreMode, setRestoreMode] = useState<AccountRestoreMode>('merge');
  const [restorePreview, setRestorePreview] = useState<AccountRestorePreview | null>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const backupFileInputRef = useRef<HTMLInputElement>(null);
  const dirtyDraftIdsRef = useRef(new Set<string>());
  const loadRequestIdRef = useRef(0);

  const replaceAccounts = useCallback((next: LocalAccountSummary[]) => {
    const sorted = [...next].sort(
      (left, right) => left.priority - right.priority || left.createdAtMs - right.createdAtMs
    );
    const accountIds = new Set(sorted.map(account => account.id));
    for (const id of dirtyDraftIdsRef.current) {
      if (!accountIds.has(id)) dirtyDraftIdsRef.current.delete(id);
    }
    setAccounts(sorted);
    setDrafts(current =>
      Object.fromEntries(
        sorted.map(account => {
          const existing = current[account.id];
          return [
            account.id,
            dirtyDraftIdsRef.current.has(account.id) && existing
              ? existing
              : { name: account.name, priority: String(account.priority) },
          ];
        })
      )
    );
  }, []);

  const load = useCallback(
    async (showLoading = true) => {
      const requestId = ++loadRequestIdRef.current;
      if (showLoading) setLoading(true);
      try {
        const next = await listAccounts();
        if (requestId === loadRequestIdRef.current) replaceAccounts(next);
      } catch (reason) {
        if (requestId === loadRequestIdRef.current) setError(messageFrom(reason));
      } finally {
        if (requestId === loadRequestIdRef.current) setLoading(false);
      }
    },
    [replaceAccounts]
  );

  useEffect(() => {
    void load();
    return () => {
      loadRequestIdRef.current += 1;
    };
  }, [load]);

  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && busy === null && !importPreview && !restorePreview) onClose();
    };
    window.addEventListener('keydown', closeOnEscape);
    return () => window.removeEventListener('keydown', closeOnEscape);
  }, [busy, importPreview, onClose, restorePreview]);

  const run = async (key: string, operation: () => Promise<string>) => {
    if (loading || busy !== null) return;
    setBusy(key);
    setError(null);
    setNotice(null);
    try {
      const result = await operation();
      await load(false);
      setNotice(result);
    } catch (reason) {
      setError(messageFrom(reason));
    } finally {
      setBusy(null);
    }
  };

  const beginImportPreview = async (
    key: string,
    input: Parameters<typeof previewAccountImport>[0]
  ) => {
    if (loading || busy !== null) return false;
    setBusy(key);
    setError(null);
    setNotice(null);
    try {
      const preview = await previewAccountImport(input);
      setImportPreview(preview);
      return true;
    } catch (reason) {
      setError(messageFrom(reason));
      return false;
    } finally {
      setBusy(null);
    }
  };

  const importLocal = () => void beginImportPreview('local', { local: true });

  const importContent = () => {
    if (!content.trim()) {
      setError('请输入 API Key 或账号 JSON');
      return;
    }
    const pasted = content.trim();
    void beginImportPreview('paste', {
      content: pasted,
      source: 'json',
      ...(importTool === 'auto' ? {} : { tool: importTool }),
      ...(importName.trim() ? { name: importName.trim() } : {}),
    }).then(created => {
      if (!created) return;
      // Once Rust owns the short-lived preview session, do not retain the
      // credential text in React state.
      setContent('');
      setImportName('');
    });
  };

  const importFiles = (files: FileList | null) => {
    if (!files?.length) return;
    void (async () => {
      if (loading || busy !== null) return;
      setBusy('files');
      setError(null);
      setNotice(null);
      const selectedFiles = Array.from(files);
      try {
        const totalBytes = selectedFiles.reduce((total, file) => total + file.size, 0);
        if (totalBytes > MAX_IMPORT_BYTES) {
          throw new Error('所选文件合计过大（最大 8 MiB）');
        }
        const contents = await Promise.all(selectedFiles.map(file => file.text()));
        const preview = await previewAccountImport({ contents, source: 'file' });
        setImportPreview(preview);
      } catch (reason) {
        setError(messageFrom(reason));
      } finally {
        setBusy(null);
        if (fileInputRef.current) fileInputRef.current.value = '';
      }
    })();
  };

  const confirmImport = () => {
    const preview = importPreview;
    if (!preview || preview.items.some(item => item.action === 'conflict')) return;
    void run('commit-import', async () => {
      const result = await commitAccountImport(preview.sessionId);
      setImportPreview(null);
      return `账号导入完成：${summarizeImport(result)}`;
    });
  };

  const cancelImportPreview = () => {
    const preview = importPreview;
    if (!preview || busy !== null) return;
    setImportPreview(null);
    void cancelAccountImport(preview.sessionId).catch(reason => {
      setError(messageFrom(reason));
    });
  };

  const exportBackup = () => {
    if (backupPassphrase.length < 8) {
      setError('备份口令至少需要 8 个字符');
      return;
    }
    if (backupPassphrase !== backupPassphraseConfirm) {
      setError('两次输入的备份口令不一致');
      return;
    }
    const passphrase = backupPassphrase;
    setBackupPassphrase('');
    setBackupPassphraseConfirm('');
    void run('export-backup', async () => {
      const path = await exportAccountBackup(passphrase);
      return `加密备份已导出：${path}`;
    });
  };

  const previewRestoreFile = (files: FileList | null) => {
    const file = files?.[0];
    if (!file) return;
    if (restorePassphrase.length < 8) {
      setError('请输入至少 8 个字符的备份口令');
      if (backupFileInputRef.current) backupFileInputRef.current.value = '';
      return;
    }
    if (file.size > MAX_BACKUP_BYTES) {
      setError('备份文件过大（最大 24 MiB）');
      if (backupFileInputRef.current) backupFileInputRef.current.value = '';
      return;
    }
    const passphrase = restorePassphrase;
    setRestorePassphrase('');
    void (async () => {
      if (loading || busy !== null) return;
      setBusy('preview-restore');
      setError(null);
      setNotice(null);
      try {
        const preview = await previewAccountRestore({
          content: await file.text(),
          passphrase,
          mode: restoreMode,
        });
        setRestorePreview(preview);
      } catch (reason) {
        setError(messageFrom(reason));
      } finally {
        setBusy(null);
        if (backupFileInputRef.current) backupFileInputRef.current.value = '';
      }
    })();
  };

  const confirmRestore = () => {
    const preview = restorePreview;
    if (!preview || preview.items.some(item => item.action === 'conflict')) return;
    let confirmReplace = false;
    if (preview.mode === 'replace') {
      if (!window.confirm('替换恢复会删除备份中不存在的本机账号，是否继续？')) return;
      if (!window.confirm('再次确认：将先创建回滚快照，然后替换整个本机账号池。')) return;
      confirmReplace = true;
    }
    void run('commit-restore', async () => {
      const result = await commitAccountRestore(
        preview.sessionId,
        preview.mode,
        confirmReplace
      );
      setRestorePreview(null);
      return `账号恢复完成：${summarizeImport(result)}${result.removed ? `，移除 ${result.removed} 个账号` : ''}`;
    });
  };

  const cancelRestorePreview = () => {
    const preview = restorePreview;
    if (!preview || busy !== null) return;
    setRestorePreview(null);
    void cancelAccountRestore(preview.sessionId).catch(reason => {
      setError(messageFrom(reason));
    });
  };

  const updateDraft = (
    account: LocalAccountSummary,
    patch: Partial<AccountDraft>
  ) => {
    setDrafts(current => {
      const next = {
        ...(current[account.id] ?? { name: account.name, priority: String(account.priority) }),
        ...patch,
      };
      if (next.name.trim() !== account.name || next.priority !== String(account.priority)) {
        dirtyDraftIdsRef.current.add(account.id);
      } else {
        dirtyDraftIdsRef.current.delete(account.id);
      }
      return { ...current, [account.id]: next };
    });
  };

  const saveAccount = (account: LocalAccountSummary) => {
    const draft = drafts[account.id];
    if (!draft?.name.trim()) {
      setError('账号名称不能为空');
      return;
    }
    if (!draft.priority.trim()) {
      setError(`优先级必须是 0—${MAX_ACCOUNT_PRIORITY} 之间的整数`);
      return;
    }
    const priority = Number(draft.priority);
    if (!Number.isInteger(priority) || priority < 0 || priority > MAX_ACCOUNT_PRIORITY) {
      setError(`优先级必须是 0—${MAX_ACCOUNT_PRIORITY} 之间的整数`);
      return;
    }
    void run(`save:${account.id}`, async () => {
      const updated = await updateAccount({ id: account.id, name: draft.name.trim(), priority });
      dirtyDraftIdsRef.current.delete(account.id);
      setDrafts(current => ({
        ...current,
        [account.id]: { name: updated.name, priority: String(updated.priority) },
      }));
      return `已保存 ${draft.name.trim()}`;
    });
  };

  const toggleAccount = (account: LocalAccountSummary) =>
    void run(`toggle:${account.id}`, async () => {
      await updateAccount({ id: account.id, enabled: !account.enabled });
      return account.enabled ? `已停用 ${account.name}` : `已启用 ${account.name}`;
    });

  const retryRoute = (account: LocalAccountSummary) =>
    void run(`retry:${account.id}`, async () => {
      await retryAccountRoute(account.id);
      return `已允许 ${account.name} 立即重试`;
    });

  const removeAccount = (account: LocalAccountSummary) => {
    if (!window.confirm(`确认删除“${account.name}”？此操作不会删除原始工具配置。`)) return;
    void run(`delete:${account.id}`, async () => {
      await deleteAccount(account.id);
      dirtyDraftIdsRef.current.delete(account.id);
      return `已删除 ${account.name}`;
    });
  };

  const isBusy = busy !== null;
  const hasPreview = importPreview !== null || restorePreview !== null;
  const isLocked = loading || isBusy || hasPreview;

  return (
    <div
      className="account-manager-backdrop"
      role="presentation"
      onMouseDown={event => event.target === event.currentTarget && !isBusy && !hasPreview && onClose()}
    >
      <section
        className="account-manager"
        role="dialog"
        aria-modal="true"
        aria-labelledby="account-manager-title"
      >
        <header className="account-manager__header">
          <span className="account-manager__title-icon"><KeyRound size={19} /></span>
          <div>
            <h2 id="account-manager-title">本机账号管理</h2>
            <span>优先级数值越小，动态路由时越优先</span>
          </div>
          <button className="dialog-close" onClick={onClose} disabled={isBusy || hasPreview} aria-label="关闭账号管理">
            <X size={18} />
          </button>
        </header>

        <div className="account-privacy-note">
          <ShieldCheck size={18} />
          <p>
            <strong>凭据只保存在这台设备</strong>
            <span>不会通过拼车连接发送给车主或乘客；乘客维护的账号也不会传给车主。</span>
          </p>
        </div>

        {(error || notice) && (
          <div className={`account-manager__message ${error ? 'is-error' : 'is-success'}`} role={error ? 'alert' : 'status'}>
            {error ? <X size={15} /> : <Check size={15} />}
            <span>{error ?? notice}</span>
            <button onClick={() => { setError(null); setNotice(null); }} aria-label="关闭账号提示"><X size={14} /></button>
          </div>
        )}

        <div className="account-manager__body">
          <aside className="account-import-panel">
            <div className="account-section-heading">
              <div><strong>导入账号</strong><span>支持 Claude 与 Codex</span></div>
            </div>

            <button className="account-import-action" onClick={importLocal} disabled={isLocked}>
              {busy === 'local' ? <LoaderCircle className="spin" size={17} /> : <HardDriveDownload size={17} />}
              <span><strong>导入本机配置</strong><small>读取当前设备已有登录</small></span>
            </button>

            <input
              ref={fileInputRef}
              className="visually-hidden"
              type="file"
              accept="application/json,.json"
              multiple
              aria-label="选择 JSON 文件"
              onChange={event => importFiles(event.target.files)}
              disabled={isLocked}
            />
            <button className="account-import-action" onClick={() => fileInputRef.current?.click()} disabled={isLocked}>
              {busy === 'files' ? <LoaderCircle className="spin" size={17} /> : <FileJson size={17} />}
              <span><strong>选择 JSON 文件</strong><small>可一次选择多个文件</small></span>
            </button>

            <div className="account-import-divider"><span>或粘贴</span></div>

            <label className="account-field">
              <span>账号类型</span>
              <select value={importTool} onChange={event => setImportTool(event.target.value as ImportTool)} disabled={isLocked}>
                <option value="auto">自动识别</option>
                <option value="claude">Claude</option>
                <option value="codex">Codex</option>
              </select>
            </label>
            <label className="account-field">
              <span>账号名称（可选）</span>
              <input value={importName} onChange={event => setImportName(event.target.value)} placeholder="例如：Claude 主账号" disabled={isLocked} />
            </label>
            <label className="account-field">
              <span>API Key 或 JSON</span>
              <textarea
                value={content}
                onChange={event => setContent(event.target.value)}
                placeholder="粘贴凭据内容"
                spellCheck={false}
                disabled={isLocked}
              />
            </label>
            <button className="account-primary-button" onClick={importContent} disabled={isLocked || !content.trim()}>
              {busy === 'paste' ? <LoaderCircle className="spin" size={17} /> : <Upload size={17} />}
              预览导入
            </button>

            <details className="account-backup-section">
              <summary><LockKeyhole size={15} /> 加密备份与恢复</summary>
              <p>只备份账号凭据和账号设置，不包含设备身份、拼车会话、日志或客户端 profile。</p>
              <label className="account-field">
                <span>备份口令</span>
                <input
                  type="password"
                  value={backupPassphrase}
                  onChange={event => setBackupPassphrase(event.target.value)}
                  autoComplete="new-password"
                  aria-label="备份口令"
                  disabled={isLocked}
                />
              </label>
              <label className="account-field">
                <span>再次输入口令</span>
                <input
                  type="password"
                  value={backupPassphraseConfirm}
                  onChange={event => setBackupPassphraseConfirm(event.target.value)}
                  autoComplete="new-password"
                  aria-label="确认备份口令"
                  disabled={isLocked}
                />
              </label>
              <button
                className="account-secondary-button"
                onClick={exportBackup}
                disabled={isLocked || !backupPassphrase || !backupPassphraseConfirm}
              >
                {busy === 'export-backup' ? <LoaderCircle className="spin" size={15} /> : <HardDriveDownload size={15} />}
                导出加密备份
              </button>

              <div className="account-import-divider"><span>恢复</span></div>
              <label className="account-field">
                <span>备份口令</span>
                <input
                  type="password"
                  value={restorePassphrase}
                  onChange={event => setRestorePassphrase(event.target.value)}
                  autoComplete="current-password"
                  aria-label="恢复备份口令"
                  disabled={isLocked}
                />
              </label>
              <label className="account-field">
                <span>恢复方式</span>
                <select
                  value={restoreMode}
                  onChange={event => setRestoreMode(event.target.value as AccountRestoreMode)}
                  aria-label="恢复方式"
                  disabled={isLocked}
                >
                  <option value="merge">合并（默认，保留本机名称和优先级）</option>
                  <option value="replace">替换整个账号池</option>
                </select>
              </label>
              <input
                ref={backupFileInputRef}
                className="visually-hidden"
                type="file"
                accept=".tcarpool-backup,application/json"
                aria-label="选择账号备份文件"
                onChange={event => previewRestoreFile(event.target.files)}
                disabled={isLocked}
              />
              <button
                className="account-secondary-button"
                onClick={() => backupFileInputRef.current?.click()}
                disabled={isLocked || restorePassphrase.length < 8}
              >
                {busy === 'preview-restore' ? <LoaderCircle className="spin" size={15} /> : <ArchiveRestore size={15} />}
                选择备份并预览
              </button>
            </details>
          </aside>

          <section className="account-list-panel" aria-label="本机账号列表">
            <div className="account-section-heading">
              <div><strong>已导入账号</strong><span>同优先级账号会自动轮换</span></div>
              <b>{accounts.length}</b>
            </div>

            {loading ? (
              <div className="account-list-state"><LoaderCircle className="spin" size={22} /><span>正在读取本机账号...</span></div>
            ) : accounts.length === 0 ? (
              <div className="account-list-state">
                <KeyRound size={24} />
                <strong>还没有导入账号</strong>
                <span>从本机配置、JSON 文件或 API Key 开始。</span>
              </div>
            ) : (
              <div className="account-list">
                {accounts.map(account => {
                  const draft = drafts[account.id] ?? { name: account.name, priority: String(account.priority) };
                  const changed = draft.name.trim() !== account.name || draft.priority !== String(account.priority);
                  const rowBusy = busy?.endsWith(account.id) ?? false;
                  const health = accountHealth(account);
                  const healthReason = account.routeHealth.reason
                    ? HEALTH_REASON_LABEL[account.routeHealth.reason]
                    : null;
                  return (
                    <article className={`account-row ${account.enabled ? '' : 'is-disabled'}`} key={account.id}>
                      <div className="account-row__summary">
                        <AccountToolIcon tool={account.tool} />
                        <div>
                          <strong>{account.name}</strong>
                          <span>{TOOL_LABEL[account.tool]} · {AUTH_LABEL[account.authKind]} · 凭据已隐藏</span>
                        </div>
                        <label className="account-toggle" title={account.enabled ? '停用账号' : '启用账号'}>
                          <input type="checkbox" checked={account.enabled} onChange={() => toggleAccount(account)} disabled={isLocked} aria-label={`${account.enabled ? '停用' : '启用'} ${account.name}`} />
                          <span aria-hidden="true" />
                        </label>
                      </div>
                      <div className="account-row__health">
                        <span className={`account-health-badge is-${health.tone}`}>{health.label}</span>
                        {healthReason && <small>{healthReason}</small>}
                        {health.retryable && (
                          <button
                            onClick={() => retryRoute(account)}
                            disabled={isLocked}
                            aria-label={`立即重试 ${account.name}`}
                          >
                            {rowBusy && busy?.startsWith('retry:') ? <LoaderCircle className="spin" size={12} /> : <RotateCcw size={12} />}
                            立即重试
                          </button>
                        )}
                      </div>
                      <div className="account-row__fields">
                        <label className="account-field">
                          <span>名称</span>
                          <input
                            value={draft.name}
                            onChange={event => updateDraft(account, { name: event.target.value })}
                            disabled={isLocked}
                            aria-label={`${account.name} 名称`}
                          />
                        </label>
                        <label className="account-field account-field--priority">
                          <span>优先级</span>
                          <input
                            type="number"
                            min="0"
                            max={MAX_ACCOUNT_PRIORITY}
                            step="1"
                            value={draft.priority}
                            onChange={event => updateDraft(account, { priority: event.target.value })}
                            disabled={isLocked}
                            aria-label={`${account.name} 优先级`}
                          />
                        </label>
                        <button className="account-row__icon-button" onClick={() => saveAccount(account)} disabled={isLocked || !changed} aria-label={`保存 ${account.name}`} title="保存名称和优先级">
                          {rowBusy && busy?.startsWith('save:') ? <LoaderCircle className="spin" size={16} /> : <Save size={16} />}
                        </button>
                        <button className="account-row__icon-button is-danger" onClick={() => removeAccount(account)} disabled={isLocked} aria-label={`删除 ${account.name}`} title="删除账号">
                          {rowBusy && busy?.startsWith('delete:') ? <LoaderCircle className="spin" size={16} /> : <Trash2 size={16} />}
                        </button>
                      </div>
                      <small className="account-row__source">来源：{SOURCE_LABEL[account.source] ?? account.source}</small>
                    </article>
                  );
                })}
              </div>
            )}
          </section>
        </div>

        {importPreview && (
          <div className="account-preview-backdrop" role="presentation">
            <section className="account-preview" role="dialog" aria-modal="true" aria-labelledby="account-import-preview-title">
              <header>
                <div>
                  <strong id="account-import-preview-title">确认导入预览</strong>
                  <span>凭据已留在 Rust，下面只显示脱敏账号信息</span>
                </div>
              </header>
              <div className="account-preview-list">
                {importPreview.items.map(item => (
                  <article key={item.itemId}>
                    <AccountToolIcon tool={item.tool} />
                    <div><strong>{item.name}</strong><span>{TOOL_LABEL[item.tool]} · {AUTH_LABEL[item.authKind]} · {SOURCE_LABEL[item.source]}</span></div>
                    <b className={`is-${item.action}`}>{PREVIEW_ACTION_LABEL[item.action]}</b>
                  </article>
                ))}
              </div>
              {importPreview.items.some(item => item.action === 'conflict') && (
                <p className="account-preview-warning" role="alert">存在身份或来源冲突，账号池不会被修改。请取消后先处理现有账号。</p>
              )}
              <footer>
                <button onClick={cancelImportPreview} disabled={isBusy}>取消</button>
                <button
                  className="account-primary-button"
                  onClick={confirmImport}
                  disabled={isBusy || importPreview.items.some(item => item.action === 'conflict')}
                >
                  {busy === 'commit-import' && <LoaderCircle className="spin" size={15} />}
                  确认导入
                </button>
              </footer>
            </section>
          </div>
        )}

        {restorePreview && (
          <div className="account-preview-backdrop" role="presentation">
            <section className="account-preview" role="dialog" aria-modal="true" aria-labelledby="account-restore-preview-title">
              <header>
                <div>
                  <strong id="account-restore-preview-title">确认备份恢复</strong>
                  <span>{restorePreview.mode === 'merge' ? '合并恢复会保留本机名称、优先级和启用状态' : '替换恢复会先创建本机回滚快照'}</span>
                </div>
              </header>
              <div className="account-preview-list">
                {restorePreview.items.map(item => (
                  <article key={item.itemId}>
                    <AccountToolIcon tool={item.tool} />
                    <div><strong>{item.name}</strong><span>{TOOL_LABEL[item.tool]} · {AUTH_LABEL[item.authKind]} · {SOURCE_LABEL[item.source]}</span></div>
                    <b className={`is-${item.action}`}>{PREVIEW_ACTION_LABEL[item.action]}</b>
                  </article>
                ))}
              </div>
              {restorePreview.mode === 'replace' && restorePreview.removeCount > 0 && (
                <p className="account-preview-warning">替换后将移除备份中不存在的 {restorePreview.removeCount} 个本机账号。</p>
              )}
              {restorePreview.items.some(item => item.action === 'conflict') && (
                <p className="account-preview-warning" role="alert">备份与本机账号身份冲突，当前账号池不会被修改。</p>
              )}
              <footer>
                <button onClick={cancelRestorePreview} disabled={isBusy}>取消</button>
                <button
                  className="account-primary-button"
                  onClick={confirmRestore}
                  disabled={isBusy || restorePreview.items.some(item => item.action === 'conflict')}
                >
                  {busy === 'commit-restore' && <LoaderCircle className="spin" size={15} />}
                  {restorePreview.mode === 'replace' ? '确认替换恢复' : '确认合并恢复'}
                </button>
              </footer>
            </section>
          </div>
        )}
      </section>
    </div>
  );
}
