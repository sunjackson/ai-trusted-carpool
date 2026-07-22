import { CheckCircle2, Download, ExternalLink, LoaderCircle, RefreshCw, ShieldCheck, X } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { createPortal } from 'react-dom';

import {
  checkAppUpdate,
  checkSignedAppUpdate,
  downloadAppUpdate,
  installAppUpdate,
  openReleasesPage,
  restartAfterAppUpdate,
} from './api';
import { debugLog } from './debugLog';
import type {
  AppUpdateDownloadProgress,
  AppUpdateInfo,
  SignedAppUpdateInfo,
} from './types';

type UpdatePhase = 'checking' | 'available' | 'downloading' | 'downloaded' | 'installing' | 'restartRequired' | 'manual' | 'error' | 'hidden';

const messageFrom = (error: unknown): string =>
  error instanceof Error ? error.message : String(error);

const formatBytes = (bytes: number): string => {
  if (bytes < 1024 * 1024) return `${Math.max(0, Math.round(bytes / 1024))} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(bytes >= 100 * 1024 * 1024 ? 0 : 1)} MB`;
};

const progressPercent = ({ downloadedBytes, totalBytes }: AppUpdateDownloadProgress): number | null =>
  totalBytes && totalBytes > 0
    ? Math.min(100, Math.round((downloadedBytes / totalBytes) * 100))
    : null;

