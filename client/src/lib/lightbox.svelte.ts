// Fullscreen image viewer (Tier 1). One shared target; the overlay lives at the
// app root and any Attachment can open it by URL.
export const lightbox = $state<{ url: string | null; alt: string }>({ url: null, alt: "" });

export function openLightbox(url: string, alt = "image"): void {
  lightbox.url = url;
  lightbox.alt = alt;
}

export function closeLightbox(): void {
  lightbox.url = null;
}
