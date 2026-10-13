import { catalogErrorLabel, catalogStateLabel, upstreamDisplayState, formatAge } from './catalog-labels';
import { UpstreamCatalog } from './model-catalog.models';

const base: UpstreamCatalog = {
  state: 'ok', models: [], totalCount: 0, truncated: false, refreshedAt: null,
  lastAttemptAt: null, error: null, refreshing: false
};

describe('catalog-labels', () => {
  [
    ['auth', 'auth failed'], ['timeout', 'timed out'], ['rateLimited', 'rate limited'],
    ['upstream', 'upstream error'], ['other', 'failed'], ['unexpectedResponse', 'unexpected response']
  ].forEach(([kind, label]) => {
    it(`catalogErrorLabel_should_map_${kind}_to_${label}`, () => {
      expect(catalogErrorLabel({ kind: kind as any, status: null, message: '' })).toBe(label);
    });
  });

  it('catalogErrorLabel_should_append_status_when_present', () => {
    expect(catalogErrorLabel({ kind: 'auth', status: 403, message: '' })).toBe('auth failed (403)');
  });

  it('upstreamDisplayState_should_distinguish_not_loaded_from_loading', () => {
    expect(upstreamDisplayState({ ...base, state: 'loading' })).toBe('not_loaded');
    expect(upstreamDisplayState({ ...base, state: 'loading', refreshing: true })).toBe('loading');
    expect(upstreamDisplayState({ ...base, state: 'loading', lastAttemptAt: 'x' })).toBe('loading');
    expect(upstreamDisplayState({ ...base, state: 'stale' })).toBe('stale');
  });

  it('catalogStateLabel_should_use_uppercase_text_labels', () => {
    expect(catalogStateLabel('not_loaded')).toBe('NOT LOADED');
    expect(catalogStateLabel('unsupported')).toBe('NOT SUPPORTED');
    expect(catalogStateLabel('stale')).toBe('STALE');
    expect(catalogStateLabel('error')).toBe('ERROR');
    expect(catalogStateLabel('ok')).toBe('OK');
    expect(catalogStateLabel('loading')).toBe('LOADING');
  });

  it('formatAge_should_pick_the_largest_whole_unit', () => {
    const now = Date.parse('2026-10-10T18:00:00Z');
    expect(formatAge('2026-10-10T17:59:40Z', now)).toBe('20s ago');
    expect(formatAge('2026-10-10T17:57:00Z', now)).toBe('3m ago');
    expect(formatAge('2026-10-10T16:00:00Z', now)).toBe('2h ago');
    expect(formatAge('2026-10-08T18:00:00Z', now)).toBe('2d ago');
    expect(formatAge('2026-10-10T18:00:05Z', now)).toBe('0s ago');
  });
});
