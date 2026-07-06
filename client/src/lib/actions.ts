// Reusable Svelte actions.

/// Focus a text field on mount, caret at end.
export function autofocus(node: HTMLTextAreaElement | HTMLInputElement) {
  node.focus();
  node.selectionStart = node.selectionEnd = node.value.length;
}
