import { ComponentFixture, TestBed } from '@angular/core/testing';
import { BenchmarkScorecardComponent } from './benchmark-scorecard.component';
import { BenchmarkData } from '../../../core/models/telemetry.models';

describe('BenchmarkScorecardComponent', () => {
  let component: BenchmarkScorecardComponent;
  let fixture: ComponentFixture<BenchmarkScorecardComponent>;

  const mockData: BenchmarkData[] = [
    {
      model: 'claude-3-5-sonnet',
      provider: 'anthropic',
      ttftP50: 120,
      ttftP90: 150,
      ttftP95: 180,
      ttftP99: 210,
      speedTokSec: 45,
      successRatePercent: 99.5,
      costPer1k: 0.015,
      tokensSavedPercent: 30,
      aiderScore: 88.0
    },
    {
      model: 'gpt-4o',
      provider: 'openai',
      ttftP50: 140,
      ttftP90: 170,
      ttftP95: 200,
      ttftP99: 240,
      speedTokSec: 40,
      successRatePercent: 98.0,
      costPer1k: 0.02,
      tokensSavedPercent: 20,
      aiderScore: 85.0
    }
  ];

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [BenchmarkScorecardComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(BenchmarkScorecardComponent);
    component = fixture.componentInstance;
    component.data = mockData;
    fixture.detectChanges();
  });

  it('should create component', () => {
    expect(component).toBeTruthy();
  });

  it('should sort data by column headers', () => {
    component.sort('model');
    expect(component.sortFieldSignal()).toBe('model');

    const sorted = component.sortedData();
    expect(sorted.length).toBe(2);
  });

  it('should toggle sort direction when sorting the same column twice', () => {
    component.sort('model');
    const initialDirection = component.sortDirectionSignal();

    component.sort('model');
    expect(component.sortDirectionSignal()).not.toBe(initialDirection);
  });
});
