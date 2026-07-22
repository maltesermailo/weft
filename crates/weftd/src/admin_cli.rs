//! `weftd admin …` — the operator-management CLI (§10.4). Runs without a live
//! server: it talks to the Postgres store directly, so operator status lives in
//! the database rather than the config `[operators]` list. In Docker:
//! `docker compose exec weftd weftd admin <command>`.

use anyhow::{bail, Context, Result};
use weft_crypto::PasswordHash;
use weft_proto::Account;
use weft_store::{AccountStore, PgStore};

const HELP: &str = "\
weftd admin — manage operator accounts (§10.4)

USAGE:
    weftd admin [--config <path>] <command>

COMMANDS:
    create <account>    Register a new account and make it an operator
                        (password from --password or $WEFT_ADMIN_PASSWORD)
    grant  <account>    Grant operator status to an existing account
    revoke <account>    Revoke operator status
    list                List operator accounts

OPTIONS:
    --config <path>     weftd config file (default: $WEFT_CONFIG, else
                        /etc/weft/weft.toml) — read only for storage.url
    --password <pw>     password for `create` (else $WEFT_ADMIN_PASSWORD)
";

/// Entry point for `weftd admin <args…>` (args after the `admin` token).
pub async fn run(args: &[String]) -> Result<()> {
    let mut config_path: Option<String> = None;
    let mut password: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => config_path = Some(it.next().context("--config needs a path")?.clone()),
            "--password" => {
                password = Some(it.next().context("--password needs a value")?.clone())
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            _ => positional.push(a.clone()),
        }
    }

    let Some(cmd) = positional.first().cloned() else {
        println!("{HELP}");
        return Ok(());
    };

    let store = connect(config_path).await?;
    let account_arg = |usage: &str| -> Result<Account> {
        let name = positional.get(1).with_context(|| usage.to_string())?;
        name.parse::<Account>()
            .map_err(|_| anyhow::anyhow!("invalid account name: {name}"))
    };

    match cmd.as_str() {
        "create" => {
            let account = account_arg("usage: weftd admin create <account>")?;
            let pw = password
                .or_else(|| std::env::var("WEFT_ADMIN_PASSWORD").ok())
                .context("no password — pass --password or set $WEFT_ADMIN_PASSWORD")?;
            if pw.is_empty() {
                bail!("password must not be empty");
            }
            let created = store.register(&account, PasswordHash::new(&pw).as_phc()).await?;
            store.set_operator(&account, true).await?;
            if created {
                println!("created operator account `{account}`");
            } else {
                println!("account `{account}` already existed — marked as operator");
            }
        }
        "grant" => {
            let account = account_arg("usage: weftd admin grant <account>")?;
            if store.set_operator(&account, true).await? {
                println!("`{account}` is now an operator");
            } else {
                bail!("no such account `{account}` — register it (or use `create`) first");
            }
        }
        "revoke" => {
            let account = account_arg("usage: weftd admin revoke <account>")?;
            if store.set_operator(&account, false).await? {
                println!("`{account}` is no longer an operator");
            } else {
                bail!("no such account `{account}`");
            }
        }
        "list" => {
            let ops = store.list_operators().await?;
            if ops.is_empty() {
                println!("(no operators — create one with `weftd admin create <account>`)");
            } else {
                for op in ops {
                    println!("{op}");
                }
            }
        }
        other => bail!("unknown command `{other}` — try `weftd admin --help`"),
    }
    Ok(())
}

/// Load the config just for its Postgres URL and connect (migrations run on
/// connect, so a fresh DB is fine). The CLI requires the postgres backend — an
/// in-memory store in a separate process couldn't affect the running server.
async fn connect(config_path: Option<String>) -> Result<PgStore> {
    let path = config_path
        .or_else(|| std::env::var("WEFT_CONFIG").ok())
        .unwrap_or_else(|| "/etc/weft/weft.toml".to_string());
    let config = crate::config::load(&path)
        .with_context(|| format!("loading config {path} — set --config or $WEFT_CONFIG"))?;
    if config.storage.backend != crate::config::StorageBackend::Postgres {
        bail!("the admin CLI requires the postgres backend (storage.backend = \"postgres\")");
    }
    let url = config
        .storage
        .url
        .as_deref()
        .context("storage.url is required for the postgres backend")?;
    PgStore::connect(url).await.context("connecting to postgres")
}
