// Reusable Svelte actions.

/// Focus a text field on mount, caret at end.
export function autofocus(node: HTMLTextAreaElement | HTMLInputElement) {
  node.focus();
  node.selectionStart = node.selectionEnd = node.value.length;
}

/// Click-to-reveal for `||spoiler||` spans (delegated, since message bodies are
/// rendered via {@html} and can't carry Svelte handlers). Reveals the clicked
/// spoiler; keyboard-accessible via the span's role="button" + tabindex.
export function spoilerReveal(node: HTMLElement) {
  const reveal = (e: Event) => {
    const sp = (e.target as HTMLElement)?.closest?.(".spoiler");
    if (sp && !sp.classList.contains("revealed")) {
      if (e instanceof KeyboardEvent && e.key !== "Enter" && e.key !== " ") return;
      sp.classList.add("revealed");
      e.preventDefault();
    }
  };
  node.addEventListener("click", reveal);
  node.addEventListener("keydown", reveal);
  return {
    destroy() {
      node.removeEventListener("click", reveal);
      node.removeEventListener("keydown", reveal);
    },
  };
}
