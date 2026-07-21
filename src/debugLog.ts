import { invoke as rawTauriInvoke } from '@tauri-apps/api/core';

export type DebugLogLevel = 'debug' | 'info' | 'warn' | 'error';

export type DebugLogEntry = {
  id: string;
  timestamp: number;
  level: DebugLogLevel;
  source: string;
  message: string;
};

type BackendDebugLogEntry = Omit<DebugLogEntry, 'id'> & { id: number };

const MAX_LOG_ENTRIES = 500;
const entries: DebugLogEntry[] = [];
const listeners = new Set<() => void>();
let nextId = 0;
let captureInstalled = false;

const REDACTED = '[REDACTED]';
const SENSITIVE_ASSIGNMENT_PATTERN = /((?:["'`]?)(?:access[_-]?token|refresh[_-]?token|id[_-]?token|api[_-]?key|x[_-]?api[_-]?key|openai[_-]?api[_-]?key|anthropic[_-]?api[_-]?key|authorization|client[_-]?secret|secret|password|credential(?:s)?|cookie|token)(?:["'`]?)\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|(?:bearer|basic)\s+[^\s,;}&\]]+|[^\s,;}&\]]+)/gi;
const AUTH_SCHEME_PATTERN = /\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]+/gi;
const API_KEY_PATTERN = /\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{8,}\b/gi;
const JWT_PATTERN = /\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b/g;

export const redactDebugMessage = (message: string): string =>
  message
    .replace(SENSITIVE_ASSIGNMENT_PATTERN, (_match, prefix: string, value: string) => {
      const quote = value.at(0);
      return `${prefix}${quote === '"' || quote === "'" || quote === '`' ? `${quote}${REDACTED}${quote}` : REDACTED}`;
    })
    .replace(AUTH_SCHEME_PATTERN, '$1 [REDACTED]')
    .replace(API_KEY_PATTERN, REDACTED)
    .replace(JWT_PATTERN, REDACTED);

const formatValue = (value: unknown): string => {
  if (value instanceof Error) return value.stack || `${value.name}: ${value.message}`;
  if (typeof value === 'string') return value;
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
};

const notify = () => listeners.forEach(listener => listener());

export function debugLog(
  level: DebugLogLevel,
  source: string,
  ...values: unknown[]
): void {
  entries.push({
    id: `frontend-${Date.now()}-${nextId++}`,
    timestamp: Date.now(),
    level,
    source,
    message: redactDebugMessage(values.map(formatValue).join(' ')),
  });
  if (entries.length > MAX_LOG_ENTRIES) entries.splice(0, entries.length - MAX_LOG_ENTRIES);
  notify();
}

export function getDebugLogs(): DebugLogEntry[] {
  return [...entries];
}

export function subscribeToDebugLogs(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function clearDebugLogs(): void {
  entries.length = 0;
  notify();
}

export function installDebugCapture(): void {
  if (captureInstalled) return;
  captureInstalled = true;

  (['debug', 'info', 'warn', 'error'] as const).forEach(level => {
    const original = console[level].bind(console);
    console[level] = (...values: unknown[]) => {
      debugLog(level, 'Console', ...values);
      original(...values);
    };
  });

  window.addEventListener('error', event => {
    debugLog('error', 'Window', event.error ?? `${event.message} (${event.filename}:${event.lineno})`);
  });
  window.addEventListener('unhandledrejection', event => {
    debugLog('error', 'Promise', event.reason);
  });

  debugLog('info', 'App', `界面启动 · ${navigator.userAgent}`);
}

const inTauri = (): boolean => '__TAURI_INTERNALS__' in window;

export async function getBackendDebugLogs(): Promise<DebugLogEntry[]> {
  if (!inTauri()) return [];
  const backendEntries = await rawTauriInvoke<BackendDebugLogEntry[]>('get_debug_logs');
  return backendEntries.map(entry => ({
    ...entry,
    id: `backend-${entry.id}`,
    source: `Rust · ${entry.source}`,
    message: redactDebugMessage(entry.message),
  }));
}

export async function clearBackendDebugLogs(): Promise<void> {
  if (!inTauri()) return;
  await rawTauriInvoke('clear_debug_logs');
}