export function AppUpdater({ rideActive }: { rideActive: boolean }) {
  const [phase, setPhase] = useState<UpdatePhase>('checking');
  const [signedUpdate, setSignedUpdate] = useState<SignedAppUpdateInfo | null>(null);
  const [manualUpdate, setManualUpdate] = useState<AppUpdateInfo | null>(null);
  const [progress, setProgress] = useState<AppUpdateDownloadProgress>({
    event: 'started',
    downloadedBytes: 0,
    totalBytes: null,
  });
  const [dialogOpen, setDialogOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const checkForUpdate = useCallback(async () => {
    setPhase('checking');
    setError(null);
    try {
      const update = await checkSignedAppUpdate();
      if (update) {
        setSignedUpdate(update);
        setManualUpdate(null);
        setPhase(update.installSupported ? 'available' : 'manual');
        setDialogOpen(true);
        return;
      }
    } catch (signedError) {
      debugLog('warn', 'Updater', `签名更新检查失败：${messageFrom(signedError)}`);
      setError('签名更新暂时不可用，当前版本未改变。');
    }

    try {
      const fallback = await checkAppUpdate();
      if (fallback) {
        setManualUpdate(fallback);
        setSignedUpdate(null);
        setPhase('manual');
        setDialogOpen(true);
        return;
      }
    } catch (fallbackError) {
      debugLog('warn', 'Updater', `官方发布页版本检查失败：${messageFrom(fallbackError)}`);
    }

    setPhase('hidden');
    setError(null);
  }, []);

  useEffect(() => {
    void checkForUpdate();
  }, [checkForUpdate]);

  const startDownload = async () => {
    setPhase('downloading');
    setError(null);
    setProgress({ event: 'started', downloadedBytes: 0, totalBytes: null });
    try {
      const result = await downloadAppUpdate(next => setProgress(next));
      setSignedUpdate(result.update);
      setProgress({
        event: 'finished',
        downloadedBytes: result.downloadedBytes,
        totalBytes: result.totalBytes,
      });
      setPhase('downloaded');
    } catch (downloadError) {
      const message = messageFrom(downloadError);
      debugLog('error', 'Updater', `应用更新下载失败：${message}`);
      setError(`更新下载或签名校验失败：${message}`);
      setPhase('error');
    }
  };

  const installAndRestart = async () => {
    if (rideActive) return;
    setPhase('installing');
    setError(null);
    try {
      await installAppUpdate();
    } catch (installError) {
      const message = messageFrom(installError);
      debugLog('error', 'Updater', `应用更新安装失败：${message}`);
      setError(`安装失败，当前版本保持不变：${message}`);
      setPhase('downloaded');
      return;
    }

    try {
      await restartAfterAppUpdate();
    } catch (restartError) {
      const message = messageFrom(restartError);
      debugLog('error', 'Updater', `应用更新已安装但自动重启失败：${message}`);
      setError(`更新已安装，但自动重启失败：${message}`);
      setPhase('restartRequired');
    }
  };

  const restartApp = async () => {
    setError(null);
    try {
      await restartAfterAppUpdate();
    } catch (restartError) {
      const message = messageFrom(restartError);
      debugLog('error', 'Updater', `应用自动重启失败：${message}`);
      setError(`自动重启失败，请手动退出并重新打开应用：${message}`);
    }
  };

  const openManualRelease = async () => {
    try {
      await openReleasesPage();
    } catch (openError) {
      const message = messageFrom(openError);
      debugLog('error', 'Updater', `打开官方发布页失败：${message}`);
      setError(`无法打开官方发布页：${message}`);
    }
  };

  const version = signedUpdate?.version ?? manualUpdate?.latestVersion ?? '';
  const percent = useMemo(() => progressPercent(progress), [progress]);
  const visible = phase !== 'hidden' && phase !== 'checking';

  if (!visible) return null;

  const downloaded = phase === 'downloaded' || phase === 'installing';
  const restartRequired = phase === 'restartRequired';
  const manual = phase === 'manual';
  const pillLabel = restartRequired
    ? `更新 ${version} 已安装`
    : downloaded
      ? `更新 ${version} 已下载`
      : `新版本 ${version}`;

  return (
    <>
      <button className="update-pill" onClick={() => setDialogOpen(true)} aria-label={pillLabel}>
        {downloaded || restartRequired ? <CheckCircle2 size={13} /> : <Download size={13} />} {pillLabel}
      </button>
      {dialogOpen && createPortal(
        <div className="app-update-backdrop" role="presentation">
          <section className="app-update-dialog" role="dialog" aria-modal="true" aria-labelledby="app-update-title">
            <header className="app-update-dialog__header">
              <span className="app-update-dialog__icon">
                {manual ? <ExternalLink size={22} /> : <ShieldCheck size={22} />}
              </span>
              <div>
                <span>{manual ? '手动更新提醒' : '可信签名更新'}</span>
                <h2 id="app-update-title">发现新版本 {version}</h2>
              </div>
              <button className="dialog-close" onClick={() => setDialogOpen(false)} aria-label="关闭更新提示">
                <X size={17} />
              </button>
            </header>

            {signedUpdate?.notes && <p className="app-update-dialog__notes">{signedUpdate.notes}</p>}

            {phase === 'manual' && (
              <div className="app-update-status app-update-status--manual">
                <ExternalLink size={18} />
                <p>
                  <strong>此安装格式保持手动更新</strong>
                  <span>{error ?? 'macOS 尚未启用公证自动安装；DEB 继续交由系统包管理器处理。'}</span>
                </p>
              </div>
            )}

            {(phase === 'downloading' || phase === 'error') && (
              <div className="app-update-progress" aria-live="polite">
                <div className="app-update-progress__label">
                  <span>{phase === 'error' ? '下载未完成' : '正在下载并验证签名'}</span>
                  <strong>{percent === null ? formatBytes(progress.downloadedBytes) : `${percent}%`}</strong>
                </div>
                <div className="app-update-progress__track">
                  <i style={{ width: `${percent ?? (progress.downloadedBytes > 0 ? 18 : 4)}%` }} />
                </div>
                <small>
                  {formatBytes(progress.downloadedBytes)}
                  {progress.totalBytes ? ` / ${formatBytes(progress.totalBytes)}` : ''}
                </small>
              </div>
            )}

            {downloaded && (
              <div className={`app-update-status${rideActive ? ' app-update-status--blocked' : ''}`} role="status">
                {phase === 'installing' ? <LoaderCircle className="spin" size={18} /> : <CheckCircle2 size={18} />}
                <p>
                  <strong>{rideActive ? '更新已下载，结束拼车后安装' : '签名已验证，可以安装'}</strong>
                  <span>{rideActive ? '发车或上车期间不会重启应用；安装限制由后端再次强制检查。' : '安装后应用会重启；失败时继续保留当前版本。'}</span>
                </p>
              </div>
            )}

            {restartRequired && (
              <div className={`app-update-status${rideActive ? ' app-update-status--blocked' : ''}`} role="status">
                <CheckCircle2 size={18} />
                <p>
                  <strong>{rideActive ? '更新已安装，结束拼车后重启' : '更新已安装，请重启完成切换'}</strong>
                  <span>{rideActive ? '后端会阻止活跃拼车期间重启，已安装状态不会丢失。' : '自动重启失败不会再次安装更新；可以重试启动，或手动退出后重新打开应用。'}</span>
                </p>
              </div>
            )}

            {error && phase !== 'manual' && <p className="app-update-error" role="alert">{error}</p>}

            <p className="app-update-dialog__security">
              {manual
                ? '手动更新不会在应用内下载或校验安装包；请只使用官方发布页，并核对发布说明中的 SHA256SUMS。'
                : '应用内更新只接受内置公钥验证通过的官方产物；不会上传账号、会话或请求内容。'}
            </p>

            <footer className="app-update-dialog__actions">
              <button className="secondary-button" onClick={() => setDialogOpen(false)}>稍后提醒</button>
              {phase === 'manual' && (
                <button className="primary-button" onClick={() => void openManualRelease()}>
                  <ExternalLink size={17} /> 打开官方发布页
                </button>
              )}
              {phase === 'available' && (
                <button className="primary-button" onClick={() => void startDownload()}>
                  <Download size={17} /> 下载更新
                </button>
              )}
              {phase === 'downloading' && (
                <button className="primary-button" disabled>
                  <LoaderCircle className="spin" size={17} /> 下载中
                </button>
              )}
              {phase === 'error' && (
                <>
                  <button className="secondary-button" onClick={() => void openManualRelease()}>
                    <ExternalLink size={17} /> 官方发布页
                  </button>
                  <button className="primary-button" onClick={() => void startDownload()}>
                    <RefreshCw size={17} /> 重新下载
                  </button>
                </>
              )}
              {downloaded && (
                <button
                  className="primary-button"
                  disabled={rideActive || phase === 'installing'}
                  onClick={() => void installAndRestart()}
                >
                  {phase === 'installing' ? <LoaderCircle className="spin" size={17} /> : <RefreshCw size={17} />}
                  {rideActive ? '结束拼车后安装' : phase === 'installing' ? '正在安装' : '安装并重启'}
                </button>
              )}
              {restartRequired && (
                <button
                  className="primary-button"
                  disabled={rideActive}
                  onClick={() => void restartApp()}
                >
                  <RefreshCw size={17} /> {rideActive ? '结束拼车后重启' : '重新启动'}
                </button>
              )}
            </footer>
          </section>
        </div>,
        document.body
      )}
    </>
  );
}
