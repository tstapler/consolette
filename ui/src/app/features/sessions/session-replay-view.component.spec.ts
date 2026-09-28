import { ComponentFixture, TestBed } from '@angular/core/testing';
import { HttpClientTestingModule } from '@angular/common/http/testing';
import { SessionReplayViewComponent } from './session-replay-view.component';
import { SessionStore } from '../../core/stores/session.store';

describe('SessionReplayViewComponent', () => {
  let component: SessionReplayViewComponent;
  let fixture: ComponentFixture<SessionReplayViewComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [SessionReplayViewComponent, HttpClientTestingModule],
      providers: [SessionStore]
    }).compileComponents();

    fixture = TestBed.createComponent(SessionReplayViewComponent);
    component = fixture.componentInstance;
    fixture.detectChanges();
  });

  it('should create session replay view component', () => {
    expect(component).toBeTruthy();
  });
});
