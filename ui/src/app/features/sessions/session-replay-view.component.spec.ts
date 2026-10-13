import { ComponentFixture, TestBed } from '@angular/core/testing';
import { HttpClientTestingModule, HttpTestingController } from '@angular/common/http/testing';
import { SessionReplayViewComponent } from './session-replay-view.component';
import { SessionStore } from '../../core/stores/session.store';
import { SessionData } from '../../core/models/telemetry.models';

const session: SessionData = {
  id: 'sess-1',
  latestRequestId: 'req_77ab01cd',
  turnCount: 2,
  tokenSavingsPercent: 0,
  lastActiveTimestamp: '2026-10-12T10:00:00Z',
  pinned: false,
  model: 'claude-sonnet-4',
  provider: 'anthropic'
};

describe('SessionReplayViewComponent', () => {
  let component: SessionReplayViewComponent;
  let fixture: ComponentFixture<SessionReplayViewComponent>;
  let http: HttpTestingController;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [SessionReplayViewComponent, HttpClientTestingModule],
      providers: [SessionStore]
    }).compileComponents();

    fixture = TestBed.createComponent(SessionReplayViewComponent);
    component = fixture.componentInstance;
    http = TestBed.inject(HttpTestingController);
    fixture.detectChanges();
    http.expectOne((r) => r.url === '/v1/dashboard/sessions').flush([session]);
    fixture.detectChanges();
  });

  it('should create session replay view component', () => {
    expect(component).toBeTruthy();
  });

  it('should list the sessions on first load without any search input (E-03)', () => {
    const rows = (fixture.nativeElement as HTMLElement).querySelectorAll('tbody tr');
    expect(rows.length).toBe(1);
    expect((rows[0] as HTMLElement).textContent).toContain('sess-1');
  });

  it('replay_should_request_by_requestId_and_label_missing_compressed_payload_with_reason', () => {
    component.onSessionSelected(session);
    const original = http.expectOne('/requests/req_77ab01cd?stage=original');
    const compressed = http.expectOne('/requests/req_77ab01cd?stage=compressed');
    http.expectNone('/requests/sess-1?stage=original');
    original.flush({ model: 'claude-sonnet-4' });
    compressed.flush(null, { status: 404, statusText: 'Not Found' });
    fixture.detectChanges();

    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('Compressed payload not captured');
    expect(text).toContain('compression is off');
  });

  it('should word the reason for compressionStatus none as nothing was compressed', () => {
    const none = { ...session, compressionStatus: 'none' as const };
    component.onSessionSelected(none);
    http.expectOne('/requests/req_77ab01cd?stage=original').flush({});
    http
      .expectOne('/requests/req_77ab01cd?stage=compressed')
      .flush(null, { status: 404, statusText: 'Not Found' });
    expect(component.compressedMissingReason).toBe('nothing was compressed');
  });

  it('timeline_and_token_bar_should_render_not_recorded_and_hide_hardcoded_defaults', () => {
    component.onSessionSelected(session);
    http.expectOne('/requests/req_77ab01cd?stage=original').flush({});
    http.expectOne('/requests/req_77ab01cd?stage=compressed').flush({});
    fixture.detectChanges();

    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect((text.match(/Not recorded/g) ?? []).length).toBeGreaterThanOrEqual(2);
    expect(text).not.toContain('Ingress');
    expect(text).not.toContain('1,250');
    expect(text).not.toContain('203');
  });

  it('should render real timeline and token values when the session carries them', () => {
    const real = {
      ...session,
      promptTokens: 700,
      responseTokens: 90,
      savedTokens: 0,
      timelineSteps: [{ name: 'Router', durationMs: 3, status: 'ok' as const }]
    };
    component.sessionStore.setSessions([real]);
    component.onSessionSelected(real);
    http.expectOne('/requests/req_77ab01cd?stage=original').flush({});
    http.expectOne('/requests/req_77ab01cd?stage=compressed').flush({});
    fixture.detectChanges();

    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('Router');
    expect(text).toContain('700');
  });
});
