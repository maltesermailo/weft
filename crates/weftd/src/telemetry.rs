//! Tracing setup. Spans come from weft-core (per session, per verb); this
//! just installs the subscriber. Precedence: `RUST_LOG` (immediate operator
//! override) wins, else the config's `log` filter, else `info`.

use tracing_subscriber::EnvFilter;

pub fn init(config_filter: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(config_filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}
