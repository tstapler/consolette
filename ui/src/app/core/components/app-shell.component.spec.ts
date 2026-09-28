import { TestBed } from '@angular/core/testing';
import { AppShellComponent } from './app-shell.component';
import { provideRouter } from '@angular/router';
import { routes } from '../../app.routes';
import { SseService } from '../services/sse.service';

describe('AppShellComponent (TC-UNIT-12)', () => {
  let sseServiceMock: any;

  beforeEach(async () => {
    sseServiceMock = {
      connectionStatus: jasmine.createSpy('connectionStatus').and.returnValue('connected'),
      retryCount: jasmine.createSpy('retryCount').and.returnValue(0),
      connect: jasmine.createSpy('connect'),
      disconnect: jasmine.createSpy('disconnect')
    };

    await TestBed.configureTestingModule({
      imports: [AppShellComponent],
      providers: [
        provideRouter(routes),
        { provide: SseService, useValue: sseServiceMock }
      ]
    }).compileComponents();
  });

  it('TC-UNIT-12: should render top header, navigation links, and connected badge', () => {
    const fixture = TestBed.createComponent(AppShellComponent);
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('header')).toBeTruthy();
    expect(compiled.textContent).toContain('Consolette');
    expect(compiled.textContent).toContain('Overview');
    expect(compiled.textContent).toContain('Session Replay');
    expect(compiled.textContent).toContain('Model Benchmarks');
    expect(compiled.textContent).toContain('Config Editor');

    const badgeText = compiled.querySelector('[data-testid="connection-badge-text"]');
    expect(badgeText?.textContent?.toLowerCase()).toContain('connected');
  });

  it('TC-UNIT-12: should update connection badge when reconnecting', () => {
    sseServiceMock.connectionStatus.and.returnValue('reconnecting');
    sseServiceMock.retryCount.and.returnValue(2);

    const fixture = TestBed.createComponent(AppShellComponent);
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    const badgeText = compiled.querySelector('[data-testid="connection-badge-text"]');
    expect(badgeText?.textContent?.toLowerCase()).toContain('reconnecting');
    expect(badgeText?.textContent).toContain('#2');
  });
});
