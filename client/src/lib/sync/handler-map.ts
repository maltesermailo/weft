// The EventHandler model: each domain exports a `HandlerMap` — a partial,
// per-kind map of wire-event handlers (each narrowed to its event variant). The
// reducer merges the domain maps into one registry and dispatches by `e.kind`,
// so a domain owns its own event handling next to its state.
import type { WeftEvent } from "$lib/transport/weft";

export type HandlerMap = {
  [K in WeftEvent["kind"]]?: (e: Extract<WeftEvent, { kind: K }>) => void;
};
