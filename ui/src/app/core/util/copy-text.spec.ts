import { copyText } from './copy-text';

describe('copyText', () => {
  function setClipboard(value: unknown): void {
    Object.defineProperty(navigator, 'clipboard', { value, configurable: true });
  }
  afterEach(() => {
    delete (navigator as unknown as Record<string, unknown>)['clipboard'];
  });

  it('writes through the async clipboard API when available', async () => {
    const writeText = jasmine.createSpy('writeText').and.resolveTo();
    setClipboard({ writeText });
    expect(await copyText('req_full_id')).toBeTrue();
    expect(writeText).toHaveBeenCalledOnceWith('req_full_id');
  });

  it('copy_text_should_fall_back_to_exec_command_when_clipboard_api_missing_and_announce_copy_failed_without_copied_when_both_fail', async () => {
    setClipboard(undefined);
    const exec = spyOn(document, 'execCommand').and.returnValue(true);
    expect(await copyText('abc')).toBeTrue();
    expect(exec).toHaveBeenCalledWith('copy');
    expect(document.querySelector('input[readonly][data-copy-fallback]')).toBeNull();

    exec.and.returnValue(false);
    expect(await copyText('abc')).toBeFalse();

    setClipboard({ writeText: () => Promise.reject(new Error('denied')) });
    exec.and.returnValue(true);
    expect(await copyText('abc')).toBeTrue();
    exec.and.throwError('boom');
    expect(await copyText('abc')).toBeFalse();
  });
});
