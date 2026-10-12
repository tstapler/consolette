import { signal } from '@angular/core';
import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { provideRouter } from '@angular/router';
import { routes } from './app.routes';
import { RecentTraceTableComponent } from './features/overview/components/recent-trace-table.component';
import { InspectDrawerComponent } from './features/overview/components/inspect-drawer.component';
import { UpstreamModelsViewComponent } from './features/models/upstream-models-view.component';
import { ConfigEditorViewComponent } from './features/config/config-editor-view.component';
import { SessionListComponent } from './features/sessions/components/session-list.component';
import { ModelBenchmarksViewComponent } from './features/benchmarks/model-benchmarks-view.component';
import { MetricKpiCardComponent } from './features/overview/components/metric-kpi-card.component';
import { ProviderHealthGridComponent } from './features/overview/components/provider-health-grid.component';
import { AppShellComponent } from './core/components/app-shell.component';
import { SseService } from './core/services/sse.service';
import { RequestTraceData, SessionData } from './core/models/telemetry.models';
import {
  expectAccessibleNames, expectContrast, expectFocusRing, expectForcedColorsSafe, expectNonTextContrast,
  expectReducedMotionSafe, expectTargetSize,
} from '../testing/a11y-checks';
import traceSuccess from '../testing/fixtures/sse/request-trace-success.json';
import traceFailure from '../testing/fixtures/sse/request-trace-failure.json';
import catalogFixture from '../testing/fixtures/api/model-catalog.json';

const success = traceSuccess.data as RequestTraceData;
const failure = traceFailure.data as RequestTraceData;
const fallback: RequestTraceData = {
  ...success, requestId: 'req_fb_0001', id: 'req_fb_0001', routeReason: 'fallback', routedProvider: 'steady',
  attempts: [
    { iteration: 0, upstream: 'flaky', model: null, outcome: 'rate_limited', status: 429, latencyMs: 20, errorClass: 'rate_limited', errorMessage: null },
    { iteration: 0, upstream: 'steady', model: 'gpt-5.1', outcome: 'ok', status: 200, latencyMs: 80, errorClass: null, errorMessage: null },
  ],
};

/** All checks every surface has to pass. */
function expectAccessible(root: HTMLElement): void {
  expectAccessibleNames(root);
  expectContrast(root);
  expectFocusRing(root);
  expectTargetSize(root);
  expectReducedMotionSafe(root);
}

