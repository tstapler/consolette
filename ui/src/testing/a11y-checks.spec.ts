import { contrastRatio, parseColor, resolveBackground } from './a11y-checks';

describe('a11y-checks helpers', () => {
  it('parseColor_resolves_hex_rgb_and_oklch_to_srgb', () => {
    expect(parseColor('#7dd3fc').slice(0, 3)).toEqual([125, 211, 252]);
    expect(parseColor('rgb(9, 13, 22)').slice(0, 3)).toEqual([9, 13, 22]);
    const grey = parseColor('oklch(0.556 0 0)'); // Tailwind neutral-500
    grey.slice(0, 3).forEach(c => expect(Math.abs(c - 115)).toBeLessThanOrEqual(2));
  });

  it('contrastRatio_matches_wcag_reference_values', () => {
    expect(contrastRatio([0, 0, 0, 1], [255, 255, 255, 1])).toBeCloseTo(21, 1);
    expect(contrastRatio([119, 119, 119, 1], [255, 255, 255, 1])).toBeCloseTo(4.48, 1);
  });

  it('resolveBackground_composites_translucent_layers_down_to_the_page_colour', () => {
    const outer = document.createElement('div');
    outer.style.backgroundColor = 'rgba(255, 255, 255, 0.5)';
    const inner = document.createElement('span');
    outer.appendChild(inner);
    document.body.appendChild(outer);
    const [r, g, b, a] = resolveBackground(inner);
    outer.remove();
    expect(a).toBe(1);
    expect([r, g, b]).toEqual([132, 134, 139]);
  });
});
