//! Server-side URL **unfurl proxy**: fetch a user-supplied link, extract its
//! OpenGraph/meta preview, and return it as JSON — so clients render link
//! previews without leaking the viewer's IP to arbitrary hosts and without
//! running into CORS. A companion `/unfurl/image` proxies the preview image the
//! same way.
//!
//! **Security (invariant 13 — SSRF).** Fetching arbitrary user-supplied URLs is
//! the textbook SSRF surface, so every fetch is guarded exactly like
//! [`crate::dialer::fetch_signing_key`]: the URL host is resolved and **every**
//! resolved address checked with [`crate::dialer::is_dialable`] *before* we
//! connect, and the guard re-runs on each redirect hop. Userinfo (`user@host`)
//! is stripped before host extraction so `https://trusted@169.254.169.254/`
//! can't smuggle an internal target. We connect to the verified IP (not by
//! re-resolving the name), closing the DNS-rebinding window. Both endpoints
//! require a valid media session bearer, so this is never an open proxy.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ServerCtx;

const MAX_HTML: usize = 512 * 1024; // enough for <head>; images capped separately
const MAX_IMAGE: usize = 4 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) fn router(ctx: Arc<ServerCtx>) -> Router {
    Router::new()
        .route("/unfurl", get(unfurl))
        .route("/unfurl/image", get(image))
        .layer(axum::middleware::from_fn(crate::cors::cors))
        .with_state(ctx)
}

// ---- request/response shapes ----

#[derive(Deserialize)]
struct UnfurlQuery {
    #[serde(default)]
    url: String,
    #[serde(default)]
    t: String,
}

#[derive(Serialize, Default)]
struct Preview {
    /// The final URL after redirects (canonical for the card link).
    url: String,
    title: Option<String>,
    description: Option<String>,
    /// Absolute image URL; the client proxies it via `/unfurl/image`.
    image: Option<String>,
    site_name: Option<String>,
}

// ---- handlers ----

/// `GET /unfurl?url=<href>&t=<bearer>` → link-preview JSON.
async fn unfurl(State(ctx): State<Arc<ServerCtx>>, Query(q): Query<UnfurlQuery>) -> Response {
    if ctx.media_bearer_account(&q.t).is_none() {
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    }
    let fetched = match fetch(&q.url, Accept::Html, MAX_HTML).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::BAD_GATEWAY, "could not fetch url").into_response(),
    };
    // Only parse HTML; a non-HTML target (a PDF, a raw image) has no preview.
    if !fetched.content_type.starts_with("text/html") {
        return Json(Preview {
            url: fetched.final_url.clone(),
            ..Default::default()
        })
        .into_response();
    }
    let body = String::from_utf8_lossy(&fetched.body);
    let mut preview = parse_meta(&body, &fetched.target);
    preview.url = fetched.final_url;
    Json(preview).into_response()
}

/// `GET /unfurl/image?url=<href>&t=<bearer>` → the proxied preview image bytes.
async fn image(State(ctx): State<Arc<ServerCtx>>, Query(q): Query<UnfurlQuery>) -> Response {
    if ctx.media_bearer_account(&q.t).is_none() {
        return (StatusCode::FORBIDDEN, "invalid token").into_response();
    }
    let fetched = match fetch(&q.url, Accept::Image, MAX_IMAGE).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::BAD_GATEWAY, "could not fetch image").into_response(),
    };
    if !fetched.content_type.starts_with("image/") {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "not an image").into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, fetched.content_type),
            // A preview image is public + immutable; let the client cache it.
            (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
        fetched.body,
    )
        .into_response()
}

// ---- URL target parsing (SSRF-aware) ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    https: bool,
    host: String,
    port: u16,
    path: String,
}

