import { ComponentFixture, TestBed } from '@angular/core/testing';
import { RecentTraceTableComponent } from './recent-trace-table.component';
import { AttemptTraceData, RequestTraceData } from '../../../core/models/telemetry.models';
import { CHIP_LEGEND, ROUTE_REASONS } from '../../../core/models/trace-labels';
import { NA_REASON_TEXTS } from '../../../core/models/na-reason';
import traceSuccess from '../../../../testing/fixtures/sse/request-trace-success.json';
import traceFailure from '../../../../testing/fixtures/sse/request-trace-failure.json';

const success = traceSuccess.data as RequestTraceData;
const failure = traceFailure.data as RequestTraceData;

export function att(upstream: string, over: Partial<AttemptTraceData> = {}): AttemptTraceData {
  return { iteration: 0, upstream, model: null, outcome: 'ok', status: 200, latencyMs: 10, errorClass: null, errorMessage: null, ...over };
}

function make(over: Partial<RequestTraceData> = {}): RequestTraceData {
  return { ...success, ...over };
}

describe('RecentTraceTableComponent (TC-UNIT-17)', () => {
  let component: RecentTraceTableComponent;
  let fixture: ComponentFixture<RecentTraceTableComponent>;
  let el: HTMLElement;

  beforeEach(async () => {
    await TestBed.configureTestingModule({ imports: [RecentTraceTableComponent] }).compileComponents();
    fixture = TestBed.createComponent(RecentTraceTableComponent);
    component = fixture.componentInstance;
    el = fixture.nativeElement as HTMLElement;
  });

  function render(traces: RequestTraceData[], status: 'connecting' | 'connected' | 'reconnecting' | 'disconnected' = 'connected'): void {
    component.traces = traces;
    component.connectionStatus = status;
    fixture.detectChanges();
  }

  const rows = () => Array.from(el.querySelectorAll('tbody tr[data-row]'));
  const text = (e: Element) => (e.textContent ?? '').replace(/\s+/g, ' ');

  it('row_should_show_req_123_42ms_est_tokens_and_200_OK_when_success_fixture_rendered', () => {
    render([success]);
    const row = text(rows()[0]);
    expect(row).toContain('req_123');
    expect(row).toContain('42ms');
    expect(row).toContain('1,870 est.');
    expect(row).toContain('200 OK');
  });

  it('row_should_show_503_Unavailable_and_sr_only_no_response_when_failure_fixture_rendered', () => {
    render([failure]);
    const row = rows()[0] as HTMLElement;
    expect(text(row)).toContain('503 Unavailable');
    const dash = row.querySelector('[data-na="firstByte"]') as HTMLElement;
    expect(dash.textContent).toContain('—');
    expect(dash.getAttribute('title')).toBe('n/a (no response)');
    expect(dash.querySelector('.sr-only')?.textContent).toBe('n/a (no response)');
    expect(dash.hasAttribute('tabindex')).toBeFalse();
    expect(row.textContent).not.toContain('undefined');
    expect(row.textContent).not.toContain('NaN');
  });

  it('row_should_render_en_dash_and_warn_once_when_event_is_malformed', () => {
    const warn = spyOn(console, 'warn');
    const bad = { ...success, requestId: 'req_bad', timeToHeadersMs: undefined, statusCode: undefined } as unknown as RequestTraceData;
    render([bad]);
    fixture.detectChanges();
    const row = rows()[0] as HTMLElement;
    expect(row.textContent).toContain('—');
    expect(row.textContent).not.toContain('undefined');
    expect(row.textContent).not.toContain('NaN');
    expect(row.querySelector('button')).toBeTruthy();
    expect(warn).toHaveBeenCalledTimes(1);
  });

  it('status_cell_should_have_title_status_at_response_headers_when_stream_true', () => {
    render([make({ stream: true }), make({ requestId: 'req_ns', id: 'req_ns', stream: false })]);
    const cells = el.querySelectorAll('[data-status]');
    expect(cells[0].getAttribute('title')).toBe('status at response headers');
    expect(cells[1].getAttribute('title')).toBeNull();
  });

  it('status cell shows cache and cancelled variants and fell back / retried sub-labels', () => {
    const attempt = (upstream: string) => att(upstream);
    render([
      make({ requestId: 'a', id: 'a', traceKind: 'cache_hit' }),
      make({ requestId: 'b', id: 'b', statusCode: 499 }),
      make({ requestId: 'c', id: 'c', attempts: [attempt('x'), attempt('y')] }),
      make({ requestId: 'd', id: 'd', attempts: [attempt('x'), attempt('x')] }),
      make({ requestId: 'e', id: 'e', statusCode: 502, attempts: [attempt('x'), attempt('y')] }),
    ]);
    const t = (i: number) => text(rows()[i]);
    expect(t(0)).toContain('200 (cache)');
    expect(t(1)).toContain('499 Client closed');
    expect(t(2)).toContain('fell back');
    expect(t(3)).toContain('retried');
    expect(t(4)).not.toContain('fell back');
  });

  it('table_should_show_three_skeleton_rows_and_connecting_text_until_first_event_or_connected', () => {
    render([], 'connecting');
    const skeleton = el.querySelectorAll('tr[data-skeleton]');
    expect(skeleton.length).toBe(3);
    skeleton.forEach(s => expect(s.getAttribute('aria-hidden')).toBe('true'));
    expect(el.textContent).toContain('Connecting to live feed...');
    expect(el.textContent).not.toContain('No requests yet');

    component.connectionStatus = 'connected';
    fixture.detectChanges();
    expect(el.querySelectorAll('tr[data-skeleton]').length).toBe(0);
    expect(el.textContent).toContain('No requests yet');

    component.connectionStatus = 'connecting';
    component.traces = [success];
    fixture.detectChanges();
    expect(el.querySelectorAll('tr[data-skeleton]').length).toBe(0);
    expect(rows().length).toBe(1);
  });

  it('empty_connected_should_show_proxy_origin_url_and_copy_url_button_and_never_mention_the_stream', async () => {
    const writeText = jasmine.createSpy('writeText').and.resolveTo();
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    render([], 'connected');
    expect(el.textContent).toContain('No requests yet. Point a client at this proxy to see traces.');
    expect(el.querySelector('code')?.textContent).toBe(window.location.origin);
    expect(el.textContent?.toLowerCase()).not.toContain('stream');
    expect(el.textContent?.toLowerCase()).not.toContain('connection error');
    const btn = Array.from(el.querySelectorAll('button')).find(b => b.textContent?.includes('Copy URL')) as HTMLButtonElement;
    btn.click();
    await fixture.whenStable();
    expect(writeText).toHaveBeenCalledOnceWith(window.location.origin);
    delete (navigator as unknown as Record<string, unknown>)['clipboard'];
  });

  it('reconnecting shows a status banner, keeps rows and dims them', () => {
    component.retryAttempt = 3;
    render([success], 'reconnecting');
    const banner = el.querySelector('[data-banner][role="status"]');
    expect(banner?.textContent).toContain('Live feed interrupted - reconnecting (attempt 3)');
    expect(rows().length).toBe(1);
    expect(el.querySelector('[data-rows-dimmed]')).toBeTruthy();
  });

  describe('copy', () => {
    afterEach(() => delete (navigator as unknown as Record<string, unknown>)['clipboard']);
    const copyBtn = () => el.querySelector('button[data-copy-id]') as HTMLButtonElement;
    const status = () => el.querySelector('[data-copy-status][role="status"]') as HTMLElement;

    it('copy_button_should_write_full_requestId_and_announce_Copied_request_ID_via_status_region', async () => {
      let resolve!: () => void;
      const writeText = jasmine.createSpy('writeText').and.returnValue(new Promise<void>(r => (resolve = r)));
      Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
      render([make({ requestId: 'req_full_identifier_1234', id: 'x' })]);
      copyBtn().click();
      fixture.detectChanges();
      expect(writeText).toHaveBeenCalledOnceWith('req_full_identifier_1234');
      expect(status().textContent).not.toContain('Copied');
      resolve();
      await fixture.whenStable();
      fixture.detectChanges();
      expect(status().textContent).toContain('Copied request ID');
      expect(el.querySelectorAll('[role="status"][data-copy-status]').length).toBe(1);
    });

    it('copy falls back to execCommand when the clipboard API is missing', async () => {
      Object.defineProperty(navigator, 'clipboard', { value: undefined, configurable: true });
      spyOn(document, 'execCommand').and.returnValue(true);
      render([success]);
      copyBtn().click();
      await fixture.whenStable();
      fixture.detectChanges();
      expect(status().textContent).toContain('Copied request ID');
    });

    it('copy announces failure and never Copied when both strategies fail', async () => {
      Object.defineProperty(navigator, 'clipboard', { value: { writeText: () => Promise.reject(new Error('no')) }, configurable: true });
      spyOn(document, 'execCommand').and.returnValue(false);
      render([success]);
      copyBtn().click();
      await fixture.whenStable();
      fixture.detectChanges();
      expect(status().textContent).toContain('Copy failed - select the ID and copy it manually');
      expect(status().textContent).not.toContain('Copied');
      expect(window.getSelection()?.toString()).toBe('req_123');
    });
  });

  it('table_should_hold_100_rows_and_show_showing_latest_100_footer_when_101st_arrives', () => {
    const many = Array.from({ length: 101 }, (_, i) => make({ requestId: `req_${i}`, id: `req_${i}` }));
    render(many.slice(0, 100));
    expect(el.textContent).toContain('Showing latest 100');
    render(many);
    expect(rows().length).toBe(100);
    expect(rows().some(r => text(r).includes('req_100'))).toBeFalse();
    render([success]);
    expect(el.textContent).not.toContain('Showing latest 100');
  });

  it('tokens_cell_should_show_2000_to_1240_minus_38_percent_only_when_compressed', () => {
    const cases: Array<[Partial<RequestTraceData>, boolean]> = [
      [{ compressionStatus: 'compressed', tokensBefore: 2000, tokensAfter: 1240 }, true],
      [{ compressionStatus: 'none', tokensBefore: 2000, tokensAfter: 2000 }, false],
      [{ compressionStatus: 'off', tokensBefore: 2000, tokensAfter: null }, false],
      [{ compressionStatus: 'skipped', tokensBefore: 2000, tokensAfter: null }, false],
      [{ compressionStatus: 'failed', tokensBefore: 2000, tokensAfter: null }, false],
    ];
    for (const [over, saved] of cases) {
      render([make(over)]);
      const row = text(rows()[0]);
      expect(row.includes('2,000 -> 1,240 (-38%)')).withContext(String(over.compressionStatus)).toBe(saved);
      expect(row.includes('saved')).toBeFalse();
      expect(row.toLowerCase()).not.toContain('no savings');
    }
    render([make({ compressionStatus: 'skipped', tokensAfter: null })]);
    expect(rows()[0].querySelector('[data-na="tokensAfter"]')?.getAttribute('title')).toBe('n/a (compression skipped)');
    render([make({ compressionStatus: 'failed', tokensAfter: null })]);
    expect(rows()[0].querySelector('[data-na="tokensAfter"]')?.getAttribute('title')).toBe('n/a (compression failed)');
  });

  it('header labels are honest', () => {
    render([success]);
    const head = text(el.querySelector('thead') as Element);
    expect(head).toContain('Latency (to headers)');
    expect(head).toContain('First byte');
    expect(head).not.toContain('TTFT');
  });

  it('na_dash_should_not_be_a_tab_stop_and_have_title_equal_to_sr_only_reason', () => {
    render([success, failure, make({ requestId: 'c', id: 'c', traceKind: 'cache_hit' }), make({ requestId: 'd', id: 'd', statusCode: 499 })]);
    const dashes = el.querySelectorAll('[data-na]');
    expect(dashes.length).toBeGreaterThanOrEqual(8);
    dashes.forEach(d => {
      expect(d.hasAttribute('tabindex')).toBeFalse();
      expect(d.getAttribute('title')).toBe(d.querySelector('.sr-only')?.textContent ?? '');
      expect(d.getAttribute('title')).toMatch(/^n\/a \(.+\)$/);
    });
  });

  it('legend_disclosure_should_show_every_truth_table_reason_string_as_visible_text_after_keyboard_activation', () => {
    render([success]);
    const toggle = Array.from(el.querySelectorAll('button')).find(b => b.textContent?.includes('What do these dashes and chips mean?')) as HTMLButtonElement;
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(el.querySelector('[data-legend]')).toBeNull();
    toggle.click(); // Enter/Space on a native button dispatch click
    fixture.detectChanges();
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    const legend = el.querySelector('[data-legend]') as HTMLElement;
    for (const r of NA_REASON_TEXTS) {
      const hit = Array.from(legend.querySelectorAll('*')).find(n => n.children.length === 0 && n.textContent?.includes(r.text));
      expect(hit).withContext(r.text).toBeTruthy();
      expect(hit?.closest('.sr-only')).toBeNull();
    }
    for (const chip of ['pinned', 'strategy', 'explore', 'fallback', 'held', 'default', 'cache', 'local', 'cancelled']) {
      expect(legend.textContent).toContain(chip);
    }
  });

  it('row_inspect_button_aria_describedby_should_list_exactly_that_rows_na_reasons', () => {
    render([success, make({ requestId: 'req_real', id: 'req_real', stream: true, firstByteMs: 800, compressionStatus: 'compressed', tokensAfter: 500 })]);
    const [r0, r1] = rows();
    const btn0 = r0.querySelector('button[data-inspect]') as HTMLButtonElement;
    const desc0 = el.querySelector('#' + btn0.getAttribute('aria-describedby')) as HTMLElement;
    expect(desc0.classList.contains('sr-only')).toBeTrue();
    expect(desc0.textContent?.trim()).toBe('First byte: n/a (not streamed). Tokens after: n/a (compression off).');
    expect(r1.querySelector('button[data-inspect]')?.hasAttribute('aria-describedby')).toBeFalse();
  });

  it('legend_entries_should_separate_term_from_definition_with_a_visible_separator_E26', () => {
    render([success]);
    (Array.from(el.querySelectorAll('button')).find(b => /legend|dashes/i.test(b.textContent ?? '')) as HTMLButtonElement).click();
    fixture.detectChanges();
    const items = Array.from(el.querySelectorAll('[data-legend] li'));
    expect(items.length).toBeGreaterThan(0);
    items.forEach(li => expect(text(li)).toMatch(/\S (-|:|\u2013|\u2014) \S/));
  });

  it('footer_dash_legend_line_should_appear_exactly_once', () => {
    render([success, failure]);
    const needle = '- = not applicable; hover a dash or open the legend for the reason';
    expect((el.textContent ?? '').split(needle).length - 1).toBe(1);
  });

  it('table_should_have_no_more_tab_stops_per_row_than_inspect_copy_and_badge_controls', () => {
    render([success, failure]);
    rows().forEach(r => {
      const stops = r.querySelectorAll('button, a[href], input, select, textarea, [tabindex]:not([tabindex="-1"])');
      expect(stops.length).toBeLessThanOrEqual(3);
    });
  });

  describe('route cell (Story 2.1.4)', () => {
    const route = (e: Element) => e.querySelector('[data-route]') as HTMLElement;
    const routed = (over: Partial<RequestTraceData>) =>
      make({ requestedModel: 'gpt-4o', routedModel: 'gpt-4o-mini', routedProvider: 'openrouter', routedModelSource: 'pinned', routeReason: 'fallback', ...over });

    it('route_cell_should_show_arrow_and_provider_when_names_differ_and_single_name_when_equal', () => {
      render([routed({}), routed({ requestId: 'eq', id: 'eq', routedModel: 'gpt-4o', routedModelSource: 'passthrough' }),
        routed({ requestId: 'pt', id: 'pt', routedModel: 'gpt-4o', routedModelSource: 'passthrough', routedProvider: 'openai' })]);
      const [a, b, c] = rows().map(r => route(r));
      expect(text(a.querySelector('[data-route-models]') as Element)).toBe('gpt-4o -> gpt-4o-mini');
      expect(text(a.querySelector('[data-route-provider]') as Element)).toBe('openrouter');
      expect(a.querySelector('[data-route-models]')?.getAttribute('title')).toBe('gpt-4o -> gpt-4o-mini');
      expect(text(b.querySelector('[data-route-models]') as Element)).toBe('gpt-4o');
      expect(text(c.querySelector('[data-route-models]') as Element)).not.toContain('->');
      expect(text(c.querySelector('[data-route-provider]') as Element)).toBe('openai');
    });

    it('route_chip_text_should_map_each_wire_reason_exactly_and_status_cell_should_derive_fell_back_vs_retried', () => {
      const expected: Record<string, string> = {
        session_pin: 'pinned', strategy: 'strategy', exploration: 'explore', fallback: 'fallback',
        rate_limit_hold: 'held', default: 'default', cache: 'cache', local: 'local',
      };
      render([...Object.keys(expected), 'brand_new_reason'].map(r => routed({ requestId: r, id: r, routeReason: r })));
      const chips = rows().map(r => text(r.querySelector('[data-chip]') as Element));
      expect(chips).toEqual([...Object.values(expected), 'brand_new_reason']);
      render([
        routed({ requestId: 'f', id: 'f', attempts: [att('anthropic', { status: 529, outcome: 'upstream' }), att('openrouter')] }),
        routed({ requestId: 'r', id: 'r', attempts: [att('anthropic', { status: 429, outcome: 'rate_limited' }), att('anthropic')] }),
        routed({ requestId: 'x', id: 'x', statusCode: 502, attempts: [att('anthropic'), att('openrouter')] }),
      ]);
      expect(text(rows()[0].querySelector('[data-status]') as Element)).toContain('fell back');
      expect(text(rows()[1].querySelector('[data-status]') as Element)).toContain('retried');
      const failed = text(rows()[2].querySelector('[data-status]') as Element);
      expect(failed).toContain('502');
      expect(failed).not.toMatch(/fell back|retried/);
    });

    it('route_cell_should_show_fallback_chip_x3_badge_and_search_rounds_badge', () => {
      const three = [att('a', { outcome: 'upstream', status: 529 }), att('b', { outcome: 'upstream', status: 529 }), att('c')];
      render([routed({ attempts: three }), routed({ requestId: 'one', id: 'one', attempts: [att('a')], iterations: 3 })]);
      const first = rows()[0];
      expect(text(first.querySelector('[data-chip]') as Element)).toBe('fallback');
      const badge = first.querySelector('button[data-attempts-badge]') as HTMLButtonElement;
      expect(text(badge)).toBe('x3 attempts');
      expect(first.querySelector('[data-rounds-badge]')).toBeNull();
      const second = rows()[1];
      expect(second.querySelector('button[data-attempts-badge]')).toBeNull();
      expect(text(second.querySelector('[data-rounds-badge]') as Element)).toBe('3 search rounds');
    });

    it('attempts badge emits an inspect request that asks for the ATTEMPTS scroll, plain Inspect does not', () => {
      render([routed({ attempts: [att('a', { outcome: 'upstream', status: 529 }), att('b')] })]);
      const seen: Array<{ attempts: boolean }> = [];
      component.inspect.subscribe(e => seen.push({ attempts: e.scrollToAttempts }));
      (rows()[0].querySelector('button[data-attempts-badge]') as HTMLButtonElement).click();
      (rows()[0].querySelector('button[data-inspect]') as HTMLButtonElement).click();
      expect(seen).toEqual([{ attempts: true }, { attempts: false }]);
    });

    it('route_cell_should_show_cache_local_and_cancelled_chips_without_provider_line', () => {
      render([
        make({ requestId: 'c', id: 'c', traceKind: 'cache_hit', routeReason: 'cache' }),
        make({ requestId: 'l', id: 'l', traceKind: 'local', routeReason: 'local' }),
        make({ requestId: 'x', id: 'x', statusCode: 499, routeReason: 'fallback' }),
      ]);
      const chip = (i: number) => text(rows()[i].querySelector('[data-chip]') as Element);
      expect([chip(0), chip(1), chip(2)]).toEqual(['cache', 'local', 'cancelled']);
      expect(rows()[0].querySelector('[data-route-provider]')).toBeNull();
    });

    it('every_wire_route_reason_should_have_chip_title_and_legend_row_with_plain_language_text', () => {
      const keys = Object.keys(ROUTE_REASONS);
      expect(keys.length).toBe(8);
      render(keys.map(r => routed({ requestId: r, id: r, routeReason: r })));
      keys.forEach((k, i) => {
        const chipEl = rows()[i].querySelector('[data-chip]') as HTMLElement;
        expect(chipEl.getAttribute('title')).toBe(ROUTE_REASONS[k].legend);
        expect(CHIP_LEGEND.some(c => c.chip === ROUTE_REASONS[k].chip && c.meaning === ROUTE_REASONS[k].legend)).toBeTrue();
      });
      expect(CHIP_LEGEND.some(c => c.chip === 'cancelled')).toBeTrue();
      const btn = Array.from(el.querySelectorAll('button')).find(b => b.textContent?.includes('What do these dashes and chips mean?')) as HTMLButtonElement;
      btn.click();
      fixture.detectChanges();
      const legend = text(el.querySelector('[data-legend]') as Element);
      CHIP_LEGEND.forEach(c => expect(legend).toContain(c.meaning));
    });

    it('table_controls_should_be_at_least_24x24_and_table_element_should_be_focus_fallback_target', () => {
      render([routed({ attempts: [att('a', { outcome: 'upstream', status: 529 }), att('b')] })]);
      el.querySelectorAll('button').forEach(b => {
        const r = b.getBoundingClientRect();
        expect(r.width).withContext(b.textContent ?? '').toBeGreaterThanOrEqual(24);
        expect(r.height).withContext(b.textContent ?? '').toBeGreaterThanOrEqual(24);
      });
      const table = el.querySelector('table[data-trace-table]') as HTMLElement;
      expect(table.getAttribute('tabindex')).toBe('-1');
    });
  });

  it('should emit inspectPayload and selectTrace events when Inspect button is clicked', () => {
    render([success, failure]);
    spyOn(component.inspectPayload, 'emit');
    spyOn(component.selectTrace, 'emit');
    (el.querySelector('button[data-inspect]') as HTMLButtonElement).click();
    expect(component.inspectPayload.emit).toHaveBeenCalledWith(success);
    expect(component.selectTrace.emit).toHaveBeenCalledWith(success);
  });

  it('rows render provider and model', () => {
    render([success]);
    expect(text(el)).toContain('anthropic');
    expect(text(el)).toContain('claude-haiku-4-5');
  });
});
