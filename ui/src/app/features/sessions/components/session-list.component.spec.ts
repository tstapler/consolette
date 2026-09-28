import { ComponentFixture, TestBed } from '@angular/core/testing';
import { SessionListComponent } from './session-list.component';
import { SessionData } from '../../../core/models/telemetry.models';

describe('SessionListComponent', () => {
  let component: SessionListComponent;
  let fixture: ComponentFixture<SessionListComponent>;

  const mockSessions: SessionData[] = [
    {
      id: 'sess-1',
      turnCount: 5,
      tokenSavingsPercent: 35.5,
      lastActiveTimestamp: '2026-09-27T14:00:00Z',
      pinned: true,
      model: 'claude-3-5-sonnet',
      provider: 'anthropic'
    },
    {
      id: 'sess-2',
      turnCount: 2,
      tokenSavingsPercent: 12.0,
      lastActiveTimestamp: '2026-09-27T14:15:00Z',
      pinned: false,
      model: 'gpt-4o',
      provider: 'openai'
    }
  ];

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [SessionListComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(SessionListComponent);
    component = fixture.componentInstance;
    component.sessions = mockSessions;
    fixture.detectChanges();
  });

  it('should create session list component', () => {
    expect(component).toBeTruthy();
  });

  it('should render sessions in table', () => {
    const compiled = fixture.nativeElement as HTMLElement;
    const rows = compiled.querySelectorAll('tbody tr');
    expect(rows.length).toBe(2);
  });

  it('should filter sessions by search query', () => {
    component.onSearchChange('openai');
    fixture.detectChanges();

    expect(component.filteredSessions().length).toBe(1);
    expect(component.filteredSessions()[0].id).toBe('sess-2');
  });

  it('should emit sessionSelected on click', () => {
    spyOn(component.sessionSelected, 'emit');
    component.selectSession(mockSessions[0]);
    expect(component.sessionSelected.emit).toHaveBeenCalledWith(mockSessions[0]);
  });
});
