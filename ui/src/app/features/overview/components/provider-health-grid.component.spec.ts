import { ComponentFixture, TestBed } from '@angular/core/testing';
import { ProviderHealthGridComponent } from './provider-health-grid.component';

describe('ProviderHealthGridComponent (TC-UNIT-14)', () => {
  let component: ProviderHealthGridComponent;
  let fixture: ComponentFixture<ProviderHealthGridComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [ProviderHealthGridComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(ProviderHealthGridComponent);
    component = fixture.componentInstance;
  });

  it('should create the component', () => {
    expect(component).toBeTruthy();
  });

  it('should render default healthy providers list if no input is passed', () => {
    component.ngOnChanges({});
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('Anthropic');
    expect(compiled.textContent).toContain('AWS Bedrock');
    expect(compiled.textContent).toContain('OpenAI');
  });

  it('should render status badges for Healthy, Degraded, Cooldown, and AuthError states', () => {
    component.providersList = [
      { name: 'Anthropic', status: 'Healthy' },
      { name: 'Bedrock', status: 'Degraded' },
      { name: 'OpenAI', status: 'Cooldown', cooldownSeconds: 25 },
      { name: 'Groq', status: 'AuthError', lastError: '401 Unauthorized' }
    ];
    component.ngOnChanges({});
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('Active');
    expect(compiled.textContent).toContain('Degraded');
    expect(compiled.textContent).toContain('In Cooldown');
    expect(compiled.textContent).toContain('Auth Error');
    expect(compiled.textContent).toContain('Cooldown: 25s remaining');
    expect(compiled.textContent).toContain('401 Unauthorized');
  });

  it('should handle providerHealth dictionary input', () => {
    component.providerHealth = {
      Anthropic: 'Healthy',
      OpenAI: 'Cooldown'
    };
    component.ngOnChanges({});
    fixture.detectChanges();

    expect(component.providers.length).toBe(2);
    expect(component.getStatusLabel(component.providers[0].status)).toBe('Active');
    expect(component.getStatusLabel(component.providers[1].status)).toBe('In Cooldown');
  });

  it('should clean up timer interval on destroy', () => {
    component.providersList = [
      { name: 'OpenAI', status: 'Cooldown', cooldownSeconds: 10 }
    ];
    component.ngOnChanges({});
    fixture.detectChanges();

    expect((component as any).intervalId).not.toBeNull();

    component.ngOnDestroy();
    expect((component as any).intervalId).toBeNull();
  });
});
