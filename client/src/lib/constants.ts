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
