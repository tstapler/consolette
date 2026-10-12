import { ComponentFixture, TestBed } from '@angular/core/testing';
import { Router, provideRouter } from '@angular/router';
import { InspectDrawerComponent } from './inspect-drawer.component';
import { AttemptTraceData, RequestTraceData } from '../../../core/models/telemetry.models';
import { ROUTE_REASONS } from '../../../core/models/trace-labels';
import traceFailure from '../../../../testing/fixtures/sse/request-trace-failure.json';
import traceSuccess from '../../../../testing/fixtures/sse/request-trace-success.json';

const base = traceSuccess.data as RequestTraceData;
const failureFixture = traceFailure.data as RequestTraceData;
const ID_A = '9f8e7d6c-1a2b-4c3d-8e9f-0a1b2c3d4e5f';
const ID_B = '11223344-aaaa-bbbb-cccc-ddddeeeeffff';

const att = (upstream: string, over: Partial<AttemptTraceData> = {}): AttemptTraceData =>
  ({ iteration: 0, upstream, model: null, outcome: 'ok', status: 200, latencyMs: 10, errorClass: null, errorMessage: null, ...over });

const threeAttempts: AttemptTraceData[] = [
  att('anthropic', { model: 'sonnet-4-5', outcome: 'upstream', status: 529, latencyMs: 310, errorClass: 'overloaded', errorMessage: 'Upstream overloaded (HTTP 529)' }),
  att('bedrock', { model: 'sonnet-4-5', outcome: 'rate_limited', status: 429, latencyMs: 120, errorClass: 'rate_limited', errorMessage: 'Upstream rate limit (HTTP 429)' }),
  att('openrouter', { model: 'gpt-4o-mini', status: 200, latencyMs: 3650 }),
];

function trace(over: Partial<RequestTraceData> = {}): RequestTraceData {
  return {
    ...base, requestId: ID_A, id: ID_A, requestedModel: 'gpt-4o', routedModel: 'gpt-4o-mini', routedModelSource: 'pinned',
    routedProvider: 'openrouter', routeReason: 'fallback', attempts: threeAttempts, totalMs: 4120, ...over,
  };
}

