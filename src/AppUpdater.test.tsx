import '@testing-library/jest-dom/vitest';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { AppUpdateDownloadResult, SignedAppUpdateInfo } from './types';

const {
  checkAppUpdateMock,
  checkSignedAppUpdateMock,
  downloadAppUpdateMock,
  installAppUpdateMock,
  openReleasesPageMock,
  restartAfterAppUpdateMock,
} = vi.hoisted(() => ({
  checkAppUpdateMock: vi.fn(),
  checkSignedAppUpdateMock: vi.fn(),
  downloadAppUpdateMock: vi.fn(),
  installAppUpdateMock: vi.fn(),
  openReleasesPageMock: vi.fn(),
  restartAfterAppUpdateMock: vi.fn(),
}));

vi.mock('./api', () => ({
  checkAppUpdate: checkAppUpdateMock,
  checkSignedAppUpdate: checkSignedAppUpdateMock,
  downloadAppUpdate: downloadAppUpdateMock,
  installAppUpdate: installAppUpdateMock,
  openReleasesPage: openReleasesPageMock,
  restartAfterAppUpdate: restartAfterAppUpdateMock,
}));

import { AppUpdater } from './AppUpdater';

const update: SignedAppUpdateInfo = {
  currentVersion: '0.0.4',
  version: '0.0.5',
  notes: '安全更新说明',
  date: '2026-07-22T00:00:00Z',
  installSupported: true,
  installBlockReason: null,
};

const downloaded: AppUpdateDownloadResult = {
  update,
  downloadedBytes: 240_000_000,
  totalBytes: 240_000_000,
};

describe('signed application updater', () => {
  beforeEach(() => {
    checkSignedAppUpdateMock.mockResolvedValue(update);
    checkAppUpdateMock.mockResolvedValue(null);
    downloadAppUpdateMock.mockResolvedValue(downloaded);
    installAppUpdateMock.mockResolvedValue(undefined);
    openReleasesPageMock.mockResolvedValue(undefined);
    restartAfterAppUpdateMock.mockResolvedValue(undefined);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
  });

  it('downloads with progress, verifies, then explicitly installs and relaunches', async () => {
    let finishDownload: (result: AppUpdateDownloadResult) => void = () => undefined;
    downloadAppUpdateMock.mockImplementation((onProgress: (value: unknown) => void) => {
      onProgress({
        event: 'progress',
        downloadedBytes: 96_000_000,
        totalBytes: 240_000_000,
      });
      return new Promise<AppUpdateDownloadResult>(resolve => {
        finishDownload = resolve;
      });
    });

    render(<AppUpdater rideActive={false} />);
    expect(await screen.findByRole('dialog', { name: '发现新版本 0.0.5' })).toBeInTheDocument();
    expect(screen.getByText('安全更新说明')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: '下载更新' }));
    expect(await screen.findByText('40%')).toBeInTheDocument();
    expect(downloadAppUpdateMock).toHaveBeenCalledTimes(1);

    act(() => finishDownload(downloaded));
    const install = await screen.findByRole('button', { name: '安装并重启' });
    expect(screen.getByText('签名已验证，可以安装')).toBeInTheDocument();
    fireEvent.click(install);

    await waitFor(() => expect(installAppUpdateMock).toHaveBeenCalledTimes(1));
    expect(restartAfterAppUpdateMock).toHaveBeenCalledTimes(1);
  });

  it('keeps a verified download pending while a ride is active', async () => {
    const view = render(<AppUpdater rideActive />);
    fireEvent.click(await screen.findByRole('button', { name: '下载更新' }));

    const blocked = await screen.findByRole('button', { name: '结束拼车后安装' });
    expect(blocked).toBeDisabled();
    expect(screen.getByText('更新已下载，结束拼车后安装')).toBeInTheDocument();
    expect(installAppUpdateMock).not.toHaveBeenCalled();

    view.rerender(<AppUpdater rideActive={false} />);
    expect(screen.getByRole('button', { name: '安装并重启' })).toBeEnabled();
  });

  it('retains the downloaded state when installation fails', async () => {
    installAppUpdateMock.mockRejectedValue(new Error('活跃拼车期间禁止安装更新'));
    render(<AppUpdater rideActive={false} />);
    fireEvent.click(await screen.findByRole('button', { name: '下载更新' }));
    fireEvent.click(await screen.findByRole('button', { name: '安装并重启' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('当前版本保持不变');
    expect(screen.getByRole('button', { name: '安装并重启' })).toBeEnabled();
    expect(restartAfterAppUpdateMock).not.toHaveBeenCalled();
  });

  it('does not reinstall when the update succeeded but relaunch failed', async () => {
    restartAfterAppUpdateMock.mockRejectedValueOnce(new Error('relaunch denied'));
    const view = render(<AppUpdater rideActive={false} />);
    fireEvent.click(await screen.findByRole('button', { name: '下载更新' }));
    fireEvent.click(await screen.findByRole('button', { name: '安装并重启' }));

    expect(await screen.findByRole('alert')).toHaveTextContent('更新已安装，但自动重启失败');
    expect(screen.getByText('更新已安装，请重启完成切换')).toBeInTheDocument();
    expect(installAppUpdateMock).toHaveBeenCalledTimes(1);

    view.rerender(<AppUpdater rideActive />);
    expect(screen.getByText('更新已安装，结束拼车后重启')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '结束拼车后重启' })).toBeDisabled();

    view.rerender(<AppUpdater rideActive={false} />);
    restartAfterAppUpdateMock.mockResolvedValueOnce(undefined);
    fireEvent.click(screen.getByRole('button', { name: '重新启动' }));
    await waitFor(() => expect(restartAfterAppUpdateMock).toHaveBeenCalledTimes(2));
    expect(installAppUpdateMock).toHaveBeenCalledTimes(1);
  });

  it('falls back to the pinned release page when signed updates are unavailable', async () => {
    checkSignedAppUpdateMock.mockRejectedValue(new Error('manifest unavailable'));
    checkAppUpdateMock.mockResolvedValue({
      currentVersion: '0.0.4',
      latestVersion: 'v0.0.5',
      releaseUrl: 'https://malicious.example/ignored',
    });
    render(<AppUpdater rideActive={false} />);

    expect(await screen.findByText('签名更新暂时不可用，当前版本未改变。')).toBeInTheDocument();
    expect(screen.getByText('手动更新提醒')).toBeInTheDocument();
    expect(screen.queryByText('可信签名更新')).not.toBeInTheDocument();
    expect(screen.queryByText(/内置公钥验证通过/)).not.toBeInTheDocument();
    expect(screen.getByText(/手动更新不会在应用内下载或校验安装包/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '打开官方发布页' }));
    expect(openReleasesPageMock).toHaveBeenCalledTimes(1);
  });

  it('supports later reminder without discarding update state', async () => {
    render(<AppUpdater rideActive={false} />);
    const dialog = await screen.findByRole('dialog', { name: '发现新版本 0.0.5' });
    expect(dialog).toBeInTheDocument();
    fireEvent.click(screen.getByText('稍后提醒'));
    expect(screen.queryByRole('dialog', { name: '发现新版本 0.0.5' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '新版本 0.0.5' }));
    expect(screen.getByRole('dialog', { name: '发现新版本 0.0.5' })).toBeInTheDocument();
  });
});
