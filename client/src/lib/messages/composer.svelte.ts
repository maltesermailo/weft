// Message composition + inline editing: the composer draft, attachment tray,
// @-mention and :emoji: autocomplete, optimistic send/slash-commands, typing
// indicators, and the inline edit / reaction actions. Shared by `Composer` and
// `MessageItem`, so the state lives here rather than in either component.
import type { Msg, MentionOpt } from "$lib/types";
import * as media from "$lib/media/media";
import { view } from "$lib/navigation/view.svelte";
import { store } from "$lib/store/store.svelte";
import { ui } from "$lib/ui/ui.svelte";
import * as weft from "$lib/transport/weft";
import { toast } from "$lib/notifications/toasts.svelte";
import { channelStore } from "$lib/channels/channel.svelte";
import { mkMsg, sys } from "$lib/messages/messages.svelte";
import { clock } from "$lib/rendering/time";
import { roleStore } from "$lib/roles/roles.svelte";
import { profileStore } from "$lib/profile/profile.svelte";
import { activeEmoji, emojiUrlFor } from "$lib/namespaces/server.svelte";
import { moderate } from "$lib/moderation/moderation";
import { searchUnicode } from "$lib/rendering/shortcodes";

type Attachment = { uri: string; name: string; mime: string; thumb: string | null; width: number | null; height: number | null };
type EmojiSuggestion = { name: string; url: string | null; char?: string };

// All mutable composer/edit state in one reactive object so components can bind
// its fields directly (`bind:value={compose.text}`).
export const compose = $state<{
  text: string;
  attachments: Attachment[];
  mentionQuery: string | null;
  mentionIndex: number;
  emojiQuery: string | null;
  emojiIndex: number;
  editingKey: number | null;
  editDraft: string;
  pickerKey: number | null; // message whose reaction picker is open
}>({
  text: "",
  attachments: [],
  mentionQuery: null,
  mentionIndex: 0,
  emojiQuery: null,
  emojiIndex: 0,
  editingKey: null,
  editDraft: "",
  pickerKey: null,
});

const activeChannel = () => (view.active ? channelStore.channels[view.active] : undefined);

// ---- @-mention autocomplete ----
const _mentionMatches = $derived.by<MentionOpt[]>(() => {
  if (compose.mentionQuery === null) return [];
  const q = compose.mentionQuery.toLowerCase();
  const me = store.session.account;
  const opts: MentionOpt[] = [];
  if ("everyone".startsWith(q)) opts.push({ name: "everyone", kind: "special", display: "everyone" });
  if ("here".startsWith(q)) opts.push({ name: "here", kind: "special", display: "here" });
  // Pingable roles at this server (single-word names — the token can't hold spaces).
  for (const r of roleStore.rolesAt(`ns:${view.activeServer}`))
    if (r.pingable && !/\s/.test(r.name) && r.name.toLowerCase().startsWith(q))
      opts.push({ name: r.name, kind: "role", display: r.name, color: r.color });
  // Members: match the account token OR the resolved display name.
  for (const m of activeChannel()?.members ?? []) {
    if (m.name === me) continue;
    const disp = profileStore.displayName(m.name);
    if (!m.name.toLowerCase().startsWith(q) && !disp.toLowerCase().startsWith(q)) continue;
    const identity = m.name.includes("@") ? m.name : `${m.name}@${store.session.network}`;
    opts.push({ name: m.name, kind: "member", display: disp, identity });
  }
  return opts.slice(0, 8);
});

// ---- :emoji: autocomplete (custom emoji + unicode shortcodes) ----
const _emojiSuggestions = $derived.by<EmojiSuggestion[]>(() => {
  if (compose.emojiQuery === null) return [];
  const q = compose.emojiQuery.toLowerCase();
  const rank = (n: string) => (n.toLowerCase().startsWith(q) ? 0 : 1);
  const custom: EmojiSuggestion[] = activeEmoji()
    .filter((e) => e.name.toLowerCase().includes(q))
    .sort((a, b) => rank(a.name) - rank(b.name) || a.name.localeCompare(b.name))
    .map((e) => ({ name: e.name, url: emojiUrlFor(e.name) }));
  const taken = new Set(custom.map((c) => c.name));
  const unicode: EmojiSuggestion[] = searchUnicode(q).filter((u) => !taken.has(u.name)).map((u) => ({ name: u.name, url: null, char: u.char }));
  return [...custom, ...unicode].slice(0, 10);
});

const _typingLabel = $derived.by(() => {
  const who = activeChannel()?.typers ?? [];
  if (!who.length) return "";
  if (who.length === 1) return `${who[0]} is typing…`;
  if (who.length === 2) return `${who[0]} and ${who[1]} are typing…`;
  return "several people are typing…";
});

// Derived views (Svelte 5 can't export `$derived` bindings directly).
export const composeView = {
  get mentionMatches() {
    return _mentionMatches;
  },
  get emojiSuggestions() {
    return _emojiSuggestions;
  },
  get typingLabel() {
    return _typingLabel;
  },
};

