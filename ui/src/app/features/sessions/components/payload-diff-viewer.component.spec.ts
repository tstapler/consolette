import { ComponentFixture, TestBed } from '@angular/core/testing';
import { PayloadDiffViewerComponent } from './payload-diff-viewer.component';

describe('PayloadDiffViewerComponent', () => {
  let component: PayloadDiffViewerComponent;
  let fixture: ComponentFixture<PayloadDiffViewerComponent>;

  const mockOriginal = { model: 'claude-3-5-sonnet', prompt: 'Hello world long text' };
  const mockCompressed = { model: 'claude-3-5-sonnet', prompt: 'Hello world' };

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [PayloadDiffViewerComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(PayloadDiffViewerComponent);
    component = fixture.componentInstance;
    component.originalPayload = mockOriginal;
    component.compressedPayload = mockCompressed;
    component.ngOnChanges({
      originalPayload: { currentValue: mockOriginal, previousValue: null, firstChange: true, isFirstChange: () => true },
      compressedPayload: { currentValue: mockCompressed, previousValue: null, firstChange: true, isFirstChange: () => true }
    });
    fixture.detectChanges();
  });

  it('should create diff viewer component', () => {
    expect(component).toBeTruthy();
  });

  it('should compute diff lines', () => {
    expect(component.sideBySidePairs().length).toBeGreaterThan(0);
    expect(component.unifiedLines().length).toBeGreaterThan(0);
  });

  it('should toggle view mode', () => {
    component.setMode('inline');
    expect(component.viewMode()).toBe('inline');
    component.setMode('side-by-side');
    expect(component.viewMode()).toBe('side-by-side');
  });
});
