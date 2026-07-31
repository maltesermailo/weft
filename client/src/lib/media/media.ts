// §13 media domain: content-addressed upload, link unfurl, and media-ref
// helpers. Split out of the transport client (weft.ts) — it owns the shared
// media bearer/base state that the transport connection sets on connect.

/// Tauri v2 injects `__TAURI_INTERNALS__`; its absence ⇒ a plain browser.
/// Defined locally (not imported from transport) so media has no dependency
/// back on the transport client that imports it.
const IS_TAURI = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

// ---- §13 media ----

/** The per-session fetch bearer (from the `media-token` event); set on connect. */
let mediaBearer = "";
export function setMediaBearer(token: string) {
  mediaBearer = token;
}

export type UploadResult = {
  media: string; // weft-media://origin/hash
  thumb: string | null;
  width: number | null;
  height: number | null;
};

/**
 * Base HTTP origin for the §13 media endpoints.
 *
 * On the **web** the page is already served by the network, so same-origin is
 * right. On the **desktop** the page is served from the Tauri bundle, so
 * same-origin points at the app, not the server — the base has to be set
 * explicitly (see {@link setMediaBase}) or media silently 404s.
 */
let mediaBase = "";
export function mediaOrigin(): string {
  if (IS_TAURI) return mediaBase;
  if (typeof window !== "undefined" && window.location) return window.location.origin;
  return "";
}

/**
 * Point the desktop client at the HTTP origin serving `/media`.
 *
 * `configured` wins when set (dev / reverse proxy on a nonstandard port);
 * otherwise it is derived as `https://<host>` with the QUIC port dropped —
 * weftd's HTTP listener is a different port, and in a real deployment the
 * network's DNS name fronts it on 443.
 */
export function setMediaBase(host: string, configured?: string | null) {
  if (configured) {
    mediaBase = configured.replace(/\/+$/, "");
    return;
  }
  const hostname = host.trim().replace(/^\w+:\/\//, "").split("/")[0].replace(/:\d+$/, "");
  mediaBase = hostname ? `https://${hostname}` : "";
}

/**
 * Upload a file to the network and return its content-addressed reference: a
 * single authed POST to `/media`, the session bearer authorizing it. Identical
 * on web and desktop — they differ only in where {@link mediaOrigin} points.
 */
export async function upload(file: File | Blob): Promise<UploadResult> {
  if (!mediaBearer) throw new Error("no media session");
  if (IS_TAURI && !mediaOrigin()) {
    throw new Error(
      "no media server configured — set `media_base` in client.toml if it isn't at https://<host>",
    );
  }
  let res: Response;
  try {
    res = await fetch(`${mediaOrigin()}/media?t=${encodeURIComponent(mediaBearer)}`, {
      method: "POST",
      headers: { "Content-Type": (file as File).type || "application/octet-stream" },
      body: file,
    });
  } catch (e) {
    // A bare fetch rejection ("TypeError: Load failed") means the media origin
    // was unreachable or blocked the cross-origin request — surface something
    // the user can act on instead of the raw network error.
    throw new Error(
      `could not reach the media server at ${mediaOrigin() || "(unset)"} — check it is running and reachable (${e})`,
    );
  }
  if (!res.ok) throw new Error(`upload failed (${res.status})`);
  const j = await res.json();
  return { media: j.media, thumb: j.thumb ?? null, width: j.width ?? null, height: j.height ?? null };
}

/** A server-fetched link preview (§13 unfurl proxy). */
export type LinkPreview = {
  url: string;
  title?: string;
  description?: string;
  image?: string;
  siteName?: string;
};

// Per-session cache: URL → preview (null = no useful preview / fetch failed).
const unfurlCache = new Map<string, LinkPreview | null>();

/** Fetch a link preview via the server-side unfurl proxy (SSRF-guarded). */
export async function unfurl(url: string): Promise<LinkPreview | null> {
  if (!mediaBearer) return null;
  if (unfurlCache.has(url)) return unfurlCache.get(url) ?? null;
  const origin = mediaOrigin();
  if (IS_TAURI && !origin) return null;
  try {
    const res = await fetch(
      `${origin}/unfurl?url=${encodeURIComponent(url)}&t=${encodeURIComponent(mediaBearer)}`,
    );
    if (!res.ok) {
      unfurlCache.set(url, null);
      return null;
    }
    const j = await res.json();
    const preview: LinkPreview = {
      url: j.url ?? url,
      title: j.title ?? undefined,
      description: j.description ?? undefined,
      image: j.image ?? undefined,
      siteName: j.site_name ?? undefined,
    };
    // Only surface a card when there's something to show.
    const useful = preview.title || preview.description || preview.image ? preview : null;
    unfurlCache.set(url, useful);
    return useful;
  } catch {
    unfurlCache.set(url, null);
    return null;
  }
}

/** Proxy a preview image through the server so the client never hits the
 * origin host directly (no IP leak). */
export function unfurlImageUrl(imageUrl: string): string {
  return `${mediaOrigin()}/unfurl/image?url=${encodeURIComponent(imageUrl)}&t=${encodeURIComponent(mediaBearer)}`;
}

/** Resolve a `weft-media://origin/hash` reference to a fetchable URL. */
export function mediaUrl(ref: string): string {
  return avatarUrl(mediaHash(ref));
}

/** The BLAKE3 hash portion of a `weft-media://origin/hash` reference. A trailing
 *  `#WxH` (or `?…`) dimensions suffix is metadata, not part of the hash. */
export function mediaHash(ref: string): string {
  const rest = ref.replace(/^weft-media:\/\//, "").split(/[#?]/)[0];
  return rest.slice(rest.indexOf("/") + 1);
}

/** Intrinsic pixel size a sender stamped onto an image reference
 *  (`weft-media://origin/hash#WxH`), so the renderer can reserve exact space
 *  before the bytes load. `null` when absent (older messages / non-images). */
export function mediaDims(ref: string): { w: number; h: number } | null {
  const m = ref.match(/#(\d+)x(\d+)$/);
  if (!m) return null;
  const w = Number(m[1]);
  const h = Number(m[2]);
  return w > 0 && h > 0 ? { w, h } : null;
}

/** Append an intrinsic-size suffix to an image reference, for the exact-space
 *  reservation on render (§13). Only when both dimensions are known. */
export function withMediaDims(ref: string, width: number | null, height: number | null): string {
  return width && height ? `${ref}#${width}x${height}` : ref;
}

/** §10.3 a fetchable URL for an avatar (or any) blob hash, home-network only. */
export function avatarUrl(hash: string): string {
  return `${mediaOrigin()}/media/${hash}?t=${encodeURIComponent(mediaBearer)}`;
}
