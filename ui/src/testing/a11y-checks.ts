/**
 * In-house accessibility checks over Karma-rendered DOM (no axe-core dependency).
 * Each helper throws via Jasmine `fail`, listing every offender so one run shows them all.
 * Colours go through a canvas so oklch()/color() computed values resolve to sRGB.
 */

type Rgba = [number, number, number, number];

const PAGE_BG: Rgba = [9, 13, 22, 1]; // --bg-primary in styles.css

const FOCUSABLE = 'a[href], button, input, select, textarea, summary, [tabindex]';

let ctx: CanvasRenderingContext2D | null = null;

export function parseColor(css: string): Rgba {
  ctx ??= document.createElement('canvas').getContext('2d', { willReadFrequently: true });
  const c = ctx as CanvasRenderingContext2D;
  c.clearRect(0, 0, 1, 1);
  c.fillStyle = '#000';
  c.fillStyle = css;
  c.fillRect(0, 0, 1, 1);
  const d = c.getImageData(0, 0, 1, 1).data;
  const a = d[3] / 255;
  return a === 0 ? [0, 0, 0, 0] : [d[0], d[1], d[2], a];
}

function over(fg: Rgba, bg: Rgba): Rgba {
  const a = fg[3];
  return [0, 1, 2].map(i => Math.round(fg[i] * a + bg[i] * (1 - a))).concat(1) as Rgba;
}

export function luminance([r, g, b]: Rgba): number {
  const f = (v: number) => { v /= 255; return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}

export function contrastRatio(a: Rgba, b: Rgba): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** Opaque background an element is painted on: translucent layers are composited down to the page colour. */
export function resolveBackground(el: Element | null): Rgba {
  const layers: Rgba[] = [];
  for (let n: Element | null = el; n; n = n.parentElement) {
    const bg = parseColor(getComputedStyle(n).backgroundColor);
    if (bg[3] > 0) layers.push(bg);
    if (bg[3] === 1) break;
  }
  return layers.reduceRight((under, layer) => over(layer, under), PAGE_BG);
}

/** Product of `opacity` up the ancestor chain: opacity lowers text contrast without changing `color`. */
function effectiveOpacity(el: Element): number {
  let o = 1;
  for (let n: Element | null = el; n; n = n.parentElement) o *= parseFloat(getComputedStyle(n).opacity);
  return o;
}

function describe_(el: Element): string {
  const label = el.getAttribute('aria-label') ?? (el.textContent ?? '').replace(/\s+/g, ' ').trim().slice(0, 30);
  return `<${el.tagName.toLowerCase()}${el.id ? '#' + el.id : ''}> "${label}"`;
}

function visible(el: Element): boolean {
  const r = el.getBoundingClientRect();
  const cs = getComputedStyle(el);
  return r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none';
}

function focusables(root: Element): HTMLElement[] {
  return Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(e =>
    visible(e) && !(e as HTMLInputElement).disabled && e.getAttribute('tabindex') !== '-1' && e.closest('[inert]') === null);
}

function report(check: string, offenders: string[]): void {
  if (offenders.length) fail(`${check}: ${offenders.length} offender(s)\n  ${offenders.join('\n  ')}`);
}

function accessibleName(el: HTMLElement): string {
  const labelledby = el.getAttribute('aria-labelledby');
  if (labelledby) {
    const t = labelledby.split(/\s+/).map(id => document.getElementById(id)?.textContent ?? '').join(' ').trim();
    if (t) return t;
  }
  const aria = el.getAttribute('aria-label')?.trim();
  if (aria) return aria;
  if (el instanceof HTMLInputElement || el instanceof HTMLSelectElement || el instanceof HTMLTextAreaElement) {
    const labels = Array.from(el.labels ?? []).map(l => l.textContent ?? '').join(' ').trim();
    if (labels) return labels;
    return el.getAttribute('title')?.trim() ?? '';
  }
  return ((el.textContent ?? '').replace(/\s+/g, ' ').trim() || el.getAttribute('title')?.trim()) ?? '';
}

/** Every button, link, input, select, textarea and region/dialog has a non-empty accessible name (a placeholder alone does not count). */
export function expectAccessibleNames(root: Element): void {
  const named = Array.from(root.querySelectorAll<HTMLElement>('a[href], button, input, select, textarea, [role="region"], [role="dialog"]'))
    .filter(e => visible(e) && (e as HTMLInputElement).type !== 'hidden');
  report('expectAccessibleNames', named.filter(e => accessibleName(e) === '').map(describe_));
}

/** Every rendered text node meets 4.5:1 (3:1 for large or bold-large text) against its resolved background. */
export function expectContrast(root: Element, ignore: (el: Element) => boolean = () => false): void {
  const bad: string[] = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const el = n.parentElement;
    if (!el || !(n.textContent ?? '').trim() || !visible(el) || ignore(el)) continue;
    if (el.closest('.sr-only, [aria-hidden="true"], :disabled, [aria-disabled="true"]')) continue; // disabled controls are exempt (WCAG 1.4.3)
    const cs = getComputedStyle(el);
    const bg = resolveBackground(el);
    const text = parseColor(cs.color);
    if (text[3] === 0) continue; // transparent text is gradient/clip text; its fill is not a computed colour
    text[3] *= effectiveOpacity(el);
    const fg = over(text, bg);
    const px = parseFloat(cs.fontSize);
    const large = px >= 24 || (px >= 18.66 && parseInt(cs.fontWeight, 10) >= 700);
    const ratio = contrastRatio(fg, bg);
    if (ratio < (large ? 3 : 4.5)) bad.push(`${describe_(el)} ${ratio.toFixed(2)}:1 (${cs.color} on rgb(${bg.slice(0, 3)}))`);
  }
  report('expectContrast', bad);
}