impl Target {
    fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        // Keep an explicit non-default port for relative-URL resolution.
        let default = if self.https { 443 } else { 80 };
        if self.port == default {
            format!("{scheme}://{}", self.host)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
    fn absolute(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }
}

/// Parse an `http(s)` URL into a fetch target. Rejects any other scheme; strips
/// userinfo before extracting the host (SSRF); handles IPv6 literals + ports.
fn parse_target(raw: &str) -> Option<Target> {
    let raw = raw.trim();
    let (scheme, rest) = raw.split_once("://")?;
    let https = match scheme.to_ascii_lowercase().as_str() {
        "https" => true,
        "http" => false,
        _ => return None,
    };

    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let path = &rest[auth_end..];

    // Drop userinfo: the real host is after the last '@'.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);

    let (host, port) = if let Some(after_bracket) = hostport.strip_prefix('[') {
        // IPv6 literal: [::1] or [::1]:8080
        let (h, tail) = after_bracket.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse().ok()?),
            None if tail.is_empty() => None,
            None => return None,
        };
        (h.to_string(), port)
    } else if let Some((h, p)) = hostport.rsplit_once(':') {
        (h.to_string(), Some(p.parse().ok()?))
    } else {
        (hostport.to_string(), None)
    };

    if host.is_empty() {
        return None;
    }
    let port = port.unwrap_or(if https { 443 } else { 80 });
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path.to_string()
    };
    Some(Target {
        https,
        host,
        port,
        path,
    })
}

/// Resolve a `Location` redirect (absolute, protocol-relative, root-relative,
/// or relative) against the current target.
fn resolve_redirect(base: &Target, location: &str) -> Option<Target> {
    let loc = location.trim();
    if loc.contains("://") {
        parse_target(loc)
    } else if let Some(rest) = loc.strip_prefix("//") {
        parse_target(&format!(
            "{}://{rest}",
            if base.https { "https" } else { "http" }
        ))
    } else if loc.starts_with('/') {
        parse_target(&format!("{}{loc}", base.origin()))
    } else {
        // Relative to the current directory.
        let dir = base.path.rsplit_once('/').map_or("/", |(d, _)| d);
        parse_target(&format!("{}{dir}/{loc}", base.origin()))
    }
}

// ---- SSRF-guarded fetch ----

enum Accept {
    Html,
    Image,
}
impl Accept {
    fn header(&self) -> &'static str {
        match self {
            Accept::Html => "text/html,application/xhtml+xml",
            Accept::Image => "image/*",
        }
    }
}

struct Fetched {
    target: Target,
    final_url: String,
    content_type: String,
    body: Vec<u8>,
}

#[derive(Debug)]
enum FetchError {
    BadUrl,
    NotPublic,
    TooManyRedirects,
    Upstream,
}

/// Fetch `initial`, following redirects (each re-guarded), up to `max_bytes`.
async fn fetch(initial: &str, accept: Accept, max_bytes: usize) -> Result<Fetched, FetchError> {
    let mut target = parse_target(initial).ok_or(FetchError::BadUrl)?;

    for _ in 0..=MAX_REDIRECTS {
        let addr = resolve_and_guard(&target).await?;
        let resp = http_get(&target, addr, &accept, max_bytes).await?;
        match resp.status {
            200 => {
                return Ok(Fetched {
                    final_url: target.absolute(),
                    target,
                    content_type: resp.content_type,
                    body: resp.body,
                });
            }
            301 | 302 | 303 | 307 | 308 => {
                let loc = resp.location.ok_or(FetchError::Upstream)?;
                target = resolve_redirect(&target, &loc).ok_or(FetchError::BadUrl)?;
            }
            _ => return Err(FetchError::Upstream),
        }
    }
    Err(FetchError::TooManyRedirects)
}

/// Resolve the target host and reject unless **every** resolved address is a
/// public unicast address (invariant 13). Returns the verified address to
/// connect to (so we never re-resolve the name — no DNS-rebinding window).
async fn resolve_and_guard(target: &Target) -> Result<SocketAddr, FetchError> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|_| FetchError::NotPublic)?
        .collect();
    if addrs.is_empty() {
        return Err(FetchError::NotPublic);
    }
    if !addrs.iter().all(crate::dialer::is_dialable) {
        return Err(FetchError::NotPublic);
    }
    Ok(addrs[0])
}

struct RawResponse {
    status: u16,
    content_type: String,
    location: Option<String>,
    body: Vec<u8>,
}

fn tls_config() -> Arc<tokio_rustls::rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = tokio_rustls::rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                tokio_rustls::rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            )
        })
        .clone()
}

