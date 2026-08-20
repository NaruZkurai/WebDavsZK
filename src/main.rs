mod auth;
mod autofs;
mod config;
mod logging;
mod server;
mod tls;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use config::Config;
use log::info;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::parse();
    cfg.validate()?;
    init_logging(&cfg);

    // Ensure the served root exists, then refuse to start if the TLS
    // certificate/private key would be served from inside it.
    std::fs::create_dir_all(&cfg.root)?;
    tls::guard_certs_outside_root(&cfg)?;

    if cfg.gen_cert {
        tls::generate_self_signed(&cfg)?;
        return Ok(());
    }

    let handler = server::build_handler(&cfg)?;

    let tls = if cfg.no_tls {
        None
    } else {
        let server_config = tls::build_server_config(&cfg.cert, &cfg.key)?;
        info!(
            "TLS enabled: cert {} / key {}",
            cfg.cert.display(),
            cfg.key.display()
        );
        Some(Arc::new(server_config))
    };

    server::serve(cfg, handler, tls).await
}

fn init_logging(cfg: &Config) {
    let log_file = cfg.log_file.as_deref();
    logging::init(cfg.verbose, log_file);
}
