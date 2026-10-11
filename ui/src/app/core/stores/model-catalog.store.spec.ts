import { TestBed, fakeAsync, tick, flush } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';
import { ModelCatalogStore } from './model-catalog.store';
import { MetricsStore } from './metrics.store';
import { SseService } from '../services/sse.service';
import { RefreshResponse } from '../models/model-catalog.models';
import fixture from '../../../testing/fixtures/api/model-catalog.json';
import changed from '../../../testing/fixtures/sse/model-catalog-changed.json';

const URL = '/api/model-catalog';

function catalog(overrides: Record<string, Partial<any>> = {}): any {
  const copy = JSON.parse(JSON.stringify(fixture));
  for (const [name, patch] of Object.entries(overrides)) {
    copy.upstreams[name] = { ...copy.upstreams[name], ...patch };
  }
  return copy;
}

describe('ModelCatalogStore', () => {
  let store: ModelCatalogStore;
  let http: HttpTestingController;
  let es: any[];

  beforeEach(() => {
    es = [];
    spyOn(window as any, 'EventSource').and.callFake(function (this: any, url: string) {
      const inst = { url, close: () => undefined, addEventListener: () => undefined, onopen: null, onmessage: null, onerror: null };
      es.push(inst);
      return inst;
    } as any);
    TestBed.configureTestingModule({ providers: [provideHttpClient(), provideHttpClientTesting()] });
    store = TestBed.inject(ModelCatalogStore);
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => http.verify());

  it('load_should_issue_one_GET_and_no_POST', () => {
    store.load();
    const req = http.expectOne(URL);
    expect(req.request.method).toBe('GET');
    req.flush(catalog());
    expect(store.catalog()?.upstreams['bedrock'].state).toBe('stale');
    expect(store.isLoading()).toBeFalse();
    http.expectNone(`${URL}/refresh`);
  });

  it('load_should_set_error_text_with_status_when_GET_fails', () => {
    store.load();
    http.expectOne(URL).flush('boom', { status: 503, statusText: 'x' });
    expect(store.loadError()).toBe('503');
    store.load();
    http.expectOne(URL).error(new ProgressEvent('error'));
    expect(store.loadError()).toBe('offline');
  });

  it('refresh_one_should_POST_with_upstream_param_and_keep_busy_after_response', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    const post = http.expectOne(`${URL}/refresh?upstream=bedrock`);
    expect(post.request.method).toBe('POST');
    const body: RefreshResponse = { ...catalog({ bedrock: { refreshing: true } }), refreshing: true, coalesced: false, retryAfterSecs: null };
    post.flush(body);
    expect(store.isBusy('bedrock')).toBeTrue();
    expect(store.isBusy('gemini')).toBeFalse();
    tick(14000);
    expect(store.isBusy('bedrock')).toBeTrue();
    flush();
  }));

  it('busy_should_clear_when_event_refetch_shows_refreshing_false', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    http.expectOne(`${URL}/refresh?upstream=bedrock`)
      .flush({ ...catalog({ bedrock: { refreshing: true } }), refreshing: true, coalesced: false, retryAfterSecs: null });
    store.onCatalogChanged();
    http.expectOne(URL).flush(catalog({ bedrock: { refreshing: false } }));
    expect(store.isBusy('bedrock')).toBeFalse();
    flush();
  }));

  it('busy_should_stay_when_refetch_still_shows_refreshing_true', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    http.expectOne(`${URL}/refresh?upstream=bedrock`)
      .flush({ ...catalog({ bedrock: { refreshing: true } }), refreshing: true, coalesced: false, retryAfterSecs: null });
    store.onCatalogChanged();
    http.expectOne(URL).flush(catalog({ bedrock: { refreshing: true } }));
    expect(store.isBusy('bedrock')).toBeTrue();
    flush();
  }));

  it('busy_should_clear_after_15s_safety_timeout_with_unknown_status_notice', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    http.expectOne(`${URL}/refresh?upstream=bedrock`)
      .flush({ ...catalog({ bedrock: { refreshing: true } }), refreshing: true, coalesced: false, retryAfterSecs: null });
    tick(15000);
    expect(store.isBusy('bedrock')).toBeFalse();
    expect(store.notice('bedrock')).toBe('Refresh status unknown');
  }));

  it('coalesced_response_should_show_running_notice_and_keep_busy_until_settled', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    http.expectOne(`${URL}/refresh?upstream=bedrock`)
      .flush({ ...catalog({ bedrock: { refreshing: true } }), refreshing: true, coalesced: true, retryAfterSecs: null });
    expect(store.isBusy('bedrock')).toBeTrue();
    expect(store.notice('bedrock')).toBe('Refresh already running');
    store.onCatalogChanged();
    http.expectOne(URL).flush(catalog());
    expect(store.isBusy('bedrock')).toBeFalse();
    expect(store.notice('bedrock')).toBeNull();
    flush();
  }));

  it('retryAfterSecs_response_should_clear_busy_at_once_with_try_again_notice', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('bedrock');
    http.expectOne(`${URL}/refresh?upstream=bedrock`)
      .flush({ ...catalog(), refreshing: false, coalesced: false, retryAfterSecs: 7 });
    expect(store.isBusy('bedrock')).toBeFalse();
    expect(store.notice('bedrock')).toBe('Try again in 7s');
    flush();
  }));

  it('refresh_all_should_POST_without_params_and_mark_every_refreshing_upstream_busy', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh();
    const post = http.expectOne(`${URL}/refresh`);
    expect(post.request.method).toBe('POST');
    post.flush({
      ...catalog({ bedrock: { refreshing: true }, gemini: { refreshing: true } }),
      refreshing: true, coalesced: false, retryAfterSecs: null
    });
    expect(store.isBusy('bedrock')).toBeTrue();
    expect(store.isBusy('gemini')).toBeTrue();
    expect(store.isBusy('ollama')).toBeFalse();
    flush();
  }));

  it('refresh_failure_should_clear_busy_with_failure_notice', fakeAsync(() => {
    store.load();
    http.expectOne(URL).flush(catalog());
    store.refresh('nope');
    http.expectOne(`${URL}/refresh?upstream=nope`).flush({ error: 'unknown upstream' }, { status: 404, statusText: 'x' });
    expect(store.isBusy('nope')).toBeFalse();
    expect(store.notice('nope')).toBe('Refresh failed (404)');
    flush();
  }));

  it('onCatalogChanged_should_not_fetch_before_the_page_was_opened', () => {
    store.onCatalogChanged();
    http.expectNone(URL);
  });

  it('metricsStore_should_dispatch_ModelCatalogChanged_to_one_refetch', () => {
    TestBed.inject(MetricsStore);
    store.load();
    http.expectOne(URL).flush(catalog());
    const sse = TestBed.inject(SseService);
    sse.connect({ url: '/test/events' });
    es[0].onmessage({ data: JSON.stringify(changed) });
    http.expectOne(URL).flush(catalog());
  });
});
