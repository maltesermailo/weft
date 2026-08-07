// The SDUI tree a plugin sends (plugin-spec.md §10–§11).
//
// These mirror `weft_proto::plugin` exactly: internally tagged on `type`, kebab
// case. The wire carries base64 CBOR; weft-client-core decodes it to JSON at the
// boundary, so everything here is plain JSON and the renderer never sees CBOR.
//
// Forward compatibility is the point of `Unknown`: a component from a newer
// server decodes rather than failing, and the renderer skips it. A plugin that
// sends something we cannot draw loses that one block, not the whole dialog.

export type ButtonStyle = "primary" | "default" | "danger";

export type Button = {
  id: string;
  label: string;
  style?: ButtonStyle;
  /** Confirm-before-fire prompt. Shown before the click is sent. */
  confirm?: string;
};

export type SelectOption = { value: string; label: string };
export type KvRow = { key: string; value: string };

export type Component =
  // inputs (§10.1)
  | {
      type: "text";
      id: string;
      label: string;
      required?: boolean;
      default?: string;
      placeholder?: string;
      multiline?: boolean;
      max_len?: number;
      pattern?: string;
    }
  | {
      type: "number";
      id: string;
      label: string;
      required?: boolean;
      default?: number;
      min?: number;
      max?: number;
      step?: number;
    }
  | { type: "select"; id: string; label: string; required?: boolean; default?: string; options: SelectOption[] }
  | {
      type: "multiselect";
      id: string;
      label: string;
      default?: string[];
      options: SelectOption[];
      min?: number;
      max?: number;
    }
  | { type: "toggle"; id: string; label: string; default?: boolean }
  | { type: "date"; id: string; label: string; required?: boolean; default?: string; min?: string; max?: string }
  // display (§10.2)
  | { type: "heading"; text: string; level?: number }
  | { type: "markdown"; text: string }
  | { type: "divider" }
  | { type: "keyvalue"; rows: KvRow[] }
  | { type: "table"; columns: string[]; rows: string[][]; dense?: boolean }
  | { type: "image"; src: string; alt?: string; max_height?: number }
  // controls (§10.3)
  | { type: "button" } & Button
  | { type: "action-row"; buttons: Button[] }
  | { type: "submit"; label?: string; style?: ButtonStyle }
  | { type: "unknown" };

export type Container = "modal" | "panel" | "custom";

export type View = {
  container: Container;
  title?: string;
  /** A panel's stable push handle (§11.3) — the plugin patches by this. */
  panel_key?: string;
  submit_label?: string;
  blocks?: Component[];
  /** `custom` only: the client-bundle asset ref to mount (§11.6). */
  widget?: string;
  params?: string[];
};

export type PatchOp =
  | { type: "replace"; view: View }
  | { type: "set"; component_id: string; props: View }
  | { type: "append"; container_id: string; blocks: Component[] }
  | { type: "remove"; component_id: string }
  | { type: "unknown" };

export type ViewResult =
  | { type: "toast"; kind: "ok" | "warn" | "error"; text: string }
  | { type: "navigate"; target: string }
  | { type: "close"; reason?: string }
  | { type: "refresh"; scope?: string };

/** Which surface an action appears on (§13.1). */
export type Surface =
  | "context-menu"
  | "slash"
  | "settings"
  | "global"
  | "server-menu"
  | "channel-list"
  | "channel-settings"
  | "admin";

export type ActionDecl = {
  id: string;
  label: string;
  icon?: string;
  surface: Surface;
  context: "message" | "channel" | "member" | "user" | "namespace" | "none";
  description?: string;
  visibility?: string;
  input?: Component[];
};