// ---- §13 attachments ----
// Upload a batch of files into the pending tray (picker / paste / drag-drop).
// Caps at 10 per message (§13); a failure toasts, not throws.
async function addFiles(files: Iterable<File>): Promise<void> {
  if (!view.active) return;
  for (const file of files) {
    if (compose.attachments.length >= 10) {
      toast("up to 10 attachments per message", "error");
      break;
    }
    try {
      const up = await media.upload(file);
      compose.attachments = [
        ...compose.attachments,
        { uri: up.media, name: file.name || "pasted-file", mime: file.type, thumb: up.thumb, width: up.width, height: up.height },
      ];
    } catch (e) {
      toast(`upload failed: ${e}`, "error");
    }
  }
}
export function attachFile(): void {
  const input = document.createElement("input");
  input.type = "file";
  input.multiple = true;
  input.onchange = () => addFiles(Array.from(input.files ?? []));
  input.click();
}
export function pasteFiles(e: ClipboardEvent): void {
  const files = Array.from(e.clipboardData?.files ?? []);
  if (files.length) {
    e.preventDefault();
    addFiles(files);
  }
}
export function dropFiles(e: DragEvent): void {
  const files = Array.from(e.dataTransfer?.files ?? []);
  if (files.length) {
    e.preventDefault();
    addFiles(files);
  }
}
export function removeAttachment(i: number): void {
  compose.attachments = compose.attachments.filter((_, k) => k !== i);
}

// ---- send / slash commands ----
function runSlash(input: string): void {
  const [raw, ...rest] = input.slice(1).split(/\s+/);
  const cmd = raw.toLowerCase();
  const arg = rest.join(" ").trim();
  const active = view.active;
  switch (cmd) {
    case "ban":
    case "unban":
    case "kick":
    case "mute":
    case "unmute":
      moderate(cmd, arg);
      break;
    case "join":
      if (arg) weft.join(arg.startsWith("#") ? arg : `#${arg}`).catch(() => {});
      break;
    case "part":
    case "leave":
      if (active.startsWith("#")) weft.part(active).catch(() => {});
      break;
    case "create":
      if (arg) weft.channelCreate(arg.startsWith("#") ? arg : `#${arg}`).catch(() => {});
      break;
    case "delete":
      if (active.startsWith("#")) weft.channelDelete(active).catch(() => {});
      break;
    case "topic":
      if (active.startsWith("#")) weft.channelMeta(active, "topic", arg).catch(() => {});
      break;
    case "help":
      sys("/join #chan · /part · /create #chan · /delete · /topic <text> · /ban /unban /kick /mute /unmute <user>");
      break;
    default:
      sys(`unknown command: /${cmd} (try /help)`);
  }
}

export function doSend(): void {
  const text = compose.text.trim();
  if (text.startsWith("/")) {
    runSlash(text);
    compose.text = "";
    return;
  }
  // §6.4: empty body is legal when there are attachments.
  if (!text && !compose.attachments.length) return;
  if (!view.active) return;

  // Stamp intrinsic image size onto the reference so recipients (and history
  // replay) can reserve exact space before the bytes load (§13).
  const attachments = compose.attachments.map((a) => media.withMediaDims(a.uri, a.width, a.height));
  const target = view.active;
  const savedReply = ui.replyTo?.msgid;

  // §9.2/§11.13 optimistic send: show the message immediately as "sending", keyed
  // by a client nonce the authoritative MESSAGE echoes back — so the send feels
  // instant regardless of federation latency.
  const label = crypto.randomUUID();
  channelStore.ensure(target).messages.push(
    mkMsg({
      author: store.session.account,
      body: text,
      time: clock(),
      ts: Date.now(),
      own: true,
      md: true,
      replyTo: savedReply,
      attachments: attachments.length ? attachments : undefined,
      label,
      pending: true,
    }),
  );

  // Clear optimistically; the placeholder carries the text.
  ui.replyTo = null;
  stopTyping();
  compose.text = "";
  compose.attachments = [];

  weft.sendMessage(target, text, savedReply, attachments, undefined, label).catch((e) => {
    // Rejected (e.g. over-long body): drop the placeholder, restore the text so
    // it isn't silently eaten, and surface the error.
    const ch = channelStore.channels[target];
    const i = ch?.messages.findIndex((m) => m.label === label) ?? -1;
    if (ch && i !== -1) ch.messages.splice(i, 1);
    compose.text = text;
    toast(String(e), "error");
  });
}