async fn http_get(
    target: &Target,
    addr: SocketAddr,
    accept: &Accept,
    max_bytes: usize,
) -> Result<RawResponse, FetchError> {
    let host_header = if target.port == if target.https { 443 } else { 80 } {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nAccept: {}\r\nUser-Agent: weftd-unfurl\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        target.path,
        accept.header(),
    );

    let raw = tokio::time::timeout(FETCH_TIMEOUT, async {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|_| FetchError::Upstream)?;
        if target.https {
            let server_name =
                tokio_rustls::rustls::pki_types::ServerName::try_from(target.host.clone())
                    .map_err(|_| FetchError::BadUrl)?;
            let tls = tokio_rustls::TlsConnector::from(tls_config())
                .connect(server_name, tcp)
                .await
                .map_err(|_| FetchError::Upstream)?;
            read_http(tls, &req, max_bytes).await
        } else {
            read_http(tcp, &req, max_bytes).await
        }
    })
    .await
    .map_err(|_| FetchError::Upstream)??;

    Ok(raw)
}

/// Write the request, read the response (capped), and parse status + headers +
/// (dechunked) body.
async fn read_http<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    req: &str,
    max_bytes: usize,
) -> Result<RawResponse, FetchError> {
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|_| FetchError::Upstream)?;

    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| FetchError::Upstream)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Cap generously: headers + capped body. Stop reading once we're well
        // past the body budget so a hostile server can't stream forever.
        if buf.len() > max_bytes + 16 * 1024 {
            break;
        }
    }

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(FetchError::Upstream)?;
    let head = &buf[..split];
    let body_raw = &buf[split + 4..];

    let head_str = String::from_utf8_lossy(head);
    let mut lines = head_str.split("\r\n");
    let status_line = lines.next().ok_or(FetchError::Upstream)?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or(FetchError::Upstream)?;

    let mut content_type = String::new();
    let mut location = None;
    let mut chunked = false;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "content-type" => content_type = value.to_ascii_lowercase(),
            "location" => location = Some(value.to_string()),
            "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
            _ => {}
        }
    }

    let mut body = if chunked {
        dechunk(body_raw)
    } else {
        body_raw.to_vec()
    };
    body.truncate(max_bytes);

    Ok(RawResponse {
        status,
        content_type,
        location,
        body,
    })
}

/// Decode an HTTP `chunked` body. Best-effort: on a malformed/truncated chunk it
/// returns what was decoded so far (untrusted input must never panic).
fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(nl) = data.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_line = &data[..nl];
        let size_str = std::str::from_utf8(size_line).unwrap_or("");
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16);
        let Ok(size) = size else { break };
        data = &data[nl + 2..];
        if size == 0 {
            break;
        }
        if data.len() < size {
            out.extend_from_slice(data);
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size..];
        if data.starts_with(b"\r\n") {
            data = &data[2..];
        }
    }
    out
}

// ---- pure meta extraction ----

/// Extract an OpenGraph/meta preview from HTML. Pure + panic-free (untrusted
/// remote input). Precedence: `og:*` → `twitter:*` → `<title>` / `meta[name]`.
fn parse_meta(html: &str, base: &Target) -> Preview {
    // Only scan the head-ish prefix — meta lives there, and it bounds the work.
    let scan = &html[..html.len().min(MAX_HTML)];

    let mut og_title = None;
    let mut og_desc = None;
    let mut og_image = None;
    let mut og_site = None;
    let mut tw_title = None;
    let mut tw_desc = None;
    let mut tw_image = None;
    let mut meta_desc = None;

    // Walk every <meta ...> tag.
    let bytes = scan.as_bytes();
    let mut i = 0;
    while let Some(rel) = find_ci(&scan[i..], "<meta") {
        let start = i + rel;
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'>')
            .map_or(scan.len(), |p| start + p);
        let tag = &scan[start..end];

        let key = tag_attr(tag, "property").or_else(|| tag_attr(tag, "name"));
        if let Some(key) = key {
            if let Some(content) = tag_attr(tag, "content") {
                match key.to_ascii_lowercase().as_str() {
                    "og:title" => og_title = Some(content),
                    "og:description" => og_desc = Some(content),
                    "og:image" | "og:image:url" | "og:image:secure_url" => og_image = Some(content),
                    "og:site_name" => og_site = Some(content),
                    "twitter:title" => tw_title = Some(content),
                    "twitter:description" => tw_desc = Some(content),
                    "twitter:image" | "twitter:image:src" => tw_image = Some(content),
                    "description" => meta_desc = Some(content),
                    _ => {}
                }
            }
        }
        i = (end + 1).min(scan.len());
    }

    let title = og_title.or(tw_title).or_else(|| extract_title(scan));
    let description = og_desc.or(tw_desc).or(meta_desc);
    let image = og_image.or(tw_image).and_then(|img| absolutize(base, &img));

    Preview {
        url: String::new(),
        title: title.map(|s| clip(&s, 300)),
        description: description.map(|s| clip(&s, 600)),
        image,
        site_name: og_site.map(|s| clip(&s, 120)),
    }
}

