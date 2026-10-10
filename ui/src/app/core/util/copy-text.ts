/** Copy text to the clipboard; resolves false (never throws) when every strategy fails. */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through: permission denied or non-secure context
  }
  return execCommandCopy(text);
}

function execCommandCopy(text: string): boolean {
  const input = document.createElement('input');
  input.readOnly = true;
  input.value = text;
  input.setAttribute('data-copy-fallback', '');
  input.style.position = 'fixed';
  input.style.opacity = '0';
  document.body.appendChild(input);
  try {
    input.select();
    return document.execCommand('copy');
  } catch {
    return false;
  } finally {
    input.remove();
  }
}
