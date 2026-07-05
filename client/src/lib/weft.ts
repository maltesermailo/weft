// Thin typed wrapper over the Tauri commands + the `weft` event stream.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Mode = "login" | "register";

export type WeftEvent =
  | { kind: "connected"; network: string; account: string }
  | { kind: "auth-failed"; reason: string }
  | {
      kind: "message";
      target: string;
      sender: string;
      network: string;
      msgid: string;
      body: string;
      own: boolean;
      history: boolean;
    }
  | { kind: "batch-start"; id: string }
  | { kind: "batch-end"; id: string; truncated: boolean }
  | {
      kind: "member";
      channel: string;
      user: string;
      network: string;
      action: string;
      count: number | null;
    }
  | { kind: "policy"; channel: string; policy: string }
  | { kind: "edited"; target: string; sender: string; msgid: string; body: string }
  | { kind: "deleted"; target: string; msgid: string }
  | { kind: "reaction"; target: string; msgid: string; emoji: string; op: string; by: string }
  | {
      kind: "reactions";
      target: string;
      msgid: string;
      emoji: string;
      count: number;
      by: string[];
    }
  | {
      kind: "moderated";
      scope: string;
      account: string;
      action: string;
      by: string | null;
      reason: string | null;
    }
  | { kind: "error"; code: string; text: string }
  | { kind: "closed"; reason: string }
  | { kind: "raw"; line: string };

export function connect(host: string, account: string, password: string, mode: Mode) {
  return invoke("connect", { host, account, password, mode });
}

export function join(channel: string) {
  return invoke("join", { channel });
}

/// Auto-join every visible channel in a namespace (§6.2 NS JOIN).
export function nsJoin(name: string) {
  return invoke("ns_join", { name });
}

/// Request a page of history for `target`, older than `before` if given.
export function history(target: string, before?: string) {
  return invoke("history", { target, before: before ?? null });
}

export function sendMessage(target: string, body: string) {
  return invoke("send_message", { target, body });
}

export function sendRaw(line: string) {
  return invoke("send_raw", { line });
}

export function onWeft(cb: (e: WeftEvent) => void): Promise<UnlistenFn> {
  return listen<WeftEvent>("weft", (evt) => cb(evt.payload));
}
