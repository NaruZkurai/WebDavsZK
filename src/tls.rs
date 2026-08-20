//! TLS support: loading PEM identities, building the rustls server config and
//! generating a self-signed certificate for out-of-the-box WebDAVS.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::config::Config;

/// Load and parse a PEM certificate chain and private key from disk.
pub fn load_identity(
    cert: &Path,
    key: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let certs: Vec<CertificateDer<'static>> = {
        let mut rd = BufReader::new(
            File::open(cert).with_context(|| format!("open certificate {}", cert.display()))?,
        );
        rustls_pemfile::certs(&mut rd)
            .collect::<std::result::Result<_, _>>()
            .with_context(|| format!("parse certificate {}", cert.display()))?
    };
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", cert.display());
    }

    let key = {
        let mut rd = BufReader::new(
            File::open(key).with_context(|| format!("open private key {}", key.display()))?,
        );
        rustls_pemfile::private_key(&mut rd)
            .with_context(|| format!("parse private key {}", key.display()))?
            .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key.display()))?
    };

    Ok((certs, key))
}

/// Refuse to start if the TLS certificate or private key would be served from
/// inside the WebDAV root. Relative cert paths (e.g. `certs/server.key`)
/// combined with a working directory inside the root would otherwise let any
/// client download the private key over WebDAV.
pub fn guard_certs_outside_root(cfg: &Config) -> Result<()> {
    if cfg.no_tls {
        return Ok(());
    }
    let root = std::fs::canonicalize(&cfg.root)
        .with_context(|| format!("resolve WebDAV root {}", cfg.root.display()))?;
    for (kind, path) in [("certificate", &cfg.cert), ("private key", &cfg.key)] {
        // Canonicalize the file itself, or its parent if it doesn't exist yet
        // (e.g. before `--gen-cert` has written it).
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| {
            path.parent()
                .and_then(|d| std::fs::canonicalize(d).ok())
                .unwrap_or_else(|| path.clone())
        });
        if canon.starts_with(&root) {
            anyhow::bail!(
                "TLS {kind} at {} resolves inside the WebDAV root {} and would be \
                 served to clients. Move it outside the root (e.g. /etc/nzk-webdavs/) \
                 and update NZK_WEBDAVS_CERT / NZK_WEBDAVS_KEY.",
                path.display(),
                root.display()
            );
        }
    }
    Ok(())
}

/// Build a rustls [`ServerConfig`] (TLS 1.2 + 1.3, `ring` crypto provider).
pub fn build_server_config(cert: &Path, key: &Path) -> Result<ServerConfig> {
    let (certs, key) = load_identity(cert, key)?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configure TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("install certificate / private key")?;

    Ok(config)
}

/// Generate a self-signed certificate and write PEM files to `--cert`/`--key`.
///
/// Uses the SAN hostnames from `--cert-san` so clients connecting via the
/// server's hostname / IP don't hit name-mismatch errors.
pub fn generate_self_signed(cfg: &Config) -> Result<()> {
    let sans: Vec<String> = cfg
        .cert_san
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let certified = rcgen::generate_simple_self_signed(sans).context("generate certificate")?;

    let cert_pem = certified.cert.pem();
    let key_pem = certified.key_pair.serialize_pem();

    if let Some(parent) = cfg.cert.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    if let Some(parent) = cfg.key.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }

    std::fs::write(&cfg.cert, cert_pem).with_context(|| format!("write {}", cfg.cert.display()))?;
    std::fs::write(&cfg.key, key_pem).with_context(|| format!("write {}", cfg.key.display()))?;

    // Restrict access to the private key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg.key, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", cfg.key.display()))?;
    }

    println!("Generated self-signed certificate:");
    println!("  cert: {}", cfg.cert.display());
    println!("  key:  {}", cfg.key.display());
    println!("SAN : {}", cfg.cert_san);
    println!("Trust this cert on the client, or run behind a reverse proxy with a proper CA.");
    Ok(())
}
