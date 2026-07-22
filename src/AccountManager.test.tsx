import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  AccountImportPreview,
  AccountImportResult,
  AccountRestorePreview,
  LocalAccountSummary,
} from './types';

const apiMocks = vi.hoisted(() => ({
  cancelAccountImport: vi.fn(),
  cancelAccountRestore: vi.fn(),
  commitAccountImport: vi.fn(),
  commitAccountRestore: vi.fn(),
  deleteAccount: vi.fn(),
  exportAccountBackup: vi.fn(),
  importAccounts: vi.fn(),
  importLocalAccounts: vi.fn(),
  listAccounts: vi.fn(),
  previewAccountImport: vi.fn(),
  previewAccountRestore: vi.fn(),
  retryAccountRoute: vi.fn(),
  updateAccount: vi.fn(),
}));

vi.mock('./api', () => apiMocks);

import { AccountManager } from './AccountManager';

const accounts: LocalAccountSummary[] = [
  {
    id: 'claude-main',
    tool: 'claude',
    name: 'Claude 主账号',
    authKind: 'oauth',
    enabled: true,
    priority: 10,
    source: 'Claude 本机配置',
    createdAtMs: 100,
    updatedAtMs: 100,
    credentialState: 'normal',
    routeHealth: {
      status: 'normal',
      reason: null,
      cooldownUntilMs: null,
      consecutiveFailures: 0,
      lastAttemptAtMs: null,
      lastSuccessAtMs: null,
      lastFailureAtMs: null,
    },
  },
  {
    id: 'codex-backup',
    tool: 'codex',
    name: 'Codex 备用账号',
    authKind: 'apiKey',
    enabled: true,
    priority: 20,
    source: '手动导入',
    createdAtMs: 200,
    updatedAtMs: 200,
    credentialState: 'normal',
    routeHealth: {
      status: 'normal',
      reason: null,
      cooldownUntilMs: null,
      consecutiveFailures: 0,
      lastAttemptAtMs: null,
      lastSuccessAtMs: null,
      lastFailureAtMs: null,
    },
  },
];

const importResult = (
  affected: LocalAccountSummary[],
  imported = affected.length,
  updated = 0
): AccountImportResult => ({ imported, updated, accounts: affected });

const importPreview = (
  overrides: Partial<AccountImportPreview> = {}
): AccountImportPreview => ({
  sessionId: 'import-session',
  expiresAtMs: Date.now() + 10 * 60_000,
  items: [
    {
      itemId: 'preview-item-1',
      tool: 'claude',
      authKind: 'oauth',
      name: 'Claude 预览账号',
      source: 'local',
      action: 'new',
    },
  ],
  ...overrides,
});

const restorePreview = (
  overrides: Partial<AccountRestorePreview> = {}
): AccountRestorePreview => ({
  ...importPreview({ sessionId: 'restore-session' }),
  mode: 'merge',
  removeCount: 0,
  ...overrides,
});