describe('accessibility (Story 4.1.7)', () => {
  describe('trace table', () => {
    let fixture: ComponentFixture<RecentTraceTableComponent>;
    beforeEach(async () => {
      await TestBed.configureTestingModule({ imports: [RecentTraceTableComponent] }).compileComponents();
      fixture = TestBed.createComponent(RecentTraceTableComponent);
    });
    const render = (status: 'connected' | 'reconnecting' | 'disconnected' = 'connected', traces = [success, failure, fallback]) => {
      fixture.componentInstance.traces = traces;
      fixture.componentInstance.connectionStatus = status;
      fixture.detectChanges();
      return fixture.nativeElement as HTMLElement;
    };

    it('connected_table_with_rows_passes_names_contrast_focus_ring_target_size_and_motion_checks', () => {
      expectAccessible(render());
    });
    it('empty_connected_and_disconnected_states_pass_contrast', () => {
      expectContrast(render('connected', []));
      expectContrast(render('disconnected', []));
    });
    it('dimmed_reconnecting_rows_keep_every_text_node_at_4_5_to_1', () => {
      const root = render('reconnecting');
      expect(root.querySelector('[data-rows-dimmed]')).toBeTruthy();
      expectContrast(root);
    });
  });

  describe('reflow at 320 px (WCAG 1.4.10)', () => {
    it('trace_table_scrolls_inside_one_named_focusable_region_and_does_not_widen_the_card', async () => {
      await TestBed.configureTestingModule({ imports: [RecentTraceTableComponent] }).compileComponents();
      const fixture = TestBed.createComponent(RecentTraceTableComponent);
      const host = fixture.nativeElement as HTMLElement;
      host.style.cssText = 'display:block;width:320px';
      fixture.componentInstance.traces = [success, failure, fallback];
      fixture.detectChanges();
      const region = host.querySelector('[role="region"]') as HTMLElement;
      expect(region.getAttribute('aria-label')).toBe('Live request traces, scroll horizontally');
      expect(region.tabIndex).toBe(0);
      expect(region.scrollWidth).toBeGreaterThan(region.clientWidth);
      expect(host.scrollWidth).withContext('the card itself must not overflow').toBeLessThanOrEqual(320);
    });

    it('models_list_wraps_a_120_character_model_id_without_horizontal_scroll', async () => {
      await TestBed.configureTestingModule({
        imports: [UpstreamModelsViewComponent],
        providers: [provideHttpClient(), provideHttpClientTesting(), provideRouter([])],
      }).compileComponents();
      spyOn(window as any, 'EventSource').and.callFake(function () { return { close: () => undefined, addEventListener: () => undefined }; } as any);
      const http = TestBed.inject(HttpTestingController);
      const fx = TestBed.createComponent(UpstreamModelsViewComponent);
      const host = fx.nativeElement as HTMLElement;
      host.style.cssText = 'display:block;width:320px';
      fx.detectChanges();
      const cat = JSON.parse(JSON.stringify(catalogFixture));
      cat.upstreams.anthropic.models = [{ id: 'm'.repeat(120), ownedBy: null }];
      cat.upstreams.anthropic.totalCount = 1;
      http.expectOne('/api/model-catalog').flush(cat);
      http.match('/v1/dashboard/traces?limit=50').forEach(r => r.flush([]));
      fx.detectChanges();
      expect(host.textContent).toContain('m'.repeat(120));
      expect(host.scrollWidth).toBeLessThanOrEqual(host.clientWidth);
    });
  });

  describe('inspect drawer (E-01)', () => {
    let fixture: ComponentFixture<InspectDrawerComponent>;
    let header: HTMLElement;
    beforeEach(async () => {
      await TestBed.configureTestingModule({ imports: [InspectDrawerComponent], providers: [provideRouter([])] }).compileComponents();
      // Same box as the app shell header: sticky, 64px, above the drawer in z-order.
      header = document.createElement('header');
      header.className = 'h-16 sticky top-0 z-50 bg-slate-900';
      header.textContent = 'app header';
      document.body.prepend(header);
      fixture = TestBed.createComponent(InspectDrawerComponent);
      fixture.detectChanges();
      fixture.componentInstance.traces = [fallback];
      fixture.componentInstance.open(fallback, null, false);
      fixture.detectChanges();
    });
    afterEach(() => { header.remove(); fixture.destroy(); });

    const el = () => fixture.nativeElement as HTMLElement;

    it('heading_copy_and_close_are_not_covered_by_the_app_header', () => {
      expect(document.elementFromPoint(5, 5)).withContext('replica header must sit at the top of the page').toBe(header);
      for (const sel of ['[data-drawer-heading]', '[data-drawer-copy]', '[data-drawer-close]']) {
        const target = el().querySelector(sel) as HTMLElement;
        const r = target.getBoundingClientRect();
        const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        expect(target.contains(hit)).withContext(`${sel} at y=${Math.round(r.top)} is hit-tested as ${hit?.tagName} ${hit?.className}`).toBeTrue();
      }
    });
    it('drawer_passes_names_contrast_focus_ring_target_size_motion_and_waterfall_bar_checks', () => {
      expectAccessible(el());
      expectNonTextContrast(el());
      expectForcedColorsSafe(el(), '');
      expect(el().querySelectorAll('[data-bar]').length).toBe(2);
    });
  });

  describe('models page', () => {
    let http: HttpTestingController;
    let fx: ComponentFixture<UpstreamModelsViewComponent>;
    const root = () => fx.nativeElement as HTMLElement;
    const statusText = () => (root().querySelector('[data-search-status]')?.textContent ?? '').trim();

    beforeEach(async () => {
      await TestBed.configureTestingModule({
        imports: [UpstreamModelsViewComponent],
        providers: [provideHttpClient(), provideHttpClientTesting(), provideRouter([])],
      }).compileComponents();
      spyOn(window as any, 'EventSource').and.callFake(function () { return { close: () => undefined, addEventListener: () => undefined }; } as any);
      http = TestBed.inject(HttpTestingController);
      fx = TestBed.createComponent(UpstreamModelsViewComponent);
      fx.detectChanges();
      http.expectOne('/api/model-catalog').flush(JSON.parse(JSON.stringify(catalogFixture)));
      http.match('/v1/dashboard/traces?limit=50').forEach(r => r.flush([]));
      fx.detectChanges();
    });

    it('models_page_passes_all_checks_and_pinned_warning_has_a_text_label', () => {
      expectAccessible(root());
      const warn = root().querySelector('[data-config-warnings] h2') as HTMLElement;
      expect(root().querySelectorAll('[data-config-warnings] li').length).toBeGreaterThan(0);
      const visibleText = Array.from(warn.childNodes).filter(n => !(n instanceof Element && n.getAttribute('aria-hidden') === 'true'))
        .map(n => n.textContent ?? '').join(' ').toLowerCase();
      expect(visibleText).toContain('warning');
    });

    it('refresh_now_announces_start_and_finish_in_the_status_region_E28', () => {
      expect(statusText()).toBe('');
      (root().querySelector('button[data-refresh-all]') as HTMLButtonElement).click();
      const running = JSON.parse(JSON.stringify(catalogFixture));
      Object.values<any>(running.upstreams).forEach(u => { u.refreshing = true; });
      http.expectOne('/api/model-catalog/refresh').flush({ ...running, refreshing: true, coalesced: false, retryAfterSecs: null });
      fx.detectChanges();
      expect(statusText()).toContain('Refreshing models');
      const done = JSON.parse(JSON.stringify(catalogFixture));
      Object.values<any>(done.upstreams).forEach(u => { u.refreshing = false; });
      (fx.componentInstance as any).store.onCatalogChanged();
      http.expectOne('/api/model-catalog').flush(done);
      fx.detectChanges();
      expect(statusText()).toMatch(/Refresh finished/);
    });
  });

  describe('config editor (E-08, E-10, E-11)', () => {
    it('config_editor_passes_all_checks_including_slider_and_input_focus_and_apply_contrast', async () => {
      await TestBed.configureTestingModule({
        imports: [ConfigEditorViewComponent], providers: [provideHttpClient(), provideHttpClientTesting()],
      }).compileComponents();
      const http = TestBed.inject(HttpTestingController);
      const fx = TestBed.createComponent(ConfigEditorViewComponent);
      fx.detectChanges();
      http.expectOne('/v1/dashboard/config').flush({
        webUi: 'angular',
        providers: { anthropic: { apiKey: '****abcd', baseUrl: 'https://api.anthropic.com', weight: 60 }, openai: { apiKey: '****wxyz', baseUrl: 'https://api.openai.com/v1', weight: 40 } },
        rateLimits: { rpm: 60 }, fallbackCascade: ['anthropic', 'openai'],
      });
      fx.detectChanges();
      const root = fx.nativeElement as HTMLElement;
      expect(root.querySelectorAll('input[type="range"]').length).toBe(2);
      expectAccessible(root);
    });
  });

  describe('session list (E-04)', () => {
    const sessions: SessionData[] = [{ id: 'sess-1', turnCount: 2, tokenSavingsPercent: 0, lastActiveTimestamp: '2026-10-12T10:00:00Z', pinned: false, model: 'claude-sonnet-4', provider: 'anthropic' }];
    let fx: ComponentFixture<SessionListComponent>;
    beforeEach(async () => {
      await TestBed.configureTestingModule({ imports: [SessionListComponent] }).compileComponents();
      fx = TestBed.createComponent(SessionListComponent);
      fx.componentInstance.sessions = sessions;
      fx.detectChanges();
    });
    it('session_rows_are_keyboard_selectable', () => {
      const root = fx.nativeElement as HTMLElement;
      const selected: SessionData[] = [];
      fx.componentInstance.sessionSelected.subscribe(s => selected.push(s));
      const control = root.querySelector('tbody tr button, tbody tr a[href], tbody tr [tabindex="0"]') as HTMLElement;
      expect(control).withContext('a focusable control inside the session row').toBeTruthy();
      control.focus();
      expect(document.activeElement).toBe(control);
      control.click();
      expect(selected.map(s => s.id)).toEqual(['sess-1']);
    });
    it('empty_state_and_list_pass_contrast_and_focus_checks', () => {
      expectAccessible(fx.nativeElement);
      fx.componentInstance.sessions = [];
      fx.detectChanges();
      expectContrast(fx.nativeElement);
    });
  });

  describe('benchmarks (E-10, E-11)', () => {
    it('benchmarks_page_empty_and_loaded_passes_all_checks', async () => {
      await TestBed.configureTestingModule({
        imports: [ModelBenchmarksViewComponent], providers: [provideHttpClient(), provideHttpClientTesting()],
      }).compileComponents();
      const http = TestBed.inject(HttpTestingController);
      const fx = TestBed.createComponent(ModelBenchmarksViewComponent);
      fx.detectChanges();
      http.match(() => true).forEach(r => r.flush([]));
      fx.detectChanges();
      expectAccessible(fx.nativeElement);
      expect((fx.nativeElement as HTMLElement).querySelector('input[type="checkbox"]')).toBeTruthy();
    });
  });

  describe('overview cards (E-11, E-09)', () => {
    it('kpi_card_hint_text_has_4_5_to_1_contrast', async () => {
      await TestBed.configureTestingModule({ imports: [MetricKpiCardComponent] }).compileComponents();
      const fx = TestBed.createComponent(MetricKpiCardComponent);
      fx.componentInstance.title = 'Upstream Attempts Per Min';
      fx.componentInstance.value = 3;
      fx.componentInstance.subtitle = 'Counts every upstream attempt, not client requests';
      fx.detectChanges();
      expectAccessible(fx.nativeElement);
    });
    it('provider_health_dots_do_not_animate_without_motion_safe_and_survive_forced_colors', async () => {
      await TestBed.configureTestingModule({ imports: [ProviderHealthGridComponent] }).compileComponents();
      const fx = TestBed.createComponent(ProviderHealthGridComponent);
      fx.componentRef.setInput('providersList', [{ name: 'steady', status: 'Active' }, { name: 'flaky', status: 'Cooldown', cooldownSeconds: 30 }]);
      fx.detectChanges();
      const root = fx.nativeElement as HTMLElement;
      expectReducedMotionSafe(root);
      expect(root.querySelectorAll('[data-status-dot]').length).toBeGreaterThan(0);
      expectForcedColorsSafe(root, '');
      expectNonTextContrast(root);
    });
  });

  describe('app shell (E-12, E-24, E-25)', () => {
    let fx: ComponentFixture<AppShellComponent>;
    beforeEach(async () => {
      const sse = { connectionStatus: signal('connected'), retryCount: () => 0, connect: () => undefined, disconnect: () => undefined };
      await TestBed.configureTestingModule({
        imports: [AppShellComponent], providers: [provideRouter(routes), { provide: SseService, useValue: sse }],
      }).compileComponents();
      fx = TestBed.createComponent(AppShellComponent);
      fx.detectChanges();
    });
    const root = () => fx.nativeElement as HTMLElement;

    it('shell_passes_names_contrast_focus_ring_target_size_and_motion_checks', () => {
      expectAccessible(root());
      expectNonTextContrast(root());
      expectForcedColorsSafe(root(), '');
    });
    it('first_tab_stop_is_a_skip_link_that_moves_focus_to_the_page_traces_region', () => {
      const first = root().querySelector('a, button') as HTMLAnchorElement;
      expect(first.hasAttribute('data-skip-link')).toBeTrue();
      const target = document.createElement('div');
      target.setAttribute('data-skip-target', '');
      target.tabIndex = 0;
      root().querySelector('main')!.appendChild(target);
      first.click();
      expect(document.activeElement).toBe(target);
      target.remove();
    });
    it('narrow_viewports_get_a_menu_button_that_reveals_the_nav_instead_of_hiding_it', () => {
      const menu = root().querySelector('button[data-menu-toggle]') as HTMLButtonElement;
      expect(menu).withContext('menu toggle (shown below md)').toBeTruthy();
      expect(menu.getAttribute('aria-expanded')).toBe('false');
      expect(root().querySelector('nav[data-mobile-nav]')).toBeNull();
      menu.click();
      fx.detectChanges();
      expect(menu.getAttribute('aria-expanded')).toBe('true');
      const links = Array.from(root().querySelectorAll('nav[data-mobile-nav] a')).map(a => (a.textContent ?? '').trim());
      expect(links).toEqual(['Overview', 'Session Replay', 'Model Benchmarks', 'Models', 'Config Editor']);
    });
    it('sidebar_stays_in_view_while_the_page_scrolls', () => {
      expect(getComputedStyle(root().querySelector('aside') as HTMLElement).position).toBe('sticky');
    });
  });
});

