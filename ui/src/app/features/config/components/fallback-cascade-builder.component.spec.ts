import { TestBed } from '@angular/core/testing';
import { FallbackCascadeBuilderComponent } from './fallback-cascade-builder.component';

describe('FallbackCascadeBuilderComponent (TC-UNIT-29)', () => {
  it('TC-UNIT-29: should re-order failover cascade priority', () => {
    const fixture = TestBed.createComponent(FallbackCascadeBuilderComponent);
    const component = fixture.componentInstance;

    fixture.componentRef.setInput('cascade', ['anthropic', 'openai', 'bedrock']);
    fixture.detectChanges();

    let emitted: string[] | undefined;
    component.cascadeChange.subscribe((val) => {
      emitted = val;
    });

    component.moveDown(0);
    expect(emitted).toEqual(['openai', 'anthropic', 'bedrock']);
  });
});
