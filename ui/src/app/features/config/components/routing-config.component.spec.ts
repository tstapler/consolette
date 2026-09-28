import { TestBed } from '@angular/core/testing';
import { RoutingConfigComponent, ProviderConfigItem } from './routing-config.component';

describe('RoutingConfigComponent (TC-UNIT-28)', () => {
  it('TC-UNIT-28: should calculate total weight sum and preserve masked API keys', () => {
    const fixture = TestBed.createComponent(RoutingConfigComponent);
    const component = fixture.componentInstance;

    const sampleProviders: ProviderConfigItem[] = [
      { name: 'anthropic', apiKey: 'sk-ant-...****', weight: 60 },
      { name: 'openai', apiKey: 'sk-...****', weight: 40 }
    ];

    fixture.componentRef.setInput('providers', sampleProviders);
    fixture.detectChanges();

    expect(component.totalWeight).toBe(100);

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('Total Weight: 100%');

    let emitted: ProviderConfigItem[] | undefined;
    component.providersChange.subscribe((val) => {
      emitted = val;
    });

    component.onApiKeyChange('anthropic', 'sk-ant-...****');
    expect(emitted).toBeDefined();
    expect(emitted![0].apiKey).toBe('sk-ant-...****');
  });
});
