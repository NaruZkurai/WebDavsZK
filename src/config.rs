use std::path::PathBuf;

use clap::Parser;

/// Robust Rust WebDAVS server tuned for KDE KIO recursive uploads and the KIO
/// WebDAV large-file transfer bug (KDE Bug #422668).
#[derive(Parser, Debug, Clone)]
#[command(name = "nzk-webdavs", version, about, long_about = None)]
pub struct Config {
    /// Address to bind, e.g. 0.0.0.0:8443 or [::]:8443
    #[arg(long, env = "NZK_WEBDAVS_BIND", default_value = "0.0.0.0:8443")]
    pub bind: String,

    /// Root directory served over WebDAV
    #[arg(long, env = "NZK_WEBDAVS_ROOT", default_value = "/srv/webdav")]
    pub root: PathBuf,

    /// URL prefix to strip before mapping to disk (e.g. "/dav"). Empty = serve at root.
    #[arg(long, env = "NZK_WEBDAVS_PREFIX", default_value = "")]
    pub prefix: String,

    /// Path to TLS certificate chain (PEM). Must NOT resolve inside --root,
    /// or the server refuses to start (it would be served to clients).
    #[arg(long, env = "NZK_WEBDAVS_CERT", default_value = "certs/server.crt")]
    pub cert: PathBuf,

    /// Path to TLS private key (PEM). Must NOT resolve inside --root,
    /// or the server refuses to start (it would be served to clients).
    #[arg(long, env = "NZK_WEBDAVS_KEY", default_value = "certs/server.key")]
    pub key: PathBuf,

    /// Disable TLS and serve plain HTTP (debugging / when fronted by a reverse proxy)
    #[arg(long, env = "NZK_WEBDAVS_NO_TLS")]
    pub no_tls: bool,

    /// Generate a self-signed certificate at --cert/--key and exit
    #[arg(long, env = "NZK_WEBDAVS_GEN_CERT")]
    pub gen_cert: bool,

    /// Comma-separated SAN hostnames/IPs for the generated certificate
    #[arg(
        long,
        env = "NZK_WEBDAVS_CERT_SAN",
        default_value = "localhost,0.0.0.0,::1"
    )]
    pub cert_san: String,

    /// Basic-auth username; empty disables authentication
    #[arg(long, env = "NZK_WEBDAVS_AUTH_USER")]
    pub auth_user: Option<String>,

    /// Basic-auth password; empty disables authentication
    #[arg(long, env = "NZK_WEBDAVS_AUTH_PASS")]
    pub auth_pass: Option<String>,

    /// WebDAV lock principal (owner name reported in LOCK responses)
    #[arg(long, env = "NZK_WEBDAVS_PRINCIPAL", default_value = "nzk-webdavs")]
    pub principal: String,

    /// Auto-create missing parent directories on PUT/MKCOL (KIO recursive
    /// upload fix). Set to false for strict RFC 4918 behavior.
    #[arg(
        long,
        env = "NZK_WEBDAVS_CREATE_PARENTS",
        action = clap::ArgAction::Set,
        default_value_t = true
    )]
    pub create_parents: bool,

    /// Write uploads atomically (temp file + rename on success) so concurrent
    /// readers never see a partial file and aborted uploads leave no corrupt
    /// file behind.
    #[arg(
        long,
        env = "NZK_WEBDAVS_ATOMIC_WRITES",
        action = clap::ArgAction::Set,
        default_value_t = true
    )]
    pub atomic_writes: bool,

    /// fsync each finished upload before the atomic rename (maximum
    /// durability). Default off: relies on the OS page cache, which is much
    /// faster when moving many small files.
    #[arg(
        long,
        env = "NZK_WEBDAVS_FSYNC",
        action = clap::ArgAction::Set,
        default_value_t = false
    )]
    pub fsync: bool,

    /// Send TCP keep-alive probes on idle connections after this many seconds
    /// (0 disables). Keeps connections alive through NATs/firewalls during long
    /// user pauses such as KIO's overwrite dialog.
    #[arg(long, env = "NZK_WEBDAVS_KEEPALIVE_SECS", default_value_t = 60)]
    pub keepalive_secs: u64,

    /// Also append logs to this file (in addition to stderr).
    #[arg(long, env = "NZK_WEBDAVS_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Target IP shown in the request log (e.g. your public/NAT IP). Defaults
    /// to the connection's actual local address.
    #[arg(long, env = "NZK_WEBDAVS_TARGET_IP")]
    pub target_ip: Option<String>,

    /// Shared secret for the auto-update webhook. When set, `POST
    /// /.nzk-webdavs-update` with header `X-Nzk-Update-Token: <secret>`
    /// triggers an update (git pull + rebuild + restart) so the server updates
    /// the moment a client pushes. Unset = webhook disabled.
    #[arg(long, env = "NZK_WEBDAVS_UPDATE_SECRET")]
    pub update_secret: Option<String>,

    /// Shell command executed when the update webhook fires. Relative to the
    /// working directory. Default: `scripts/update.sh`.
    #[arg(
        long,
        env = "NZK_WEBDAVS_UPDATE_CMD",
        default_value = "scripts/update.sh"
    )]
    pub update_cmd: PathBuf,

    /// Enable debug logging
    #[arg(long, env = "NZK_WEBDAVS_VERBOSE")]
    pub verbose: bool,
}

impl Config {
    /// Validate the configuration, returning a friendly error on problems.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.auth_user.is_some() != self.auth_pass.is_some() {
            anyhow::bail!("--auth-user and --auth-pass must be set together (or neither)");
        }
        if self.no_tls && self.gen_cert {
            anyhow::bail!("--gen-cert is meaningless together with --no-tls");
        }
        Ok(())
    }
}
