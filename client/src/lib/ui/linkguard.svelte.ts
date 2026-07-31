// Confirm-before-navigate for links inside rendered message markdown.
//
// Message bodies are {@html}-rendered, so their links can't carry Svelte
// handlers. A single delegated document listener catches clicks on
// `a[data-mdlink]` anywhere (message list, thread panel, search/pins modals)
// and routes them here, so the user sees the *real* destination before leaving
// — the anti-phishing guard for masked `[text](url)` links whose visible text
// can differ from where they actually point.

export const linkPrompt = $state<{ open: boolean; url: string; text: string }>({
  open: false,
  url: "",
  text: "",
});

export function askLink(url: string, text: string): void {
  linkPrompt.url = url;
  linkPrompt.text = text;
  linkPrompt.open = true;
}

export function closeLink(): void {
  linkPrompt.open = false;
}

// Navigate to the confirmed URL. A synthetic anchor click reproduces the exact
// behaviour of the original in-body link (new tab, noopener) across both the
// web build and the Tauri webview.
export function openConfirmed(): void {
  const url = linkPrompt.url;
  linkPrompt.open = false;
  if (!url) return;

  const a = document.createElement("a");
  a.href = url;
  a.target = "_blank";
  a.rel = "noopener noreferrer";
  a.click();
}

// Install the delegated listener once, at the app root. Returns a cleanup fn.
export function installLinkGuard(): () => void {
  const handler = (e: MouseEvent) => {
    // Let modified / non-primary clicks through (open-in-new-tab still works and
    // still lands on the real href).
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;

    const a = (e.target as HTMLElement)?.closest?.("a[data-mdlink]") as HTMLAnchorElement | null;
    if (!a) return;

    e.preventDefault();
    // `a.href` is the browser-resolved absolute URL — the real destination.
    askLink(a.href, a.textContent ?? a.href);
  };

  document.addEventListener("click", handler);
  return () => document.removeEventListener("click", handler);
}