describe('InspectDrawerComponent', () => {
  let fixture: ComponentFixture<InspectDrawerComponent>;
  let drawer: InspectDrawerComponent;
  let el: HTMLElement;
  let tableEl: HTMLElement;
  let widthSpy: jasmine.Spy;
  const pageNodes: HTMLElement[] = [];

  const q = <T extends HTMLElement = HTMLElement>(sel: string) => el.querySelector(sel) as T | null;
  const text = (e: Element | null) => (e?.textContent ?? '').replace(/\s+/g, ' ').trim();
  const dialog = () => q('[role="dialog"]') as HTMLElement;
  const heading = () => q('[data-drawer-heading]') as HTMLElement;
  const attemptButtons = () => Array.from(el.querySelectorAll<HTMLButtonElement>('button[data-attempt]'));

  function originButton(): HTMLButtonElement {
    const b = document.createElement('button');
    b.textContent = 'Inspect';
    document.body.appendChild(b);
    pageNodes.push(b);
    b.focus();
    return b;
  }

  function openOn(t: RequestTraceData, others: RequestTraceData[] = [], origin: HTMLElement | null = originButton(), scroll = false): void {
    drawer.traces = [t, ...others];
    drawer.open(t, origin, scroll);
    fixture.detectChanges();
  }

  beforeEach(async () => {
    await TestBed.configureTestingModule({ imports: [InspectDrawerComponent], providers: [provideRouter([])] }).compileComponents();
    fixture = TestBed.createComponent(InspectDrawerComponent);
    drawer = fixture.componentInstance;
    el = fixture.nativeElement as HTMLElement;
    widthSpy = spyOnProperty(window, 'innerWidth', 'get').and.returnValue(1200);
    tableEl = document.createElement('table');
    tableEl.setAttribute('data-trace-table', '');
    tableEl.tabIndex = -1;
    document.body.appendChild(tableEl);
    pageNodes.push(tableEl);
    fixture.detectChanges();
  });

  afterEach(() => {
    pageNodes.splice(0).forEach(n => n.remove());
    fixture.destroy();
  });

  it('drawer_should_open_non_modal_dialog_focus_heading_set_requestId_param_and_return_focus_to_inspect_button_or_table_when_row_left_buffer', async () => {
    const origin = originButton();
    openOn(trace(), [], origin);
    await fixture.whenStable();
    expect(dialog().getAttribute('aria-modal')).toBe('false');
    expect(dialog().getAttribute('aria-labelledby')).toBe(heading().id);
    expect(document.activeElement).toBe(heading());
    expect(text(heading())).toContain('9f8e7d6c');
    expect(TestBed.inject(Router).url).toContain(`requestId=${ID_A}`);
    dialog().dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    fixture.detectChanges();
    expect(q('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(origin);
    await fixture.whenStable();
    expect(TestBed.inject(Router).url).not.toContain('requestId');

    const gone = originButton();
    openOn(trace(), [], gone);
    gone.remove();
    dialog().dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(document.activeElement).toBe(tableEl);
    expect(document.activeElement).not.toBe(document.body);

    openOn(trace(), [], null);
    heading().dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(document.activeElement).toBe(tableEl);
  });

  it('drawer_should_list_three_attempts_with_text_outcomes_and_sr_only_summary', () => {
    openOn(trace());
    const rows = attemptButtons().map(b => text(b));
    expect(rows[0]).toContain('1 anthropic / sonnet-4-5');
    expect(rows[0]).toContain('x 529 overloaded');
    expect(rows[0]).toContain('310ms');
    expect(rows[1]).toContain('2 bedrock / sonnet-4-5');
    expect(rows[1]).toContain('x 429 rate limited');
    expect(rows[1]).toContain('120ms');
    expect(rows[2]).toContain('3 openrouter / gpt-4o-mini');
    expect(rows[2]).toContain('ok 200');
    expect(rows[2]).toContain('3650ms');
    expect(q('ol[data-attempt-list]')).not.toBeNull();
    expect(el.querySelectorAll('[data-bar]').length).toBe(3);
    el.querySelectorAll('[data-bar]').forEach(b => expect(b.getAttribute('aria-hidden')).toBe('true'));
    expect(text(q('[data-attempt-summary]'))).toBe('3 attempts; first two failed; third succeeded');
    expect(q('[data-attempt-summary]')?.classList.contains('sr-only')).toBeTrue();
  });

  it('drawer_should_group_attempts_under_round_headings_when_search_loop_ran', () => {
    openOn(trace({ iterations: 2, attempts: [att('a'), att('b', { iteration: 1 })] }));
    const rounds = Array.from(el.querySelectorAll('[data-round]')).map(h => text(h));
    expect(rounds).toEqual(['Round 1', 'Round 2']);
  });

  it('drawer_should_select_first_failed_attempt_by_default_and_show_error_class_label_and_message_for_selected_attempt', () => {
    openOn(trace());
    const detail = () => text(q('[data-attempt-detail]'));
    expect(attemptButtons().map(b => b.getAttribute('aria-pressed'))).toEqual(['true', 'false', 'false']);
    expect(detail()).toContain('Attempt 1');
    expect(detail()).toContain('anthropic / sonnet-4-5');
    expect(detail()).toContain('529');
    expect(detail()).toContain('310ms');
    expect(detail()).toContain('overloaded');
    expect(detail()).toContain('Upstream overloaded (HTTP 529)');
    expect(q('[data-attempt-detail]')?.getAttribute('aria-live')).toBe('off');

    const second = attemptButtons()[1];
    second.focus();
    second.click();
    fixture.detectChanges();
    expect(document.activeElement).toBe(second);
    expect(second.getAttribute('aria-pressed')).toBe('true');
    expect(detail()).toContain('rate limited');
    expect(detail()).toContain('Upstream rate limit (HTTP 429)');

    attemptButtons()[2].click();
    fixture.detectChanges();
    expect(detail()).toContain('3650ms');
    expect(detail()).not.toContain('Error');
    expect(q('[data-attempt-error]')).toBeNull();
  });

  it('drawer_should_show_error_class_label_and_message_and_not_sentinel_for_failed_attempt', () => {
    const t = trace({ attempts: [att('anthropic', { outcome: 'upstream', status: 529, errorClass: 'overloaded', errorMessage: 'Upstream overloaded (HTTP 529)' }), att('openrouter')] });
    openOn(t);
    expect(el.textContent).toContain('overloaded');
    expect(el.textContent).toContain('Upstream overloaded (HTTP 529)');
    expect(el.textContent).not.toContain('SENTINEL');
    expect(JSON.stringify(t)).not.toContain('SENTINEL');
  });

  it('drawer_should_show_evicted_no_attempt_cache_and_truncated_states', () => {
    drawer.traces = [];
    drawer.selectById('gone');
    fixture.detectChanges();
    expect(text(el)).toContain('This trace is no longer retained (buffer holds last 100).');

    openOn(trace({ attempts: [] }));
    expect(text(el)).toContain('Attempt detail not captured');

    openOn(trace({ traceKind: 'cache_hit', routeReason: 'cache', attempts: [] }));
    expect(text(el)).toContain('Served from the response cache; no upstream attempt');

    openOn(trace({ attemptsTruncated: true }));
    expect(text(el)).toContain('showing first 16 attempts');
  });

  it('attempts_badge_should_open_drawer_with_attempts_heading_scrolled_into_view_and_first_failed_attempt_selected', () => {
    const spy = spyOn(HTMLElement.prototype, 'scrollIntoView');
    openOn(trace(), [], originButton(), true);
    expect(spy).toHaveBeenCalledTimes(1);
    expect(spy.calls.mostRecent().object === q('[data-attempts-heading]')).toBeTrue();
    expect(document.activeElement).toBe(heading());
    expect(attemptButtons()[0].getAttribute('aria-pressed')).toBe('true');
    spy.calls.reset();
    openOn(trace({ requestId: ID_B, id: ID_B }), [], originButton(), false);
    expect(spy).not.toHaveBeenCalled();
  });

  it('drawer_should_announce_showing_trace_id8_and_keep_focus_on_heading_when_content_swaps', async () => {
    const a = trace();
    const b = trace({ requestId: ID_B, id: ID_B });
    openOn(a, [b]);
    expect(text(heading())).toContain('9f8e7d6c');
    drawer.open(b, originButton(), false);
    fixture.detectChanges();
    heading().focus();
    expect(document.activeElement).toBe(heading());
    expect(text(heading())).toContain('11223344');
    expect(text(q('[data-drawer-status]'))).toBe('Showing trace 11223344');
    await fixture.whenStable();
    expect(TestBed.inject(Router).url).toContain(`requestId=${ID_B}`);
  });

  it('drawer_should_be_full_width_sheet_without_horizontal_scroll_at_640_and_320px_and_all_controls_at_least_24x24', () => {
    widthSpy.and.returnValue(640);
    openOn(trace());
    expect(dialog().getAttribute('data-mode')).toBe('sheet');
    expect(dialog().classList.contains('w-full')).toBeTrue();
    expect(q('button[data-drawer-close]')).not.toBeNull();
    dialog().style.width = '320px';
    expect(dialog().scrollWidth).toBeLessThanOrEqual(dialog().clientWidth);
    widthSpy.and.returnValue(1200);
    fixture.componentInstance.refreshLayout();
    fixture.detectChanges();
    expect(dialog().getAttribute('data-mode')).toBe('panel');
    el.querySelectorAll('button').forEach(b => {
      const r = b.getBoundingClientRect();
      expect(r.width).withContext(b.textContent ?? '').toBeGreaterThanOrEqual(24);
      expect(r.height).withContext(b.textContent ?? '').toBeGreaterThanOrEqual(24);
    });
  });

  it('drawer_should_state_upstream_class_status_caveat_and_headers_note_for_streams', () => {
    openOn(trace({ stream: false }));
    expect(text(q('[data-status-note]'))).toContain('Status shown is the upstream-class status; the client may have received a different code (for example 529 vs 503 depending on the endpoint)');
    expect(text(q('[data-status-note]'))).not.toContain('status at response headers');
    openOn(trace({ stream: true }));
    expect(text(q('[data-status-note]'))).toContain('status at response headers');
  });

  it('cache_hit_should_not_claim_a_routed_upstream_model_because_no_upstream_ran_E23', () => {
    openOn(trace({ traceKind: 'cache_hit', routeReason: 'cache', attempts: [], requestedModel: 'claude-sonnet-4', routedModel: 'claude-sonnet-4', routedModelSource: null }));
    const routed = text(q('[data-routed]'));
    expect(routed).not.toContain('Routed');
    expect(routed).toContain('claude-sonnet-4');
    expect(routed).toContain('no upstream ran');
  });

  it('session_pinned_request_should_say_the_model_came_from_the_session_pin_not_the_configured_route_E23', () => {
    openOn(trace({ routeReason: 'session_pin', routedModelSource: 'pinned', routedProvider: 'steady', requestedModel: 'claude-sonnet-4', routedModel: 'gpt-5.2' }));
    const src = text(q('[data-model-source]'));
    expect(src).toContain("session's pin");
    expect(src).not.toContain('configured model');
  });

  it('why_block_should_show_model_source_pinned_or_passthrough_with_routed_upstream_and_exact_why_text_for_all_eight_reasons_and_cancelled', () => {
    const t = trace();
    openOn(t);
    expect(text(q('[data-requested]'))).toContain('Requested gpt-4o');
    expect(text(q('[data-routed]'))).toContain('Routed openrouter / gpt-4o-mini');
    expect(text(q('[data-model-source]'))).toBe("pinned: openrouter's configured model replaces the client's gpt-4o");
    expect(text(q('[data-why]'))).toContain('moved to the next candidate after anthropic failed (overloaded, HTTP 529)');

    openOn(trace({ routedModelSource: 'passthrough', routedProvider: 'openai' }));
    expect(text(q('[data-model-source]'))).toBe("passthrough: the client's model name was sent unchanged to openai");

    const rateHold = [att('a', { outcome: 'rate_limited', status: 429, errorClass: 'rate_limited' }), att('a')];
    const expected: Array<[string, Partial<RequestTraceData>, string]> = [
      ['session_pin', {}, 'pinned to this upstream by the session'],
      ['strategy', { routeStrategy: 'weighted' }, 'chosen by the weighted strategy'],
      ['exploration', {}, 'exploration pick'],
      ['fallback', {}, 'moved to the next candidate after anthropic failed (overloaded, HTTP 529)'],
      ['rate_limit_hold', { attempts: rateHold }, 'waited for a rate-limit cooldown, then retried (1x)'],
      ['default', {}, 'default route: no session pin, exploration or strategy decision applied'],
      ['cache', { traceKind: 'cache_hit', attempts: [] }, 'served from the response cache; no upstream was called'],
      ['local', { traceKind: 'local', routedModel: 'llama3', attempts: [] }, 'served by a local model llama3'],
    ];
    expect(expected.map(e => e[0]).sort()).toEqual(Object.keys(ROUTE_REASONS).sort());
    for (const [reason, over, why] of expected) {
      openOn(trace({ routeReason: reason, ...over }));
      expect(text(q('[data-why]'))).withContext(reason).toContain(why);
    }
    openOn(trace({ statusCode: 499 }));
    expect(text(q('[data-why]'))).toContain('client closed the connection');
  });

  it('not_recorded_block_should_list_each_na_column_with_exact_reason_string_and_be_omitted_when_none', () => {
    openOn(trace({ traceKind: 'cache_hit', routeReason: 'cache', attempts: [], tokensAfter: null }));
    const block = text(q('[data-not-recorded]'));
    expect(block).toContain('First byte: n/a (served from cache)');
    expect(block).toContain('Tokens after: n/a (served from cache)');
    openOn(trace({ firstByteMs: 120, compressionStatus: 'compressed', tokensBefore: 2000, tokensAfter: 1240 }));
    expect(q('[data-not-recorded]')).toBeNull();
  });

  it('tokens block shows before and after per the column truth table', () => {
    openOn(trace({ compressionStatus: 'off', tokensBefore: 1870, tokensAfter: null }));
    expect(text(q('[data-tokens]'))).toContain('before 1,870 est.');
    expect(text(q('[data-tokens]'))).toContain('after n/a (compression off)');
  });

  it('drawer_should_keep_last_data_and_show_no_longer_in_live_buffer_banner_when_open_trace_is_evicted_without_moving_focus', () => {
    const a = trace();
    openOn(a, [trace({ requestId: ID_B, id: ID_B })]);
    drawer.traces = [trace({ requestId: ID_B, id: ID_B })];
    fixture.detectChanges();
    expect(text(q('[data-drawer-status]'))).toContain('No longer in live buffer - showing the last data received');
    expect(text(heading())).toContain('9f8e7d6c');
    expect(attemptButtons().length).toBe(3);
    expect(document.activeElement).toBe(heading());
    expect(text(el)).not.toContain('no longer retained');
  });

  it('drawer_should_be_modal_with_inert_page_and_tab_cycle_and_esc_restore_at_640px_and_non_modal_with_tab_out_above_640px', () => {
    const outside = document.createElement('div');
    outside.setAttribute('data-outside', '');
    document.body.appendChild(outside);
    pageNodes.push(outside);

    widthSpy.and.returnValue(640);
    const origin = originButton();
    openOn(trace(), [], origin);
    expect(dialog().getAttribute('aria-modal')).toBe('true');
    expect(outside.hasAttribute('inert')).toBeTrue();
    expect(tableEl.hasAttribute('inert')).toBeTrue();
    expect(dialog().hasAttribute('inert')).toBeFalse();
    const focusables = Array.from(dialog().querySelectorAll<HTMLElement>('button')).filter(b => !b.hasAttribute('disabled'));
    const first = focusables[0];
    const last = focusables[focusables.length - 1];
    last.focus();
    const tab = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    last.dispatchEvent(tab);
    expect(tab.defaultPrevented).toBeTrue();
    expect(document.activeElement).toBe(first);
    const back = new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true });
    first.dispatchEvent(back);
    expect(document.activeElement).toBe(last);
    last.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(outside.hasAttribute('inert')).toBeFalse();
    expect(document.activeElement).toBe(origin);

    widthSpy.and.returnValue(800);
    openOn(trace(), [], origin);
    expect(dialog().getAttribute('aria-modal')).toBe('false');
    expect(outside.hasAttribute('inert')).toBeFalse();
    const focusables2 = Array.from(dialog().querySelectorAll<HTMLElement>('button'));
    const tab2 = new KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true });
    focusables2[focusables2.length - 1].dispatchEvent(tab2);
    expect(tab2.defaultPrevented).toBeFalse();
    focusables2[1].dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    expect(q('[role="dialog"]')).toBeNull();
  });

  it('drawer_should_clear_inert_from_the_page_when_destroyed_while_the_narrow_sheet_is_open', () => {
    const outside = document.createElement('div');
    document.body.appendChild(outside);
    pageNodes.push(outside);
    widthSpy.and.returnValue(640);
    openOn(trace());
    expect(outside.hasAttribute('inert')).toBeTrue();
    fixture.destroy();
    expect(outside.hasAttribute('inert')).toBeFalse();
    expect(tableEl.hasAttribute('inert')).toBeFalse();
  });

  it('drawer_should_keep_the_same_attempt_button_node_when_an_unrelated_trace_is_pushed', () => {
    const t = trace();
    openOn(t);
    const before = attemptButtons();
    expect(before.length).toBeGreaterThan(0);
    before[0].focus();
    drawer.traces = [trace({ requestId: 'other-request-id' } as Partial<RequestTraceData>), t];
    fixture.detectChanges();
    const after = attemptButtons();
    expect(after[0]).toBe(before[0]);
    expect(document.activeElement).toBe(before[0]);
  });

  it('inspect_should_show_failed_attempt_error_class_label_message_and_latency_within_two_steps', () => {
    openOn(failureFixture as RequestTraceData);
    const detail = text(q('[data-attempt-detail]'));
    expect(detail).toContain('529');
    expect(detail).toContain('overloaded');
    expect(detail).toContain('Upstream overloaded (HTTP 529)');
    expect(detail).toMatch(/\d+ms/);
  });

  it('copy button copies the full request id and announces it', async () => {
    const write = jasmine.createSpy('writeText').and.returnValue(Promise.resolve());
    spyOnProperty(navigator, 'clipboard', 'get').and.returnValue({ writeText: write } as unknown as Clipboard);
    openOn(trace());
    q<HTMLButtonElement>('button[data-drawer-copy]')!.click();
    await fixture.whenStable();
    fixture.detectChanges();
    expect(write).toHaveBeenCalledWith(ID_A);
    expect(text(q('[data-drawer-status]'))).toContain('Copied request ID');
  });
});
