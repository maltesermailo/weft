# Testing built-in ACME + the admin panel on a VPS

One `weftd` process, no front proxy: built-in ACME (Let's Encrypt) issues the
cert, and weftd serves the admin panel over **native HTTPS** with that same cert
(`[listen] https`). `:80` stays plain HTTP for the ACME HTTP-01 challenge.

## What you need

- A **domain** you control (e.g. `weft.example.com`) with a DNS **A record →
  your VPS IP**.
- A VPS with these ports open to the internet:
  | Port | Proto | Why |
  |---|---|---|
  | 80  | TCP | ACME HTTP-01 challenge (Let's Encrypt validates over it) |
  | 443 | TCP | admin panel over HTTPS |
  | 443 | UDP | QUIC (the WEFT transport) — coexists with 443/TCP |
  | 22  | TCP | SSH |

## 1. Config

Save as `config.toml` on the VPS (edit the domain + email):

```toml
network     = "weft.example.com"     # your domain
operators   = ["admin"]              # who can log into the panel
registration = "open"                # so you can register 'admin'; close it later

[listen]
quic  = "0.0.0.0:443"                # QUIC (UDP)
http  = "0.0.0.0:80"                 # ACME HTTP-01 challenge (must be public :80)
https = "0.0.0.0:443"                # admin panel over TLS (TCP :443)

[identity]
key_file = "weftd.key"               # persist the network signing key across restarts

[storage]
backend = "memory"                   # quick test; use "postgres" to keep accounts

[acme]
enabled   = true
domains   = ["weft.example.com"]
email     = "you@example.com"
staging   = true                     # untrusted TEST certs, high rate limits
cache_dir = "acme"                   # caches the ACME account + issued cert

[admin]
enabled = true
```

Keep `staging = true` while iterating — Let's Encrypt's real endpoint rate-limits
to ~5 certs/domain/week. Flip to `false` for a browser-trusted cert once it works.

## 2. Build + run

On the VPS (or build locally and `scp` the binary):

```bash
cargo build --release -p weftd
./target/release/weftd config.toml
```

Watch the logs. You should see the ACME task obtain a cert (it needs `:80`
reachable from the internet), then `HTTPS (admin/well-known) listening`. The
issued cert lands in `acme/`.

## 3. Register the operator account

The panel logs in with an **account password**, so `admin` must exist in the
store first. Register it over QUIC with the dev client (from your laptop — it
uses the insecure QUIC client, so the staging cert is fine):

```bash
cargo run -p weft-tui -- weft.example.com:443 admin '#general' 'a-long-password-12+'
```

Unknown accounts auto-register, so this creates `admin` with that password. (With
`storage = "memory"`, re-register after any weftd restart; `postgres` persists it.)

## 4. Open the panel

Visit **https://weft.example.com/admin** and log in as `admin` / your password.

- With `staging = true` the cert is from Let's Encrypt **staging** (untrusted
  root), so the browser shows a warning — **proceed anyway**. It's still HTTPS,
  so the Secure session cookie works. Set `staging = false` for a clean padlock.
- You now have: Dashboard (with live connections), Reports (with materialized
  context + resolve), Users, Messages (browse + delete), Moderation
  (mute/ban/kick).

## Verifying ACME specifically

- The `acme/` dir contains the issued cert/key + the cached account key.
- `curl -sky https://weft.example.com/.well-known/weft` returns the network
  descriptor over the ACME-issued cert.
- Renewal is automatic (weftd re-runs ACME ~30 days before expiry); the cert
  hot-swaps into both QUIC and HTTPS with no restart.

## Notes

- **Close registration** (`registration = "closed"`) once `admin` exists, so the
  open network isn't left registerable.
- `:80` is public only for the ACME challenge; the admin panel there is
  Secure-cookie-gated (login won't stick over plain HTTP) — always use the
  `https` URL.
- For a persistent deployment, use `storage.backend = "postgres"` + a
  `storage.url`, and run weftd under systemd.
