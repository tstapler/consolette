import { TestBed } from '@angular/core/testing';
import { RateLimitConfigComponent } from './rate-limit-config.component';

describe('RateLimitConfigComponent (TC-UNIT-30)', () => {
  it('TC-UNIT-30: should validate numerical inputs for rate limits', () => {
    const fixture = TestBed.createComponent(RateLimitConfigComponent);
    const component = fixture.componentInstance;

    fixture.componentRef.setInput('rateLimits', { rpm: 120, tpm: 100000, concurrent: 10 });
    fixture.detectChanges();

    expect(component.isInvalid(120)).toBeFalse();
    expect(component.isInvalid(-5)).toBeTrue();

    let emitted: any;
    component.rateLimitsChange.subscribe((val) => {
      emitted = val;
    });

    component.onRpmChange(150);
    expect(emitted?.rpm).toBe(150);
  });
});
