import { ComponentFixture, TestBed, fakeAsync, tick, discardPeriodicTasks } from '@angular/core/testing';
import { RecentTraceTableComponent } from './recent-trace-table.component';
import { RequestTraceData } from '../../../core/models/telemetry.models';
import traceSuccess from '../../../../testing/fixtures/sse/request-trace-success.json';

const success = traceSuccess.data as RequestTraceData;

describe('RecentTraceTableComponent live region, Pause and held updates (UX-22, UX-23)', () => {
  let fixture: ComponentFixture<RecentTraceTableComponent>;
  let el: HTMLElement;
  const mk = (n: number, prefix = 'req_x'): RequestTraceData[] =>
    Array.from({ length: n }, (_, i) => ({ ...success, requestId: `${prefix}${String(i).padStart(3, '0')}`, id: `${prefix}${i}` }));
  const push = (traces: RequestTraceData[]) => { fixture.componentInstance.traces = traces; fixture.detectChanges(); };
  const status = () => (el.querySelector('[data-trace-status]')?.textContent ?? '').trim();
  const ids = () => Array.from(el.querySelectorAll('tbody tr[data-row] [data-id-text]')).map(n => n.getAttribute('title'));
  const btn = (re: RegExp) => Array.from(el.querySelectorAll('button')).find(b => re.test((b.textContent ?? '').trim())) as HTMLButtonElement | undefined;
  const focusRow = () => {
    const b = el.querySelector('button[data-inspect]') as HTMLButtonElement;
    b.focus();
    b.dispatchEvent(new FocusEvent('focusin', { bubbles: true }));
  };
  const blurOut = () => {
    (document.activeElement as HTMLElement | null)?.blur();
    el.querySelector('[role="region"]')!.dispatchEvent(new FocusEvent('focusout', { bubbles: true, relatedTarget: null }));
    fixture.detectChanges();
  };

  beforeEach(async () => {
    await TestBed.configureTestingModule({ imports: [RecentTraceTableComponent] }).compileComponents();
    fixture = TestBed.createComponent(RecentTraceTableComponent);
    el = fixture.nativeElement;
    fixture.componentInstance.connectionStatus = 'connected';
    push(mk(1, 'seed'));
  });

  it('twelve_traces_within_one_second_produce_one_polite_status_message', fakeAsync(() => {
    const region = el.querySelector('[data-trace-status]') as HTMLElement;
    expect(region.getAttribute('role')).toBe('status');
    expect(region.getAttribute('aria-live')).toBe('polite');
    push([...mk(12), ...mk(1, 'seed')]);
    expect(status()).toBe('');
    tick(1000);
    fixture.detectChanges();
    expect(status()).toBe('12 new traces');
    discardPeriodicTasks();
  }));

  it('pause_toggle_swaps_its_label_and_has_no_aria_pressed', () => {
    const pause = btn(/^Pause$/)!;
    expect(pause).toBeTruthy();
    expect(pause.hasAttribute('aria-pressed')).toBeFalse();
    pause.click();
    fixture.detectChanges();
    const resume = btn(/^Resume$/)!;
    expect(resume).toBeTruthy();
    expect(resume.hasAttribute('aria-pressed')).toBeFalse();
  });

  it('rows_are_held_while_focus_is_inside_the_table_and_flushed_newest_first_on_focus_out', fakeAsync(() => {
    focusRow();
    const before = ids();
    push([...mk(5), ...mk(1, 'seed')]);
    expect(ids()).toEqual(before);
    expect(status()).toBe('5 new traces waiting');
    expect(btn(/^Show 5 new$/)).toBeTruthy();
    blurOut();
    expect(ids().length).toBe(6);
    expect(ids()[0]).toBe('req_x000');
    expect(btn(/^Show \d+ new$/)).toBeUndefined();
    tick(1000);
    fixture.detectChanges();
    expect(status()).toBe('5 new traces');
    discardPeriodicTasks();
  }));

  it('updates_to_shown_rows_apply_in_place_while_held_and_show_new_inserts_the_held_rows', () => {
    focusRow();
    const row = el.querySelector('tbody tr[data-row]') as HTMLElement;
    push([...mk(2), { ...mk(1, 'seed')[0], statusCode: 503 }]);
    expect(el.querySelector('tbody tr[data-row]')).toBe(row);
    expect(ids().length).toBe(1);
    expect(row.textContent).toContain('503');
    btn(/^Show 2 new$/)!.click();
    fixture.detectChanges();
    expect(ids().length).toBe(3);
  });

  it('manual_pause_holds_until_resume_and_resume_does_not_flush_while_focus_is_still_inside', () => {
    btn(/^Pause$/)!.click();
    fixture.detectChanges();
    push([...mk(3), ...mk(1, 'seed')]);
    expect(ids().length).toBe(1);
    expect(status()).toContain('3 new traces waiting');
    focusRow();
    btn(/^Resume$/)!.click();
    fixture.detectChanges();
    expect(ids().length).withContext('focus is still inside the table').toBe(1);
    blurOut();
    expect(ids().length).toBe(4);
  });
});
