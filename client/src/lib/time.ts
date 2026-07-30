// Pure time / timestamp helpers: wall-clock formatting, ULID-timestamp decoding
// (so backfilled history shows correct times, not arrival time), day-separator
// labels, and the retention-policy normaliser. No app state — safe to import
// anywhere (layout, reducer, components).

/// "HH:MM" for a Date.
export const hhmm = (d: Date): string =>
  `${`${d.getHours()}`.padStart(2, "0")}:${`${d.getMinutes()}`.padStart(2, "0")}`;

/// "HH:MM" for now.
export const clock = (): string => hhmm(new Date());

// A msgid is `network/<ULID>`; the ULID's first 10 Crockford-base32 chars encode
// its 48-bit ms timestamp.
const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Decode a msgid's ULID timestamp to epoch ms, or null if it isn't a ULID.
export function msgEpoch(msgid: string | undefined): number | null {
  const ulid = msgid?.split("/").pop() ?? "";
  if (ulid.length < 10) return null;
  let ms = 0;
  for (let i = 0; i < 10; i++) {
    const v = CROCKFORD.indexOf(ulid[i].toUpperCase());
    if (v < 0) return null;
    ms = ms * 32 + v;
  }
  return ms;
}

/// The "HH:MM" a msgid was minted (from its ULID), falling back to now.
export function msgTime(msgid: string): string {
  const ms = msgEpoch(msgid);
  return ms === null ? clock() : hhmm(new Date(ms));
}

// ---- day separators (Tier 1) ----
const startOfDay = (d: Date): number => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
export const dayKey = (ts: number): number => startOfDay(new Date(ts));
export function dayLabel(ts: number): string {
  const diff = Math.round((startOfDay(new Date()) - dayKey(ts)) / 86_400_000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return new Date(ts).toLocaleDateString(undefined, {
    weekday: "long",
    month: "long",
    day: "numeric",
    year: "numeric",
  });
}

/// Normalise a channel retention policy to one of the known kinds.
export const retentionOf = (policy: string): string => {
  if (policy.startsWith("retained")) return "retained";
  if (["ephemeral", "permanent", "e2ee"].includes(policy)) return policy;
  return "retained";
};
