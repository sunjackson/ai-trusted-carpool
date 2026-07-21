import { invoke as tauriInvoke } from '@tauri-apps/api/core';
import { debugLog } from './debugLog';

const messageFrom = (error: unknown): string =>
  error instanceof Error ? error.stack || error.message : String(error);

export async function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const startedAt = performance.now();
  debugLog('debug', 'Tauri', `调用 ${command}`);
  try {
    const result = await tauriInvoke<T>(command, args);
    debugLog('info', 'Tauri', `${command} 完成 · ${Math.round(performance.now() - startedAt)} ms`);
    return result;
  } catch (error) {
    debugLog(
      'error',
      'Tauri',
      `${command} 失败 · ${Math.round(performance.now() - startedAt)} ms\n${messageFrom(error)}`
    );
    throw error;
  }
}
