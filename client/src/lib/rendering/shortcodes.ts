// Standard (unicode) emoji shortcodes — `:smile:` → 😄 etc. — via node-emoji's
// GitHub-style shortcode dataset. Custom per-server emoji are handled separately
// (they resolve to an image, and always take precedence over a unicode name).
import { get, search, which } from "node-emoji";

/** `:name:` → the unicode character, or undefined if it isn't a known shortcode. */
export function shortcodeToChar(name: string): string | undefined {
  return get(name.replace(/:/g, ""));
}

/** Fuzzy shortcode search for the `:` autocomplete. */
export function searchUnicode(query: string, limit = 12): { name: string; char: string }[] {
  if (!query) return [];
  return search(query)
    .slice(0, limit)
    .map((r) => ({ name: r.name, char: r.emoji }));
}

/** The shortcode name for a unicode character (for picker labels / previews). */
export function charName(char: string): string | undefined {
  return which(char);
}
