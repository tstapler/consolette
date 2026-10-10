import { ComponentFixture, TestBed } from '@angular/core/testing';
import { RecentTraceTableComponent, TRACE_COVERAGE_NOTE_ENABLED } from './recent-trace-table.component';
import { RequestTraceData } from '../../../core/models/telemetry.models';
import { NA_REASON_TEXTS } from '../../../core/models/na-reason';
import traceSuccess from '../../../../testing/fixtures/sse/request-trace-success.json';
import traceFailure from '../../../../testing/fixtures/sse/request-trace-failure.json';

const success = traceSuccess.data as RequestTraceData;
const failure = traceFailure.data as RequestTraceData;

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
    const attempt = (upstream: string) => ({ upstream });
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

  it('footer_should_show_coverage_note_in_epic_1_build_and_tests_should_assert_handler_400_has_no_trace_until_2_1_6b', () => {
    expect(TRACE_COVERAGE_NOTE_ENABLED).toBeTrue();
    render([success]);
    expect(text(el)).toContain('Rows cover requests that reach routing. Requests rejected before routing, cache hits and multi-round search requests are not shown as one row yet.');
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
