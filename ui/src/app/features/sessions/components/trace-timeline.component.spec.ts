import { ComponentFixture, TestBed } from '@angular/core/testing';
import { TraceTimelineComponent } from './trace-timeline.component';

describe('TraceTimelineComponent', () => {
  let component: TraceTimelineComponent;
  let fixture: ComponentFixture<TraceTimelineComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [TraceTimelineComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(TraceTimelineComponent);
    component = fixture.componentInstance;
    fixture.detectChanges();
  });

  it('should create trace timeline component', () => {
    expect(component).toBeTruthy();
  });

  it('should calculate total duration', () => {
    expect(component.totalDurationMs).toBe(165);
  });
});
