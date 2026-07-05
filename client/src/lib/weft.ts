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
    }
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
  | { kind: "error"; code: string; text: string }
  | { kind: "closed"; reason: string }
  | { kind: "raw"; line: string };

export function connect(host: string, account: string, password: string, mode: Mode) {
  return invoke("connect", { host, account, password, mode });
}

export function join(channel: string) {
  return invoke("join", { channel });
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
