import { TestBed } from '@angular/core/testing';
import { SessionStore } from './session.store';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { provideHttpClient } from '@angular/common/http';
import { SessionData } from '../models/telemetry.models';

describe('SessionStore (TC-UNIT-11)', () => {
  let store: SessionStore;
  let httpMock: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [
        SessionStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    });

    store = TestBed.inject(SessionStore);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('TC-UNIT-11: should update sessions state on loadSessions HTTP response', () => {
    const mockSessions: SessionData[] = [
      {
        id: 'sess-1',
        turnCount: 5,
        tokenSavingsPercent: 42,
        lastActiveTimestamp: '2026-09-27T12:00:00Z',
        pinned: true,
        model: 'claude-3-5-sonnet',
        provider: 'anthropic'
      }
    ];

    store.loadSessions('test', 10);
    expect(store.isLoading()).toBeTrue();

    const req = httpMock.expectOne('/v1/dashboard/sessions?limit=10&search=test');
    expect(req.request.method).toBe('GET');
    req.flush(mockSessions);

    expect(store.isLoading()).toBeFalse();
    expect(store.sessions()).toEqual(mockSessions);

    store.selectSession('sess-1');
    expect(store.selectedSession()).toEqual(mockSessions[0]);
  });
});
