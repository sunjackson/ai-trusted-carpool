import { useEffect } from 'react';

import { markFrontendReady } from './api';
import { debugLog } from './debugLog';

export function StartupReady() {
  useEffect(() => {
    void markFrontendReady().catch(error => {
      debugLog(
        'error',
        'Startup',
        `无法确认界面启动状态：${error instanceof Error ? error.message : String(error)}`
      );
    });
  }, []);
  return null;
}