/** Every focusable control shows a >=2px solid outline on focus whose colour is >=3:1 against the surrounding background. */
export function expectFocusRing(root: Element): void {
  const bad: string[] = [];
  const previous = document.activeElement as HTMLElement | null;
  for (const el of focusables(root)) {
    el.focus();
    if (document.activeElement !== el) { bad.push(`${describe_(el)} could not take focus`); continue; }
    const cs = getComputedStyle(el);
    const width = parseFloat(cs.outlineWidth);
    if (cs.outlineStyle === 'none' || cs.outlineStyle === 'hidden' || !(width >= 2)) {
      bad.push(`${describe_(el)} outline ${cs.outlineWidth} ${cs.outlineStyle}`);
      continue;
    }
    const surround = resolveBackground(el.parentElement);
    const ratio = contrastRatio(over(parseColor(cs.outlineColor), surround), surround);
    if (ratio < 3) bad.push(`${describe_(el)} ring ${cs.outlineColor} is ${ratio.toFixed(2)}:1 on its surround`);
  }
  (document.activeElement as HTMLElement | null)?.blur();
  previous?.focus?.();
  report('expectFocusRing', bad);
}

/** Every focusable control is at least 24x24 CSS px (WCAG 2.5.8; checkboxes and range inputs count by their box). */
export function expectTargetSize(root: Element): void {
  const bad: string[] = [];
  for (const e of focusables(root)) {
    e.focus(); // skip links grow on focus
    const r = e.getBoundingClientRect();
    if (r.width < 23.5 || r.height < 23.5) bad.push(`${describe_(e)} ${Math.round(r.width)}x${Math.round(r.height)}`);
  }
  (document.activeElement as HTMLElement | null)?.blur();
  report('expectTargetSize', bad);
}

/**
 * Non-text contrast (WCAG 1.4.11): each matched element's fill (or border when it has no fill) is >=3:1 against the
 * background behind it. Defaults to elements marked `[data-bar]` and `[data-status-dot]`.
 */
export function expectNonTextContrast(root: Element, selector = '[data-bar], [data-status-dot]'): void {
  const bad: string[] = [];
  for (const el of Array.from(root.querySelectorAll(selector)).filter(visible)) {
    const cs = getComputedStyle(el);
    const behind = resolveBackground(el.parentElement);
    const fill = parseColor(cs.backgroundColor);
    const border = parseFloat(cs.borderTopWidth) > 0 ? parseColor(cs.borderTopColor) : null;
    const paint = fill[3] > 0 ? over(fill, behind) : border ? over(border, behind) : null;
    if (!paint) { bad.push(`${describe_(el)} has neither fill nor border`); continue; }
    const ratio = contrastRatio(paint, behind);
    if (ratio < 3) bad.push(`${describe_(el)} ${ratio.toFixed(2)}:1`);
  }
  report('expectNonTextContrast', bad);
}

/**
 * Forced-colors safety, source level (Karma cannot toggle the media feature): given the app stylesheet text, forced-color-adjust
 * must never be `none`, and every `[data-bar]` / `[data-status-dot]` element rendered under `root` must carry a border so it
 * survives the background-colour override.
 */
export function expectForcedColorsSafe(root: Element, stylesheetText: string, selector = '[data-bar], [data-status-dot]'): void {
  const bad: string[] = [];
  if (/forced-color-adjust\s*:\s*none/.test(stylesheetText)) bad.push('stylesheet sets forced-color-adjust: none');
  for (const el of Array.from(root.querySelectorAll(selector)).filter(visible)) {
    const cs = getComputedStyle(el);
    if (!(parseFloat(cs.borderTopWidth) > 0) || cs.borderTopStyle === 'none') bad.push(`${describe_(el)} has no border (invisible under forced-colors)`);
  }
  report('expectForcedColorsSafe', bad);
}

/**
 * Reduced motion (WCAG 2.3.3, UX-27): no rendered element may carry a bare `animate-*` utility (it must be
 * `motion-safe:animate-*`), and the app stylesheet must contain a `prefers-reduced-motion: reduce` rule that
 * zeroes transitions. Karma cannot flip the media feature, so this is a source-level check.
 */
export function expectReducedMotionSafe(root: Element): void {
  const bad = Array.from(root.querySelectorAll('[class]'))
    .filter(e => Array.from(e.classList).some(c => c.startsWith('animate-')))
    .map(describe_);
  const reduceRules = Array.from(document.styleSheets).flatMap(sheet => {
    try { return Array.from(sheet.cssRules); } catch { return []; }
  }).filter((r): r is CSSMediaRule => r instanceof CSSMediaRule && /prefers-reduced-motion:\s*reduce/.test(r.conditionText));
  const zeroes = reduceRules.some(r => /transition/.test(r.cssText) && /animation/.test(r.cssText));
  if (!zeroes) bad.push('no @media (prefers-reduced-motion: reduce) rule zeroing animation and transition');
  report('expectReducedMotionSafe', bad);
}
