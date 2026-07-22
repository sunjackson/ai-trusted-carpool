import '@testing-library/jest-dom/vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const debugMocks = vi.hoisted(() => ({
  clearBackendDebugLogs: vi.fn(),
  clearDebugLogs: vi.fn(),
  debugLog: vi.fn(),
  exportDiagnosticBundle: vi.fn(),
  getBackendDebugLogs: vi.fn(),
  getDebugLogs: vi.fn(),
  openDebugLogDirectory: vi.fn(),
  subscribeToDebugLogs: vi.fn(),
}));

vi.mock('./debugLog', () => debugMocks);

import { DebugPanel } from './DebugPanel';

describe('DebugPanel', () => {
  beforeEach(() => {
    debugMocks.getDebugLogs.mockReturnValue([
      { id: 'front-1', timestamp: 100, level: 'info', source: 'App', message: '界面启动' },
      { id: 'front-2', timestamp: 200, level: 'error', source: 'Relay', message: '上游连接失败' },
    ]);
    debugMocks.getBackendDebugLogs.mockResolvedValue([
      { id: 'back-1', timestamp: 300, level: 'warn', source: 'Rust · router', message: '账号冷却中' },
    ]);
    debugMocks.subscribeToDebugLogs.mockReturnValue(() => undefined);
    debugMocks.clearBackendDebugLogs.mockResolvedValue(undefined);
    debugMocks.openDebugLogDirectory.mockResolvedValue('/tmp/diagnostic-logs');
    debugMocks.exportDiagnosticBundle.mockResolvedValue('/tmp/diagnostics.zip');
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('filters historical logs by level, source, and search text', async () => {
    render(<DebugPanel onClose={vi.fn()} />);

    expect(screen.getByText('界面启动')).toBeInTheDocument();
    expect(await screen.findByText('账号冷却中')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '错误' }));
    expect(screen.getByText('上游连接失败')).toBeInTheDocument();
    expect(screen.queryByText('界面启动')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '全部' }));
    fireEvent.change(screen.getByRole('combobox', { name: '日志来源' }), {
      target: { value: 'Rust · router' },
    });
    expect(screen.getByText('账号冷却中')).toBeInTheDocument();
    expect(screen.queryByText('上游连接失败')).not.toBeInTheDocument();

    fireEvent.change(screen.getByRole('combobox', { name: '日志来源' }), {
      target: { value: 'all' },
    });
    fireEvent.change(screen.getByRole('searchbox', { name: '搜索日志' }), {
      target: { value: '连接' },
    });
    expect(screen.getByText('上游连接失败')).toBeInTheDocument();
    expect(screen.queryByText('账号冷却中')).not.toBeInTheDocument();
  });

  it('runs the persistent log maintenance and diagnostic export actions', async () => {
    render(<DebugPanel onClose={vi.fn()} />);
    await screen.findByText('账号冷却中');

    fireEvent.click(screen.getByRole('button', { name: /清空/ }));
    await waitFor(() => expect(debugMocks.clearBackendDebugLogs).toHaveBeenCalledOnce());
    expect(debugMocks.clearDebugLogs).toHaveBeenCalledOnce();

    fireEvent.click(screen.getByRole('button', { name: /日志目录/ }));
    await waitFor(() => expect(debugMocks.openDebugLogDirectory).toHaveBeenCalledOnce());
    expect(screen.getByRole('status')).toHaveTextContent('已打开日志目录');

    fireEvent.click(screen.getByRole('button', { name: /导出诊断包/ }));
    await waitFor(() => expect(debugMocks.exportDiagnosticBundle).toHaveBeenCalledOnce());
    expect(screen.getByRole('status')).toHaveTextContent('诊断包已导出');
  });
});
