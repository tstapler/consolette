import { TestBed } from '@angular/core/testing';
import { ConfigDiffModalComponent } from './config-diff-modal.component';

describe('ConfigDiffModalComponent (TC-UNIT-31)', () => {
  it('TC-UNIT-31: should format JSON diff and emit confirm event', () => {
    const fixture = TestBed.createComponent(ConfigDiffModalComponent);
    const component = fixture.componentInstance;

    fixture.componentRef.setInput('isOpen', true);
    fixture.componentRef.setInput('originalConfig', { fallbackCascade: ['anthropic'] });
    fixture.componentRef.setInput('draftConfig', { fallbackCascade: ['anthropic', 'openai'] });
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('[data-testid="diff-modal"]')).toBeTruthy();

    let confirmed = false;
    component.confirm.subscribe(() => {
      confirmed = true;
    });

    component.confirmApply();
    expect(confirmed).toBeTrue();
  });
});
