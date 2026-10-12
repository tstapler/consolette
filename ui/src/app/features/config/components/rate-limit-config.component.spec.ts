import { TestBed } from '@angular/core/testing';
import { RateLimitConfigComponent } from './rate-limit-config.component';

describe('RateLimitConfigComponent (TC-UNIT-30)', () => {
  it('TC-UNIT-30: should be read-only and explain where limits are configured', () => {
    const fixture = TestBed.createComponent(RateLimitConfigComponent);
    fixture.componentRef.setInput('rateLimits', { rpm: 120, tpm: 100000, concurrent: 10 });
    fixture.detectChanges();

    const el = fixture.nativeElement as HTMLElement;
    const inputs = Array.from(el.querySelectorAll('input')) as HTMLInputElement[];
    expect(inputs.length).toBe(3);
    expect(inputs.every((i) => i.disabled)).toBeTrue();
    expect(el.textContent).toContain(
      'Rate limits are configured in the route file; editing is not supported here'
    );
    expect(el.textContent).not.toContain('Must be a non-negative integer');
  });
});
