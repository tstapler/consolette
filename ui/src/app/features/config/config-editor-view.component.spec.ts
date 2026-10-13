import { TestBed } from '@angular/core/testing';
import { ConfigEditorViewComponent } from './config-editor-view.component';
import { ConfigStore } from '../../core/stores/config.store';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';

describe('ConfigEditorViewComponent (TC-UNIT-32)', () => {
  let store: ConfigStore;
  let httpMock: HttpTestingController;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [ConfigEditorViewComponent],
      providers: [
        ConfigStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    }).compileComponents();

    store = TestBed.inject(ConfigStore);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('TC-UNIT-32: should trigger Apply Config action and handle notifications', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    fixture.detectChanges();

    const getReq = httpMock.expectOne('/v1/dashboard/config');
    getReq.flush({
      webUi: 'angular',
      fallbackCascade: ['anthropic', 'openai']
    });
    fixture.detectChanges();

    const component = fixture.componentInstance;
    component.onApplyConfig();

    const putReq = httpMock.expectOne('/v1/dashboard/config');
    expect(putReq.request.method).toBe('PUT');
    putReq.flush({
      webUi: 'angular',
      fallbackCascade: ['anthropic', 'openai']
    });
    fixture.detectChanges();

    expect(component.showToast()).toBeTrue();
  });

  function load(fixture: any, body: object): void {
    fixture.detectChanges();
    httpMock.expectOne('/v1/dashboard/config').flush(body);
    fixture.detectChanges();
  }

  it('editor_should_disable_rate_limit_inputs_and_show_empty_state_without_invented_providers', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    load(fixture, { webUi: 'angular', rateLimits: { maxDelayMs: 2000 } });

    const el = fixture.nativeElement as HTMLElement;
    for (const id of ['input-rpm', 'input-tpm', 'input-concurrent']) {
      const input = el.querySelector(`[data-testid="${id}"]`) as HTMLInputElement;
      expect(input.disabled).withContext(id).toBeTrue();
    }
    const text = el.textContent ?? '';
    expect(text).toContain('Rate limits are configured in the route file; editing is not supported here');
    expect(text).toContain('No providers configured');
    expect(text).not.toContain('anthropic');
    expect(text).not.toContain('openai');
    expect(fixture.componentInstance.cascadeItems()).toEqual([]);
  });

  it('should PUT only what changed: no unchanged baseUrl or key, no rate limits (E-15)', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    load(fixture, {
      webUi: 'angular',
      providers: {
        legacy: { apiKey: 'sk-...abcd', baseUrl: 'http://old.example/v1', weight: 50 },
        modern: { apiKey: 'sk-...wxyz', baseUrl: 'https://api.example/v1', weight: 50 }
      },
      rateLimits: { onBreach: 'shed', maxDelayMs: 2000 }
    });
    const c = fixture.componentInstance;
    c.onProvidersChange(
      c.providerItems().map((p) => (p.name === 'modern' ? { ...p, weight: 70, baseUrl: 'https://new.example/v1' } : p))
    );
    c.onApplyConfig();

    const put = httpMock.expectOne('/v1/dashboard/config');
    expect(put.request.body.providers).toEqual({
      legacy: {},
      modern: { weight: 70, baseUrl: 'https://new.example/v1' }
    });
    expect(put.request.body.rateLimits).toBeUndefined();
    put.flush({ webUi: 'angular' });
  });

  it('should send a typed key without the mask and omit the key when untouched (E-16)', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    load(fixture, { webUi: 'angular', providers: { a: { apiKey: 'sk-...abcd', weight: 100 } } });
    const c = fixture.componentInstance;
    const input = (fixture.nativeElement as HTMLElement).querySelector(
      '[data-testid="apikey-a"]'
    ) as HTMLInputElement;
    expect(input.value).toBe('');
    expect(input.placeholder).toBe('sk-...abcd');

    input.value = 'sk-rotated-123';
    input.dispatchEvent(new Event('input'));
    c.onApplyConfig();

    const put = httpMock.expectOne('/v1/dashboard/config');
    expect(put.request.body.providers.a.apiKey).toBe('sk-rotated-123');
    put.flush({ webUi: 'angular' });
  });

  it('should show the server reason, not the raw Angular message, when Apply fails (E-15)', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    load(fixture, { webUi: 'angular', providers: { a: { weight: 100 } } });
    fixture.componentInstance.onApplyConfig();
    httpMock
      .expectOne('/v1/dashboard/config')
      .flush({ error: 'a: Scheme must be HTTPS' }, { status: 400, statusText: 'Bad Request' });
    fixture.detectChanges();

    const banner = (fixture.nativeElement as HTMLElement).querySelector('[data-testid="error-banner"]');
    expect(banner?.textContent).toContain('a: Scheme must be HTTPS');
    expect(banner?.textContent).not.toContain('Http failure response');
  });

  it('should show weights as not set on a fallback route instead of 0% (E-16)', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    load(fixture, { webUi: 'angular', providers: { a: {}, b: {} }, fallbackCascade: ['a', 'b'] });
    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).not.toContain('Total Weight: 0%');
    expect(text).toContain('Weights not set (fallback order)');
  });
});
