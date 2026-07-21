// Static option lists shared across components.

export const REPORT_CATEGORIES = [
  "spam",
  "harassment",
  "violence",
  "sexual",
  "csam",
  "illegal",
  "self-harm",
  "other",
];

/// The full standard capability set (§10.4), for role/grant pickers.
export const CAPS = [
  "send", "react", "attach", "edit-own", "delete-own", "delete-any", "pin", "invite",
  "mute", "ban", "kick", "policy", "view", "chan-create", "reports", "ns-admin",
];

/// Channel-relevant capabilities for per-channel permission editing.
export const CHAN_CAPS = ["send", "react", "attach", "pin", "edit-own", "delete-own", "delete-any"];

/// Human labels + one-line descriptions for each capability, shown in the
/// role permission checklist (Discord-style).
export const CAP_META: Record<string, { label: string; desc: string }> = {
  "send": { label: "Send messages", desc: "Post messages in channels." },
  "react": { label: "Add reactions", desc: "React to messages with emoji." },
  "attach": { label: "Attach files", desc: "Upload files and media." },
  "edit-own": { label: "Edit own messages", desc: "Edit messages they sent." },
  "delete-own": { label: "Delete own messages", desc: "Delete messages they sent." },
  "delete-any": { label: "Delete any message", desc: "Remove messages from any member." },
  "pin": { label: "Pin messages", desc: "Pin and unpin messages." },
  "invite": { label: "Create invites", desc: "Mint invite links to this scope." },
  "mute": { label: "Mute members", desc: "Stop members from posting." },
  "ban": { label: "Ban members", desc: "Block members from joining and posting." },
  "kick": { label: "Kick members", desc: "Remove members from a channel." },
  "policy": { label: "Manage policy", desc: "Change channel retention and settings." },
  "view": { label: "View channels", desc: "See view-gated channels." },
  "chan-create": { label: "Manage channels", desc: "Create and delete channels." },
  "reports": { label: "Handle reports", desc: "Review and resolve reported content." },
  "ns-admin": { label: "Administer namespace", desc: "Full control over this namespace." },
};

/// Preset role colors.
export const ROLE_COLORS = ["#e0679a", "#e8b93d", "#5865f2", "#3ba55d", "#4fb0a5", "#9d6fc4", "#e8654f", "#87898c"];

/// Report-resolution actions.
export const RESOLVE_ACTIONS = ["dismissed", "content-removed", "user-actioned", "escalated"];

/// Channel retention policies (§6.3). `value` is the wire string sent on
/// CHANNEL CREATE / CHANNEL POLICY; `key` matches the `retentionMeta` dot class.
export const RETENTION_OPTIONS = [
  { value: "ephemeral", key: "ephemeral", label: "Ephemeral — vanish on read/leave" },
  { value: "retained:30d", key: "retained", label: "Retained · 30 days" },
  { value: "retained:90d", key: "retained", label: "Retained · 90 days" },
  { value: "permanent", key: "permanent", label: "Permanent" },
  { value: "e2ee", key: "e2ee", label: "E2EE · MLS (empty channel only)" },
];
