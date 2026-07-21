# WEFT Console — HTML template pack

Reference templates for building the WEFT admin panel ("WEFT Console").
This pack is meant to be read by a coding agent (Claude Code) or a human
implementing the real panel. It is framework-agnostic static HTML + one CSS
file; port the markup into whatever templating/component system the panel
uses (Askama/Maud/Tera on the Rust side, or a JS framework) without changing
the class names or visual output.

## Files

| File                    | What it is |
|-------------------------|------------|
| `weft.css`              | The entire design system. Single source of truth. |
| `layout.html`           | Page shell: selvage, sidebar, header, content column. Every page is this shell with a different `<!-- @slot content -->`. |
| `components.html`       | Visual gallery of every component with usage notes. Open in a browser to see everything at once. |
| `page-search-list.html` | Template: search box + result list + pager (Users, Rooms, Applications lookups). |
| `page-detail.html`      | Template: detail page with key/value card, data table card, flags card (User detail, Room detail). |
| `page-moderation.html`  | Template: stacked action forms incl. danger zone (Room Actions, account actions). |
| `page-data-table.html`  | Template: full-width table view with status knots (Federation Peers, Transit Queue, Devices, Audit Log). |

## Conventions (follow these exactly)

1. **Placeholders.** Dynamic values are written as `{{name}}`. Repeatable
   regions are wrapped in `<!-- @each item in items -->…<!-- @end -->`.
   Named insertion points are `<!-- @slot name -->`. Translate these to the
   real template engine's syntax 1:1.
2. **Type discipline.** Machine-readable text (IDs, handles, fingerprints,
   timestamps, addresses, scopes) always gets `.mono` or is inside an
   element already set in the mono face (`.fp`, `.r-sub`, `.kv .v.mono`).
   Human prose stays in the sans face.
3. **The accent is scarce.** `--thread` (gold) appears only on: active nav
   item, the one primary button per view, focus rings, checked flags,
   `.pill.gold`, the shuttle bar, selvage, and the header weft line. Never
   use it for body text or decorative fills.
4. **Status vocabulary.** Connection/health states use WEFT weaving terms
   via `.knot`: `woven` (healthy), `frayed` (degraded), `severed`
   (blocked), `idle` (dormant). Do not write online/offline/error.
5. **Signature elements are load-bearing.** The `.selvage` strip, the
   header's interlaced weft line (`header::after`), and the `.shuttle` bar
   in card headers are the visual identity. Keep them on every page.
6. **Danger styling.** Irreversible actions use `.btn.danger` (outlined)
   for structural actions and `.btn.danger.solid` only for permanent
   deletes, inside an `.action.danger` block. Destructive endpoints should
   pair with typed-name confirmation in the real implementation.
7. **One `h1` per page**, inside `.h-row`, with optional `.id` (mono
   identifier) next to it and `.meta` (result count / context) pushed right.
8. **Accessibility floor.** Keep `:focus-visible` styles, `aria-label`s on
   icon-only/search inputs, `aria-current="page"` on the active nav item,
   and respect `prefers-reduced-motion` (already handled in the CSS).

## Mapping pages to templates

| Sidebar item        | Template |
|---------------------|----------|
| Users, Rooms, Applications | `page-search-list.html` → `page-detail.html` |
| Peers, Transit Queue, Remote Rooms | `page-data-table.html` |
| Devices, Capability Tokens, Revocations | `page-data-table.html` (tokens detail may embed `page-detail.html` cards) |
| Reports | `page-data-table.html` with a claim/resolve action column |
| Room Actions, account suspend/delete | `page-moderation.html` |
| Phrase Bans, Media Blocklist | `page-search-list.html` variant with add-form on top |
| IRC Bridge | `page-detail.html` cards for link status + `page-data-table.html` for channel mappings |
| QUIC Transport, Audit Log | `page-data-table.html` |
