import { CatalogError, CatalogErrorKind, UpstreamCatalog } from './model-catalog.models';

/** Catalog refresh-error vocabulary; deliberately separate from attempt `errorClass` labels. */
const ERROR_LABELS: Record<CatalogErrorKind, string> = {
  auth: 'auth failed',
  timeout: 'timed out',
  rateLimited: 'rate limited',
  upstream: 'upstream error',
  unexpectedResponse: 'unexpected response',
  other: 'failed'
};

export type DisplayState = 'not_loaded' | 'loading' | 'ok' | 'stale' | 'error' | 'unsupported';

const STATE_LABELS: Record<DisplayState, string> = {
  not_loaded: 'NOT LOADED',
  loading: 'LOADING',
  ok: 'OK',
  stale: 'STALE',
  error: 'ERROR',
  unsupported: 'NOT SUPPORTED'
};

/** Glyphs accompany the text label so state never relies on color alone. */
const STATE_GLYPHS: Record<DisplayState, string> = {
  not_loaded: '○',
  loading: '…',
  ok: '✓',
  stale: '◷',
  error: '✕',
  unsupported: '–'
};

export function catalogErrorLabel(error: CatalogError): string {
  const label = ERROR_LABELS[error.kind] ?? ERROR_LABELS.other;
  return error.status === null ? label : `${label} (${error.status})`;
}

/** A `loading` entry never attempted and not refreshing is "not loaded", not a spinner. */
export function upstreamDisplayState(entry: UpstreamCatalog): DisplayState {
  if (entry.state === 'loading') {
    return !entry.refreshing && entry.lastAttemptAt === null ? 'not_loaded' : 'loading';
  }
  return entry.state;
}

export function catalogStateLabel(state: DisplayState): string {
  return STATE_LABELS[state];
}

export function catalogStateGlyph(state: DisplayState): string {
  return STATE_GLYPHS[state];
}

export function formatAge(iso: string, nowMs: number): string {
  const secs = Math.max(0, Math.floor((nowMs - Date.parse(iso)) / 1000));
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}