describe('AccountManager', () => {
  beforeEach(() => {
    vi.resetAllMocks();
    apiMocks.listAccounts.mockResolvedValue(accounts);
    apiMocks.importLocalAccounts.mockResolvedValue(importResult([accounts[0]]));
    apiMocks.importAccounts.mockResolvedValue(importResult([accounts[0]]));
    apiMocks.previewAccountImport.mockResolvedValue(importPreview());
    apiMocks.commitAccountImport.mockResolvedValue(importResult([accounts[0]]));
    apiMocks.cancelAccountImport.mockResolvedValue(true);
    apiMocks.exportAccountBackup.mockResolvedValue('/tmp/accounts.tcarpool-backup');
    apiMocks.previewAccountRestore.mockResolvedValue(restorePreview());
    apiMocks.commitAccountRestore.mockResolvedValue({
      ...importResult([accounts[0]], 0, 1),
      removed: 0,
    });
    apiMocks.cancelAccountRestore.mockResolvedValue(true);
    apiMocks.updateAccount.mockResolvedValue(accounts[0]);
    apiMocks.retryAccountRoute.mockResolvedValue(accounts[0]);
    apiMocks.deleteAccount.mockResolvedValue(undefined);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.restoreAllMocks();
  });

  it('shows redacted summaries and maintains names, priorities, and enabled state', async () => {
    render(<AccountManager onClose={vi.fn()} />);

    expect(await screen.findByText('Claude 主账号')).toBeInTheDocument();
    expect(screen.getByText('Claude · OAuth · 凭据已隐藏')).toBeInTheDocument();
    expect(screen.getByText('Codex · API Key · 凭据已隐藏')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Claude 主账号 名称'), {
      target: { value: 'Claude 高优先级' },
    });
    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '2' },
    });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));
    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({
        id: 'claude-main',
        name: 'Claude 高优先级',
        priority: 2,
      })
    );

    fireEvent.click(screen.getByRole('checkbox', { name: '停用 Codex 备用账号' }));
    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({ id: 'codex-backup', enabled: false })
    );
  });

  it('shows enumerated health without leaking errors and retries one cooled account', async () => {
    apiMocks.listAccounts.mockResolvedValueOnce([
      {
        ...accounts[0],
        routeHealth: {
          ...accounts[0].routeHealth,
          status: 'cooling',
          reason: 'rateLimited',
          cooldownUntilMs: Date.now() + 60_000,
          consecutiveFailures: 1,
          lastFailureAtMs: Date.now(),
        },
      },
      {
        ...accounts[1],
        credentialState: 'reimportRequired',
        routeHealth: {
          ...accounts[1].routeHealth,
          reason: 'authentication',
          consecutiveFailures: 2,
        },
      },
    ]);
    render(<AccountManager onClose={vi.fn()} />);

    expect(await screen.findByText('冷却中')).toBeInTheDocument();
    expect(screen.getByText('官方额度受限')).toBeInTheDocument();
    expect(screen.getByText('需从官方客户端重新导入')).toBeInTheDocument();
    expect(screen.getByText('官方认证失败')).toBeInTheDocument();
    expect(screen.queryByText(/429|token|secret/i)).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '立即重试 Claude 主账号' }));
    await waitFor(() => expect(apiMocks.retryAccountRoute).toHaveBeenCalledWith('claude-main'));
    expect(screen.queryByRole('button', { name: '立即重试 Codex 备用账号' })).not.toBeInTheDocument();
  });

  it('previews and atomically commits local, pasted, and multi-file imports', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.click(screen.getByRole('button', { name: /导入本机配置/ }));
    await waitFor(() => expect(apiMocks.previewAccountImport).toHaveBeenCalledWith({ local: true }));
    expect(await screen.findByText('确认导入预览')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '确认导入' }));
    await waitFor(() => expect(apiMocks.commitAccountImport).toHaveBeenCalledWith('import-session'));
    expect(await screen.findByText('账号导入完成：新增 1 个账号')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('账号类型'), { target: { value: 'claude' } });
    fireEvent.change(screen.getByLabelText('账号名称（可选）'), {
      target: { value: 'Claude 新账号' },
    });
    fireEvent.change(screen.getByLabelText('API Key 或 JSON'), {
      target: { value: 'sk-ant-secret-value' },
    });
    fireEvent.click(screen.getByRole('button', { name: '预览导入' }));
    await waitFor(() =>
      expect(apiMocks.previewAccountImport).toHaveBeenCalledWith({
        content: 'sk-ant-secret-value',
        tool: 'claude',
        name: 'Claude 新账号',
        source: 'json',
      })
    );
    expect(screen.queryByText('sk-ant-secret-value')).not.toBeInTheDocument();
    expect(screen.getByLabelText('API Key 或 JSON')).toHaveValue('');
    fireEvent.click(screen.getByRole('button', { name: '确认导入' }));
    await waitFor(() => expect(apiMocks.commitAccountImport).toHaveBeenCalledTimes(2));

    const first = new File(['{"OPENAI_API_KEY":"first"}'], 'first.json', { type: 'application/json' });
    const second = new File(['{"OPENAI_API_KEY":"second"}'], 'second.json', { type: 'application/json' });
    Object.defineProperty(first, 'text', { value: vi.fn().mockResolvedValue('{"OPENAI_API_KEY":"first"}') });
    Object.defineProperty(second, 'text', { value: vi.fn().mockResolvedValue('{"OPENAI_API_KEY":"second"}') });
    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [first, second] },
    });
    await waitFor(() =>
      expect(apiMocks.previewAccountImport).toHaveBeenLastCalledWith({
        contents: ['{"OPENAI_API_KEY":"first"}', '{"OPENAI_API_KEY":"second"}'],
        source: 'file',
      })
    );
    expect(apiMocks.previewAccountImport).toHaveBeenCalledTimes(3);
    fireEvent.click(screen.getByRole('button', { name: '确认导入' }));
    await waitFor(() => expect(apiMocks.commitAccountImport).toHaveBeenCalledTimes(3));
    expect(apiMocks.importAccounts).not.toHaveBeenCalled();
    expect(apiMocks.importLocalAccounts).not.toHaveBeenCalled();
  });

  it('rejects a failed multi-file preview without partially committing or refreshing', async () => {
    apiMocks.previewAccountImport.mockRejectedValueOnce(new Error('第二个文件格式无效'));
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const first = new File(['{}'], 'first.json', { type: 'application/json' });
    const second = new File(['{}'], 'second.json', { type: 'application/json' });
    const third = new File(['{}'], 'third.json', { type: 'application/json' });
    Object.defineProperty(first, 'text', { value: vi.fn().mockResolvedValue('{}') });
    Object.defineProperty(second, 'text', { value: vi.fn().mockResolvedValue('{}') });
    Object.defineProperty(third, 'text', { value: vi.fn().mockResolvedValue('{}') });
    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [first, second, third] },
    });

    expect(await screen.findByRole('alert')).toHaveTextContent('第二个文件格式无效');
    expect(apiMocks.previewAccountImport).toHaveBeenCalledWith({
      contents: ['{}', '{}', '{}'],
      source: 'file',
    });
    expect(apiMocks.commitAccountImport).not.toHaveBeenCalled();
    expect(apiMocks.listAccounts).toHaveBeenCalledTimes(1);
  });

  it('rejects an oversized file batch before reading or previewing any file', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const oversized = new File(['{}'], 'oversized.json', { type: 'application/json' });
    const oversizedText = vi.fn().mockResolvedValue('{}');
    Object.defineProperty(oversized, 'size', { value: 8 * 1024 * 1024 + 1 });
    Object.defineProperty(oversized, 'text', { value: oversizedText });
    const small = new File(['{}'], 'small.json', { type: 'application/json' });
    const smallText = vi.fn().mockResolvedValue('{}');
    Object.defineProperty(small, 'text', { value: smallText });

    fireEvent.change(screen.getByLabelText('选择 JSON 文件'), {
      target: { files: [oversized, small] },
    });

    expect(await screen.findByRole('alert')).toHaveTextContent(
      '所选文件合计过大（最大 8 MiB）'
    );
    expect(oversizedText).not.toHaveBeenCalled();
    expect(smallText).not.toHaveBeenCalled();
    expect(apiMocks.previewAccountImport).not.toHaveBeenCalled();
    expect(apiMocks.commitAccountImport).not.toHaveBeenCalled();
  });

  it('exports an encrypted backup without retaining its passphrase', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.click(screen.getByText('加密备份与恢复'));
    const exportPassphrase = document.querySelector<HTMLInputElement>(
      'input[aria-label="备份口令"]'
    );
    expect(exportPassphrase).not.toBeNull();
    fireEvent.change(exportPassphrase!, {
      target: { value: 'correct horse battery staple' },
    });
    fireEvent.change(screen.getByLabelText('确认备份口令'), {
      target: { value: 'correct horse battery staple' },
    });
    fireEvent.click(screen.getByRole('button', { name: '导出加密备份' }));

    await waitFor(() =>
      expect(apiMocks.exportAccountBackup).toHaveBeenCalledWith(
        'correct horse battery staple'
      )
    );
    expect(await screen.findByRole('status')).toHaveTextContent(
      '加密备份已导出：/tmp/accounts.tcarpool-backup'
    );
    expect(exportPassphrase).toHaveValue('');
    expect(screen.getByLabelText('确认备份口令')).toHaveValue('');
  });

  it('previews and commits a merge restore as one backend transaction', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');
    fireEvent.click(screen.getByText('加密备份与恢复'));

    fireEvent.change(screen.getByLabelText('恢复备份口令'), {
      target: { value: 'restore-passphrase' },
    });
    const backup = new File(['encrypted-backup'], 'accounts.tcarpool-backup', {
      type: 'application/json',
    });
    Object.defineProperty(backup, 'text', {
      value: vi.fn().mockResolvedValue('encrypted-backup'),
    });
    fireEvent.change(screen.getByLabelText('选择账号备份文件'), {
      target: { files: [backup] },
    });

    await waitFor(() =>
      expect(apiMocks.previewAccountRestore).toHaveBeenCalledWith({
        content: 'encrypted-backup',
        passphrase: 'restore-passphrase',
        mode: 'merge',
      })
    );
    expect(screen.getByLabelText('恢复备份口令')).toHaveValue('');
    fireEvent.click(screen.getByRole('button', { name: '确认合并恢复' }));
    await waitFor(() =>
      expect(apiMocks.commitAccountRestore).toHaveBeenCalledWith(
        'restore-session',
        'merge',
        false
      )
    );
    expect(await screen.findByRole('status')).toHaveTextContent(
      '账号恢复完成：更新 1 个账号'
    );
  });

  it('requires two confirmations before a replace restore', async () => {
    apiMocks.previewAccountRestore.mockResolvedValueOnce(
      restorePreview({ mode: 'replace', removeCount: 1 })
    );
    const confirm = vi
      .spyOn(window, 'confirm')
      .mockReturnValueOnce(true)
      .mockReturnValueOnce(true);
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');
    fireEvent.click(screen.getByText('加密备份与恢复'));

    fireEvent.change(screen.getByLabelText('恢复备份口令'), {
      target: { value: 'restore-passphrase' },
    });
    fireEvent.change(screen.getByLabelText('恢复方式'), {
      target: { value: 'replace' },
    });
    const backup = new File(['encrypted-backup'], 'accounts.tcarpool-backup');
    Object.defineProperty(backup, 'text', {
      value: vi.fn().mockResolvedValue('encrypted-backup'),
    });
    fireEvent.change(screen.getByLabelText('选择账号备份文件'), {
      target: { files: [backup] },
    });
    await screen.findByText('替换后将移除备份中不存在的 1 个本机账号。');

    fireEvent.click(screen.getByRole('button', { name: '确认替换恢复' }));
    expect(confirm).toHaveBeenNthCalledWith(
      1,
      '替换恢复会删除备份中不存在的本机账号，是否继续？'
    );
    expect(confirm).toHaveBeenNthCalledWith(
      2,
      '再次确认：将先创建回滚快照，然后替换整个本机账号池。'
    );
    await waitFor(() =>
      expect(apiMocks.commitAccountRestore).toHaveBeenCalledWith(
        'restore-session',
        'replace',
        true
      )
    );
  });

  it('blocks a conflicted preview and cancels the one-time session', async () => {
    apiMocks.previewAccountImport.mockResolvedValueOnce(
      importPreview({
        items: [
          {
            itemId: 'conflict-item',
            tool: 'codex',
            authKind: 'oauth',
            name: '冲突账号',
            source: 'local',
            action: 'conflict',
          },
        ],
      })
    );
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.click(screen.getByRole('button', { name: /导入本机配置/ }));
    expect(await screen.findByRole('alert')).toHaveTextContent(
      '存在身份或来源冲突，账号池不会被修改'
    );
    expect(screen.getByRole('button', { name: '确认导入' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: '取消' }));
    await waitFor(() =>
      expect(apiMocks.cancelAccountImport).toHaveBeenCalledWith('import-session')
    );
    expect(apiMocks.commitAccountImport).not.toHaveBeenCalled();
  });

  it('accepts the backend maximum account priority', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    const priorityInput = screen.getByLabelText('Claude 主账号 优先级');
    expect(priorityInput).toHaveAttribute('max', '1000000');
    fireEvent.change(priorityInput, { target: { value: '1000000' } });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));

    await waitFor(() =>
      expect(apiMocks.updateAccount).toHaveBeenCalledWith({
        id: 'claude-main',
        name: 'Claude 主账号',
        priority: 1_000_000,
      })
    );
  });

  it('rejects a blank priority instead of treating it as highest priority', async () => {
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '' },
    });
    fireEvent.click(screen.getByRole('button', { name: '保存 Claude 主账号' }));

    expect(await screen.findByRole('alert')).toHaveTextContent(
      '优先级必须是 0—1000000 之间的整数'
    );
    expect(apiMocks.updateAccount).not.toHaveBeenCalled();
  });

  it('preserves dirty drafts while refreshing clean account rows', async () => {
    apiMocks.listAccounts
      .mockResolvedValueOnce(accounts)
      .mockResolvedValueOnce([
        accounts[0],
        { ...accounts[1], name: 'Codex 已更新', priority: 25, enabled: false },
      ]);
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Claude 主账号');

    fireEvent.change(screen.getByLabelText('Claude 主账号 名称'), {
      target: { value: 'Claude 未保存' },
    });
    fireEvent.change(screen.getByLabelText('Claude 主账号 优先级'), {
      target: { value: '2' },
    });
    fireEvent.click(screen.getByRole('checkbox', { name: '停用 Codex 备用账号' }));

    await waitFor(() => expect(apiMocks.listAccounts).toHaveBeenCalledTimes(2));
    await screen.findByDisplayValue('Codex 已更新');
    expect(screen.getByDisplayValue('Claude 未保存')).toBeInTheDocument();
    expect(screen.getByDisplayValue('2')).toBeInTheDocument();
    expect(screen.getByDisplayValue('Codex 已更新')).toBeInTheDocument();
    expect(screen.getByDisplayValue('25')).toBeInTheDocument();
  });

  it('keeps maintenance actions disabled until the initial account load completes', async () => {
    let resolveAccounts: (value: LocalAccountSummary[]) => void = () => undefined;
    apiMocks.listAccounts.mockReturnValueOnce(
      new Promise<LocalAccountSummary[]>(resolve => {
        resolveAccounts = resolve;
      })
    );
    render(<AccountManager onClose={vi.fn()} />);

    const localImport = screen.getByRole('button', { name: /导入本机配置/ });
    expect(localImport).toBeDisabled();
    fireEvent.click(localImport);
    expect(apiMocks.importLocalAccounts).not.toHaveBeenCalled();

    resolveAccounts(accounts);
    await screen.findByText('Claude 主账号');
    expect(localImport).toBeEnabled();
  });

  it('deletes an account only after confirmation', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(<AccountManager onClose={vi.fn()} />);
    await screen.findByText('Codex 备用账号');

    fireEvent.click(screen.getByRole('button', { name: '删除 Codex 备用账号' }));
    expect(confirm).toHaveBeenCalledWith('确认删除“Codex 备用账号”？此操作不会删除原始工具配置。');
    await waitFor(() => expect(apiMocks.deleteAccount).toHaveBeenCalledWith('codex-backup'));
  });
});