/// One plugin's catalog entry (§12.5). Field names are the wire's: the payload is
/// weftd's `CatalogEntry` serialized as-is, so this is `plugin_id`, not `id` —
/// reading `entry.id` yielded `undefined`, which keyed every plugin in the store
/// under the same missing key and handed `undefined` to `PLUGIN INVOKE`.
export type CatalogEntry = {
  plugin_id: string;
  name: string;
  icon?: string;
  actions: ActionDecl[];
  /// Foreign-URI schemes this provider serves. Empty ⇒ it governs no realm, so
  /// its surfaces are generic rather than tied to one realm's namespaces.
  schemes?: string[];
};
export type Catalog = { plugins: CatalogEntry[] };

/** A component's form id, or `null` for the display-only ones. */
export function fieldId(c: Component): string | null {
  switch (c.type) {
    case "text":
    case "number":
    case "select":
    case "multiselect":
    case "toggle":
    case "date":
      return c.id;
    default:
      return null;
  }
}

/**
 * The initial form state for a view: every input's declared default.
 *
 * Seeded up front rather than on first edit, so an untouched form submits the
 * defaults the plugin asked for instead of nothing — a plugin that pre-fills a
 * ban reason should get that reason back if the user just hits Confirm.
 */
export function initialValues(blocks: Component[] | undefined): Record<string, unknown> {
  const values: Record<string, unknown> = {};

  for (const c of blocks ?? []) {
    switch (c.type) {
      case "text":
      case "select":
      case "date":
        values[c.id] = c.default ?? "";
        break;
      case "number":
        values[c.id] = c.default ?? null;
        break;
      case "multiselect":
        values[c.id] = c.default ?? [];
        break;
      case "toggle":
        values[c.id] = c.default ?? false;
        break;
    }
  }

  return values;
}

/** Apply one §11.4 patch op to a view, returning the updated view. */
export function applyPatch(view: View, op: PatchOp): View {
  switch (op.type) {
    case "replace":
      return op.view;

    case "append":
      return { ...view, blocks: [...(view.blocks ?? []), ...op.blocks] };

    case "remove":
      return { ...view, blocks: (view.blocks ?? []).filter((c) => fieldId(c) !== op.component_id) };

    // `set` replaces one component's props. The wire types it as a `View` for
    // reuse, but only its `blocks[0]` is the replacement component.
    case "set": {
      const replacement = op.props.blocks?.[0];
      if (!replacement) return view;

      return {
        ...view,
        blocks: (view.blocks ?? []).map((c) => (fieldId(c) === op.component_id ? replacement : c)),
      };
    }

    // §10: an op from a newer server is skipped, not an error.
    default:
      return view;
  }
}

/**
 * Split a slash command's argument text into tokens.
 *
 * Quoted runs count as one token, so a free-text input can hold a phrase
 * (`/ban alice "being rude"`). The spec says a bare *token* fills the next
 * input; it does not say what a token is, and without quoting no positional
 * input could ever hold a space.
 */
export function tokenizeArgs(arg: string): string[] {
  return [...arg.matchAll(/"([^"]*)"|(\S+)/g)].map((m) => m[1] ?? m[2]);
}

/**
 * Map a slash command's arguments onto a declared action's inputs (§13.4,
 * decision §20-F): **both** forms are accepted.
 *
 * - `key:value` binds by input id, wherever it appears.
 * - a bare token fills the next input not already bound, by declaration order.
 *
 * A `key:value` whose key is not a declared input is treated as a bare token —
 * it is far likelier to be ordinary text containing a colon (a URL, a time) than
 * a typo'd field name, and silently dropping it would lose the user's words.
 */
export function slashParams(action: ActionDecl, arg: string): Record<string, unknown> {
  const inputs = (action.input ?? []).map(fieldId).filter((id): id is string => id !== null);
  const values: Record<string, unknown> = {};
  const positional: string[] = [];

  for (const token of tokenizeArgs(arg)) {
    const sep = token.indexOf(":");
    const key = sep > 0 ? token.slice(0, sep) : "";

    if (key && inputs.includes(key)) values[key] = token.slice(sep + 1);
    else positional.push(token);
  }

  for (const id of inputs) {
    if (id in values) continue;

    const next = positional.shift();
    if (next === undefined) break;
    values[id] = next;
  }

  return values;
}
