import { ComponentFixture, TestBed, fakeAsync, tick, flush } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';
import { provideRouter } from '@angular/router';
import { UpstreamModelsViewComponent } from './upstream-models-view.component';
import { ModelCatalogStore } from '../../core/stores/model-catalog.store';
import { MODELS_HELP_TEXT } from './models-help-text';
import fixture from '../../../testing/fixtures/api/model-catalog.json';

const URL = '/api/model-catalog';
const ago = (ms: number) => new Date(Date.now() - ms).toISOString();

function catalog(patch: (c: any) => void = () => undefined): any {
  const c = JSON.parse(JSON.stringify(fixture));
  c.upstreams.bedrock.refreshedAt = ago(2 * 3600_000);
  patch(c);
  return c;
}

function models(n: number, prefix = 'model'): any[] {
  return Array.from({ length: n }, (_, i) => ({ id: `${prefix}-${i}`, ownedBy: null }));
}

describe('UpstreamModelsViewComponent', () => {
  let fx: ComponentFixture<UpstreamModelsViewComponent>;
  let http: HttpTestingController;
  let el: HTMLElement;
  let es: any;

  beforeEach(async () => {
    es = { close: () => undefined, addEventListener: () => undefined };
    spyOn(window as any, 'EventSource').and.callFake(function () { return es; } as any);
    await TestBed.configureTestingModule({
      imports: [UpstreamModelsViewComponent],
      providers: [provideHttpClient(), provideHttpClientTesting(), provideRouter([])]
    }).compileComponents();
    http = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    // The view's MetricsStore backfills traces on construction; unrelated to what is asserted here.
    http.match('/v1/dashboard/traces?limit=50').forEach((r) => r.flush([]));
    http.verify();
  });

  function open(c: any = catalog()): void {
    fx = TestBed.createComponent(UpstreamModelsViewComponent);
    fx.detectChanges();
    el = fx.nativeElement;
    http.expectOne(URL).flush(c);
    fx.detectChanges();
  }

  const section = (name: string) => el.querySelector(`[data-upstream="${name}"]`) as HTMLElement;
  const text = (e: Element | null) => (e?.textContent ?? '').replace(/\s+/g, ' ').trim();

  it('open_should_send_exactly_one_GET_and_no_POST_and_show_skeleton_until_it_returns', () => {
    fx = TestBed.createComponent(UpstreamModelsViewComponent);
    fx.detectChanges();
    el = fx.nativeElement;
    expect(el.querySelector('[data-skeleton]')).toBeTruthy();
    http.expectOne(URL).flush(catalog());
    fx.detectChanges();
    expect(el.querySelector('[data-skeleton]')).toBeNull();
    http.expectNone(`${URL}/refresh`);
  });

  it('stale_upstream_header_should_read_state_count_age_and_error_and_keep_models_listed', () => {
    open();
    const h = text(section('bedrock').querySelector('[data-upstream-header]'));
    expect(h).toContain('bedrock');
    expect(h).toContain('STALE');
    expect(h).toContain('2 models');
    expect(h).toContain('refreshed 2h ago');
    expect(h).toContain('auth failed (403)');
    expect(section('bedrock').textContent).toContain('anthropic.claude-3-haiku-20240307-v1:0');
    expect(section('bedrock').querySelector('time')?.getAttribute('datetime')).toBeTruthy();
  });

  it('error_upstream_should_show_ERROR_and_Retry_and_never_0_models', () => {
    open();
    const s = section('gemini');
    expect(text(s)).toContain('ERROR');
    expect(text(s)).toContain('timed out');
    expect(text(s)).not.toContain('0 models');
    expect(text(s.querySelector('button[data-refresh]'))).toBe('Retry');
  });

  it('unsupported_upstream_should_show_NOT_SUPPORTED', () => {
    open();
    expect(text(section('vertex'))).toContain('NOT SUPPORTED');
    expect(text(section('vertex'))).not.toContain('0 models');
  });

  it('never_refreshed_upstream_should_read_not_loaded_prompt_not_spinner_or_zero', () => {
    open();
    const s = section('anthropic');
    expect(text(s)).toContain('NOT LOADED');
    expect(text(s)).toContain('Not loaded yet - press Refresh now');
    expect(text(s)).not.toContain('0 models');
    expect(s.querySelector('[data-spinner]')).toBeNull();
  });

  it('refreshing_upstream_should_show_spinner', () => {
    open();
    expect(section('openai').querySelector('[data-spinner]')).toBeTruthy();
  });

  it('healthy_empty_upstream_should_read_OK_and_upstream_reports_0_models_with_refresh', () => {
    open();
    const s = section('ollama');
    expect(text(s)).toContain('OK');
    expect(text(s)).toContain('Upstream reports 0 models.');
    expect(text(s.querySelector('button[data-refresh]'))).toBe('Refresh');
  });

  it('no_upstreams_should_show_message_and_config_editor_link', () => {
    open({ upstreams: {}, configWarnings: [] });
    expect(text(el)).toContain('No upstreams configured.');
    const a = Array.from(el.querySelectorAll('a')).find((x) => text(x) === 'Open Config Editor');
    expect(a?.getAttribute('href')).toBe('/config');
  });

  it('page_level_load_error_should_show_status_and_Retry_that_refetches', () => {
    fx = TestBed.createComponent(UpstreamModelsViewComponent);
    fx.detectChanges();
    el = fx.nativeElement;
    http.expectOne(URL).flush('x', { status: 503, statusText: 'x' });
    fx.detectChanges();
    expect(text(el)).toContain('Could not load the model catalog (503).');
    (el.querySelector('button[data-load-retry]') as HTMLButtonElement).click();
    http.expectOne(URL).flush(catalog());
    fx.detectChanges();
    expect(text(el)).not.toContain('Could not load the model catalog');
  });

  it('search_should_have_accessible_name_filter_ids_and_autoexpand_matching_sections', () => {
    const big = catalog((c) => {
      c.upstreams.openrouter = { ...c.upstreams.ollama, models: [...models(300, 'x'), { id: 'openai/gpt-4o', ownedBy: null }, { id: 'openai/gpt-4o-mini', ownedBy: null }], totalCount: 302 };
    });
    open(big);
    const input = el.querySelector('input[type="search"]') as HTMLInputElement;
    expect(input.getAttribute('aria-label')).toBe('Search models');
    expect(section('openrouter').querySelector('[data-model-list]')).toBeNull();
    input.value = 'gpt-4o';
    input.dispatchEvent(new Event('input'));
    fx.detectChanges();
    const ids = Array.from(section('openrouter').querySelectorAll('[data-model-id]')).map((n) => text(n));
    expect(ids).toEqual(['openai/gpt-4o', 'openai/gpt-4o-mini']);
    expect(section('openai').textContent).toContain('gpt-4o');
    expect(section('bedrock').querySelector('[data-model-id]')).toBeNull();
  });

  it('search_no_hit_should_show_message_and_Clear_search', () => {
    open();
    const input = el.querySelector('input[type="search"]') as HTMLInputElement;
    input.value = 'zzz';
    input.dispatchEvent(new Event('input'));
    fx.detectChanges();
    expect(text(el)).toContain("No models match 'zzz'.");
    (Array.from(el.querySelectorAll('button')).find((b) => text(b) === 'Clear search') as HTMLButtonElement).click();
    fx.detectChanges();
    expect(input.value).toBe('');
    expect(text(el)).not.toContain('No models match');
  });

  it('search_should_announce_N_matches_in_polite_status_region_after_300ms', fakeAsync(() => {
    open();
    const input = el.querySelector('input[type="search"]') as HTMLInputElement;
    input.value = 'gpt-4o';
    input.dispatchEvent(new Event('input'));
    fx.detectChanges();
    const region = el.querySelector('[data-search-status]') as HTMLElement;
    expect(region.getAttribute('role')).toBe('status');
    expect(region.getAttribute('aria-live')).toBe('polite');
    expect(text(region)).toBe('');
    tick(300);
    fx.detectChanges();
    expect(text(region)).toBe('2 matches');
    flush();
  }));

  it('large_list_should_render_at_most_200_rows_until_Show_more', () => {
    open(catalog((c) => {
      c.upstreams.ollama = { ...c.upstreams.ollama, models: models(310), totalCount: 310 };
    }));
    (section('ollama').querySelector('button[data-toggle]') as HTMLButtonElement)?.click();
    fx.detectChanges();
    expect(section('ollama').querySelectorAll('[data-model-id]').length).toBe(200);
    (section('ollama').querySelector('button[data-show-more]') as HTMLButtonElement).click();
    fx.detectChanges();
    expect(section('ollama').querySelectorAll('[data-model-id]').length).toBe(310);
  });

  it('disclosure_button_should_toggle_aria_expanded', () => {
    open(catalog((c) => {
      c.upstreams.ollama = { ...c.upstreams.ollama, models: models(60), totalCount: 60 };
    }));
    const btn = section('ollama').querySelector('button[data-toggle]') as HTMLButtonElement;
    expect(btn.getAttribute('aria-expanded')).toBe('false');
    btn.click();
    fx.detectChanges();
    expect(btn.getAttribute('aria-expanded')).toBe('true');
    expect(section('ollama').querySelector('[data-model-list]')).toBeTruthy();
  });

  it('truncated_list_should_say_how_many_are_shown', () => {
    open(catalog((c) => {
      c.upstreams.ollama = { ...c.upstreams.ollama, models: models(10), totalCount: 1500, truncated: true };
    }));
    expect(text(section('ollama'))).toContain('Showing the first 10 of 1500 models.');
  });

  it('refresh_row_button_should_POST_for_that_upstream_only_and_go_busy', () => {
    open();
    (section('bedrock').querySelector('button[data-refresh]') as HTMLButtonElement).click();
    const post = http.expectOne(`${URL}/refresh?upstream=bedrock`);
    const body = catalog((c) => { c.upstreams.bedrock.refreshing = true; });
    post.flush({ ...body, refreshing: true, coalesced: false, retryAfterSecs: null });
    fx.detectChanges();
    const b = section('bedrock').querySelector('button[data-refresh]') as HTMLButtonElement;
    expect(b.disabled).toBeTrue();
    expect(b.getAttribute('aria-busy')).toBe('true');
    expect(text(b)).toBe('Refreshing');
    const other = section('gemini').querySelector('button[data-refresh]') as HTMLButtonElement;
    expect(other.disabled).toBeFalse();
  });

  it('refresh_now_should_POST_all_and_coalesced_row_shows_already_running', () => {
    open();
    (el.querySelector('button[data-refresh-all]') as HTMLButtonElement).click();
    const post = http.expectOne(`${URL}/refresh`);
    const body = catalog((c) => { c.upstreams.bedrock.refreshing = true; });
    post.flush({ ...body, refreshing: true, coalesced: true, retryAfterSecs: null });
    fx.detectChanges();
    expect(text(section('bedrock'))).toContain('Refresh already running');
  });

  it('all_never_refreshed_should_lead_with_prominent_refresh_now', () => {
    const c = catalog((x) => {
      for (const k of Object.keys(x.upstreams)) {
        x.upstreams[k] = { ...x.upstreams.anthropic };
      }
    });
    open(c);
    const b = el.querySelector('button[data-refresh-all]') as HTMLElement;
    expect(b.hasAttribute('data-prominent')).toBeTrue();
    expect(text(el)).toContain(MODELS_HELP_TEXT.credentialFlow);
  });

  it('help_text_should_state_mode_credential_flow_and_exposure', () => {
    open();
    const t = text(el.querySelector('[data-help]'));
    expect(t).toContain(MODELS_HELP_TEXT.mode);
    expect(t).toContain(MODELS_HELP_TEXT.credentialFlow);
    expect(t).toContain(MODELS_HELP_TEXT.exposure);
    expect(MODELS_HELP_TEXT.mode).toContain('on_demand');
    expect(MODELS_HELP_TEXT.credentialFlow).toContain('SSO');
    expect(MODELS_HELP_TEXT.exposure).toContain('127.0.0.1');
    expect(MODELS_HELP_TEXT.exposure).toContain('no authentication');
    expect(MODELS_HELP_TEXT.exposure).toContain('any origin');
  });

  const CAVEAT = 'Only models pinned on the first route are checked.';

  it('warning_block_should_list_unlisted_pinned_models_with_count_and_caveat', () => {
    open();
    const w = el.querySelector('[data-config-warnings]') as HTMLElement;
    expect(text(w)).toContain('Referenced by config but not listed upstream (1)');
    expect(text(w)).toContain('openai');
    expect(text(w)).toContain('gpt-5-turbo');
    expect(text(w)).toContain('route default upstream 0');
    expect(text(w)).toContain(CAVEAT);
    expect(text(w)).toContain('so no warning here is not proof that every target exists');
  });

  it('warning_block_should_still_show_caveat_when_there_are_zero_warnings', () => {
    open(catalog((c) => { c.configWarnings = []; }));
    const w = el.querySelector('[data-config-warnings]') as HTMLElement;
    expect(text(w)).toContain('Referenced by config but not listed upstream (0)');
    expect(text(w)).toContain(CAVEAT);
  });
});
