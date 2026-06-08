export type LogLevel = 'DEBUG' | 'INFO' | 'WARN' | 'ERROR';

export interface LogEntry {
  timestamp: string;
  level: LogLevel;
  file: string;
  line: number | null;
  message: string;
}

interface LogBatch {
  entries: LogEntry[];
}

/**
 * Walks the Error stack to find the first frame outside this file.
 * Works in Chrome/V8 and Firefox.
 */
function getCallerInfo(): { file: string; line: number | null } {
  const stack = new Error().stack;
  if (!stack) return { file: 'unknown', line: null };

  for (const frame of stack.split('\n').slice(1)) {
    // Skip frames that belong to logger.ts / logger.js
    if (frame.includes('logger.ts') || frame.includes('logger.js')) continue;

    // Chrome/V8: "    at Something (http://…/file.tsx:10:5)"
    // Firefox:   "Something@http://…/file.tsx:10:5"
    const chrome = frame.match(/\(([^)]+):(\d+):\d+\)/);
    const firefox = frame.match(/@([^@\s]+):(\d+):\d+\s*$/);
    const m = chrome ?? firefox;
    if (!m) continue;

    const rawPath = m[1];
    const line = parseInt(m[2], 10);
    // Keep only the filename (last segment of path/URL)
    const file = rawPath.split('/').pop() ?? rawPath;
    return { file, line };
  }

  return { file: 'unknown', line: null };
}

class Logger {
  private buffer: LogEntry[] = [];
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor() {
    this.timer = setInterval(() => { void this.flush(); }, 5000);
    window.addEventListener('beforeunload', () => this.flushBeacon());
  }

  private emit(level: LogLevel, message: string): void {
    const { file, line } = getCallerInfo();
    const entry: LogEntry = {
      timestamp: new Date().toISOString(),
      level,
      file,
      line,
      message: String(message),
    };

    const tag = `[${entry.timestamp}] [${level}] ${file}:${line ?? '?'}`;
    switch (level) {
      case 'DEBUG': console.debug(tag, message); break;
      case 'INFO':  console.info(tag, message);  break;
      case 'WARN':  console.warn(tag, message);  break;
      case 'ERROR': console.error(tag, message); break;
    }

    this.buffer.push(entry);
    if (this.buffer.length >= 50) void this.flush();
  }

  debug(message: string): void { this.emit('DEBUG', message); }
  info(message: string):  void { this.emit('INFO',  message); }
  warn(message: string):  void { this.emit('WARN',  message); }
  error(message: string): void { this.emit('ERROR', message); }

  private async flush(): Promise<void> {
    if (this.buffer.length === 0) return;
    const batch: LogBatch = { entries: this.buffer.splice(0) };
    try {
      await fetch('/api/logs/frontend', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(batch),
        keepalive: true,
      });
    } catch {
      // Silently drop — logs already visible in console
    }
  }

  /** Synchronous last-resort flush on page unload via sendBeacon. */
  private flushBeacon(): void {
    if (this.buffer.length === 0) return;
    const batch: LogBatch = { entries: this.buffer.splice(0) };
    navigator.sendBeacon(
      '/api/logs/frontend',
      new Blob([JSON.stringify(batch)], { type: 'application/json' }),
    );
  }

  destroy(): void {
    if (this.timer !== null) clearInterval(this.timer);
    this.flushBeacon();
  }
}

export const logger = new Logger();