/// The `<title>…</title>` text, entity-decoded.
fn extract_title(html: &str) -> Option<String> {
    let open = find_ci(html, "<title")?;
    let gt = html[open..].find('>')? + open + 1;
    let close = find_ci(&html[gt..], "</title>")? + gt;
    let text = decode_entities(html[gt..close].trim());
    (!text.is_empty()).then_some(text)
}

/// Extract an HTML tag attribute's value (quoted or bare), entity-decoded.
/// Case-insensitive on the key; tolerant of surrounding whitespace.
fn tag_attr(tag: &str, key: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(key) {
        let at = from + rel;
        // The match must be a real attribute boundary (preceded by space/quote/
        // tag-start) and followed by '='.
        let before_ok = at == 0
            || matches!(
                lower.as_bytes()[at - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'/'
            );
        let after = lower[at + key.len()..].trim_start();
        if before_ok && after.starts_with('=') {
            let rest = tag[at + key.len()..].trim_start();
            let rest = rest.strip_prefix('=')?.trim_start();
            let value = if let Some(r) = rest.strip_prefix('"') {
                r.split('"').next().unwrap_or("")
            } else if let Some(r) = rest.strip_prefix('\'') {
                r.split('\'').next().unwrap_or("")
            } else {
                rest.split([' ', '\t', '\n', '\r', '>', '/'])
                    .next()
                    .unwrap_or("")
            };
            return Some(decode_entities(value));
        }
        from = at + key.len();
    }
    None
}

/// Resolve a possibly-relative image reference to an absolute URL.
fn absolutize(base: &Target, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if href.contains("://") {
        // Only pass through http(s) images.
        parse_target(href).map(|t| t.absolute())
    } else if let Some(rest) = href.strip_prefix("//") {
        Some(format!(
            "{}://{rest}",
            if base.https { "https" } else { "http" }
        ))
    } else if href.starts_with('/') {
        Some(format!("{}{href}", base.origin()))
    } else {
        let dir = base.path.rsplit_once('/').map_or("", |(d, _)| d);
        Some(format!("{}{dir}/{href}", base.origin()))
    }
}

/// Case-insensitive substring search (ASCII), byte-offset into `haystack`.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Decode the handful of HTML entities that show up in meta content.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let end = after.find(';').filter(|&e| e <= 10);
        if let Some(end) = end {
            let entity = &after[1..end];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" | "#x27" | "#X27" => Some('\''),
                "nbsp" => Some(' '),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    u32::from_str_radix(&entity[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if entity.starts_with('#') => {
                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                }
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

/// Trim to a byte budget on a char boundary, collapsing inner whitespace.
fn clip(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max {
        return collapsed;
    }
    let mut end = max;
    while end > 0 && !collapsed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &collapsed[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str) -> Target {
        parse_target(url).unwrap()
    }

    #[test]
    fn parses_scheme_host_port_path() {
        let x = t("https://example.com/foo/bar?q=1");
        assert!(x.https);
        assert_eq!(x.host, "example.com");
        assert_eq!(x.port, 443);
        assert_eq!(x.path, "/foo/bar?q=1");

        let y = t("http://example.com:8080");
        assert!(!y.https);
        assert_eq!(y.port, 8080);
        assert_eq!(y.path, "/");
    }

    #[test]
    fn strips_userinfo_so_ssrf_cannot_smuggle_host() {
        // The real host is after '@'; a trusted-looking userinfo must not win.
        let x = t("https://trusted.com@169.254.169.254/latest/meta-data");
        assert_eq!(x.host, "169.254.169.254");
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(parse_target("file:///etc/passwd").is_none());
        assert!(parse_target("gopher://x").is_none());
        assert!(parse_target("ftp://x/y").is_none());
        assert!(parse_target("javascript:alert(1)").is_none());
    }

    #[test]
    fn ipv6_literal_with_port() {
        let x = t("http://[2606:2800:220:1:248:1893:25c8:1946]:8080/x");
        assert_eq!(x.host, "2606:2800:220:1:248:1893:25c8:1946");
        assert_eq!(x.port, 8080);
    }

    #[test]
    fn redirect_resolution() {
        let base = t("https://example.com/a/b");
        assert_eq!(
            resolve_redirect(&base, "https://other.com/x")
                .unwrap()
                .absolute(),
            "https://other.com/x"
        );
        assert_eq!(
            resolve_redirect(&base, "/root").unwrap().absolute(),
            "https://example.com/root"
        );
        assert_eq!(
            resolve_redirect(&base, "//cdn.com/y").unwrap().host,
            "cdn.com"
        );
    }

    #[test]
    fn extracts_opengraph() {
        let html = r#"<html><head>
            <title>Fallback Title</title>
            <meta property="og:title" content="OG &amp; Title">
            <meta property="og:description" content="A description here.">
            <meta property="og:image" content="/img/card.png">
            <meta property="og:site_name" content="Example">
            <meta name="description" content="meta desc">
        </head></html>"#;
        let base = t("https://example.com/page");
        let p = parse_meta(html, &base);
        assert_eq!(p.title.as_deref(), Some("OG & Title"));
        assert_eq!(p.description.as_deref(), Some("A description here."));
        assert_eq!(p.image.as_deref(), Some("https://example.com/img/card.png"));
        assert_eq!(p.site_name.as_deref(), Some("Example"));
    }

    #[test]
    fn falls_back_to_title_and_meta_description() {
        let html = r#"<head><title>Just A Title</title>
            <meta name="description" content="fallback desc"></head>"#;
        let base = t("https://example.com/");
        let p = parse_meta(html, &base);
        assert_eq!(p.title.as_deref(), Some("Just A Title"));
        assert_eq!(p.description.as_deref(), Some("fallback desc"));
        assert!(p.image.is_none());
    }

    #[test]
    fn twitter_card_fallback() {
        let html = r#"<meta name="twitter:title" content="TW"><meta name="twitter:image" content="https://cdn.example.com/i.jpg">"#;
        let base = t("https://example.com/");
        let p = parse_meta(html, &base);
        assert_eq!(p.title.as_deref(), Some("TW"));
        assert_eq!(p.image.as_deref(), Some("https://cdn.example.com/i.jpg"));
    }

    #[test]
    fn dechunk_reassembles_body() {
        let chunked = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(chunked), b"Wikipedia");
    }

    #[test]
    fn dechunk_truncated_does_not_panic() {
        // A size claiming more bytes than present must return what's decoded.
        let bad = b"ff\r\nonlyfour";
        let _ = dechunk(bad);
    }

    #[test]
    fn meta_parser_never_panics_on_junk() {
        let base = t("https://x.com/");
        for junk in [
            "<meta",
            "<meta property",
            "<meta property=",
            "<meta property=og:title content",
            "<title>unclosed",
            "&#xZZZZ;<meta property=\"og:title\" content=\"&#;\">",
            "\u{0}<meta>",
        ] {
            let _ = parse_meta(junk, &base);
        }
    }

    #[test]
    fn bare_and_single_quoted_attrs() {
        let html = r#"<meta property=og:title content='Single Quoted'>"#;
        let p = parse_meta(html, &t("https://x.com/"));
        assert_eq!(p.title.as_deref(), Some("Single Quoted"));
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_internal_targets() {
        // Invariant 13: the unfurl fetch path must refuse every non-public
        // target before connecting — loopback, ULA/v6-loopback, RFC-1918, and
        // the cloud metadata address.
        for url in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://[::1]/x",
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://169.254.169.254/latest/meta-data",
        ] {
            let target = parse_target(url).unwrap();
            assert!(
                matches!(resolve_and_guard(&target).await, Err(FetchError::NotPublic)),
                "should reject internal target {url}"
            );
        }
    }
}