export function composerKey(e: KeyboardEvent): void {
  // Mention autocomplete captures navigation/accept/dismiss keys while open.
  if (compose.mentionQuery !== null && composeView.mentionMatches.length) {
    const n = composeView.mentionMatches.length;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      compose.mentionIndex = (compose.mentionIndex + 1) % n;
      return;
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      compose.mentionIndex = (compose.mentionIndex - 1 + n) % n;
      return;
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      pickMention(composeView.mentionMatches[Math.min(compose.mentionIndex, n - 1)].name);
      return;
    } else if (e.key === "Escape") {
      e.preventDefault();
      compose.mentionQuery = null;
      return;
    }
  }
  // :emoji: autocomplete captures the same keys while open.
  if (compose.emojiQuery !== null && composeView.emojiSuggestions.length) {
    const n = composeView.emojiSuggestions.length;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      compose.emojiIndex = (compose.emojiIndex + 1) % n;
      return;
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      compose.emojiIndex = (compose.emojiIndex - 1 + n) % n;
      return;
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      pickEmojiSuggestion(composeView.emojiSuggestions[Math.min(compose.emojiIndex, n - 1)].name);
      return;
    } else if (e.key === "Escape") {
      e.preventDefault();
      compose.emojiQuery = null;
      return;
    }
  }
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    doSend();
  } else if (e.key === "ArrowUp" && !compose.text) {
    // Discord-style: edit your last message from an empty composer.
    const mine = activeChannel()?.messages.filter((m) => m.own && m.msgid);
    const last = mine?.[mine.length - 1];
    if (last) {
      e.preventDefault();
      startEdit(last);
    }
  }
}

// ---- typing indicators ----
let typingChannel: string | null = null;
let typingStop: ReturnType<typeof setTimeout> | undefined;
function stopTyping(): void {
  clearTimeout(typingStop);
  if (typingChannel) {
    weft.typing(typingChannel, false).catch(() => {});
    typingChannel = null;
  }
}
export function onComposerInput(): void {
  updateMention();
  updateEmojiSuggest();
  const active = view.active;
  if (!active.startsWith("#")) return;

  if (typingChannel && typingChannel !== active) stopTyping();
  if (!typingChannel) {
    typingChannel = active;
    weft.typing(active, true).catch(() => {});
  }
  clearTimeout(typingStop);
  typingStop = setTimeout(stopTyping, 4000);
}

function updateMention(): void {
  const m = compose.text.match(/@([a-z0-9._-]*)$/i);
  compose.mentionQuery = m ? m[1] : null;
  compose.mentionIndex = 0;
}
export function pickMention(name: string): void {
  compose.text = compose.text.replace(/@[a-z0-9._-]*$/i, `@${name} `);
  compose.mentionQuery = null;
  compose.mentionIndex = 0;
}

function updateEmojiSuggest(): void {
  // A `:word` at a token boundary — not `http://`, not `12:30`.
  const m = compose.text.match(/(?:^|\s):([a-zA-Z0-9_+-]+)$/);
  compose.emojiQuery = m ? m[1] : null;
  compose.emojiIndex = 0;
}
export function pickEmojiSuggestion(name: string): void {
  // Unicode shortcodes insert the character; custom emoji keep the `:name:` form.
  const s = composeView.emojiSuggestions.find((x) => x.name === name);
  const insert = s?.char ?? `:${name}:`;
  compose.text = compose.text.replace(/:[a-zA-Z0-9_+-]*$/, `${insert} `);
  compose.emojiQuery = null;
  compose.emojiIndex = 0;
}

// ---- inline edit / delete / react / jump ----
export function startEdit(m: Msg): void {
  if (!m.own || !m.msgid) return;
  compose.editingKey = m.key;
  compose.editDraft = m.body;
}
export function cancelEdit(): void {
  compose.editingKey = null;
  compose.editDraft = "";
}
export function saveEdit(m: Msg): void {
  const body = compose.editDraft.trim();
  if (body && m.msgid && body !== m.body) {
    m.body = body; // optimistic; the EDITED echo confirms
    m.edited = true;
    weft.edit(m.msgid, body).catch(() => {});
  }
  cancelEdit();
}
export function editKey(e: KeyboardEvent, m: Msg): void {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    saveEdit(m);
  } else if (e.key === "Escape") {
    e.preventDefault();
    cancelEdit();
  }
}
export function doDelete(m: Msg): void {
  // The DELETED echo drops it (Phase 0 handler) — no optimistic removal.
  if (m.own && m.msgid) weft.del(m.msgid).catch(() => {});
}

// Non-optimistic: the server echoes our own REACTION back (like a MSG ack), so
// toggling can't double-count.
export function toggleReaction(m: Msg, emoji: string): void {
  if (!m.msgid) return;
  compose.pickerKey = null;
  const mine = m.reactions?.[emoji]?.mine;
  (mine ? weft.unreact(m.msgid, emoji) : weft.react(m.msgid, emoji)).catch(() => {});
}

export function jumpTo(msgid?: string): void {
  if (!msgid) return;
  const m = activeChannel()?.messages.find((x) => x.msgid === msgid);
  if (m) document.getElementById(`msg-${m.key}`)?.scrollIntoView({ block: "center" });
}

// ---- §6.4 pins / §6.7 report ----
export function togglePin(m: Msg): void {
  if (!m.msgid) return;
  const pinned = activeChannel()?.pinnedIds?.includes(m.msgid) ?? false;
  weft.pin(m.msgid, !pinned).catch((e) => toast(String(e), "error"));
}
export function openReport(m: Msg): void {
  if (m.msgid) store.reports.target = m;
}
