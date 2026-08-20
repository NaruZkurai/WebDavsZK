//! WebDAV handler construction and the hyper server loop.
//!
//! ## KDE Bug #422668 ("WebDAV copy fails in the middle")
//!
//! KIO/Dolphin copies of large files (> ~500 MB) used to die mid-transfer with
//! a "connection was disconnected" error. The bug lives in KIO's buffering,
//! but a server that imposes request-body timeouts or buffers whole uploads
//! in memory makes it dramatically worse.
//!
//! Server-side mitigations implemented here:
//!
//! 1. **No read/idle/write timeout on active transfers.** We build hyper's
//!    HTTP/1.1 connection *without a timer*. hyper only ever times out when
//!    you install a timer; without one a PUT body may stream for as long as
//!    it needs. Keep-alive applies only *between* requests, never during an
//!    active body stream. This is the single most important fix for #422668.
//! 2. **Streaming PUTs.** `dav-server` writes each incoming body chunk
//!    straight to disk (`LocalFs::write_buf`), so memory stays flat no matter
//!    how big the upload is.
//! 3. **`Expect: 100-continue`.** hyper answers `100 Continue` the moment the
//!    handler starts polling the body, so KIO's preflighted large PUTs start
//!    cleanly instead of hanging.
//! 4. **Chunked transfer-encoding** is supported natively (KIO uses it when
//!    it can't know the length up front).
//! 5. **Partial PUT / resume** — `dav-server` implements Apache `Content-Range`
//!    and SabreDAV `X-Update-Range` partial uploads and advertises
//!    `Accept-Ranges: bytes`, so clients that support resumable uploads can
//!    recover instead of restarting a multi-GB copy from zero.
//! 6. **`X-Expected-Entity-Length`** is honoured (used by macOS Finder and
//!    some other clients).
//!
//! Each connection is served in its own task, so KIO's parallel `MKCOL` +
//! `PUT` requests during a recursive directory copy run concurrently.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use std::path::PathBuf;

use anyhow::{Context, Result};
use dav_server::DavHandler;
use dav_server::body::Body;
use dav_server::fs::GuardedFileSystem;
use dav_server::memls::MemLs;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::{debug, info, warn};
use rustls::ServerConfig;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::auth::BasicAuth;
use crate::config::Config;

/// Reserved path for the auto-update webhook (`POST` with a token header).
const WEBHOOK_PATH: &str = "/.nzk-webdavs-update";

/// Build the WebDAV handler (filesystem + locksystem + behaviour options).
pub fn build_handler(cfg: &Config) -> Result<DavHandler> {
    // Make sure the root directory exists before we start serving.
    std::fs::create_dir_all(&cfg.root)
        .with_context(|| format!("create root dir {}", cfg.root.display()))?;

    // Robust wrapper: auto-creates missing parents and/or writes atomically
    // (temp file + rename), controlled by --create-parents / --atomic-writes.
    let filesystem: Box<dyn GuardedFileSystem<()>> = Box::new(crate::autofs::AutoMkcolFs::new(
        &cfg.root,
        cfg.create_parents,
        cfg.atomic_writes,
        cfg.fsync,
    ));

    let mut builder = DavHandler::builder()
        // `public = true` -> created files get mode 644/755 instead of 600/700,
        // so files remain accessible outside the server process.
        .filesystem(filesystem)
        // In-memory LOCK/UNLOCK state; cheap to clone, shared across connections.
        .locksystem(MemLs::new())
        .principal(cfg.principal.clone())
        // GET on a directory returns an HTML index (handy for a quick browser check).
        .autoindex(true)
        // Show symlinks in directory listings. The root serves the data drives
        // via symlinks (e.g. /srv/webdav/sda2 -> /mnt/data/sda2), so hiding
        // symlinks (the default) would make them invisible to WebDAV clients.
        .hide_symlinks(false)
        // Stream GET bodies in 1 MiB chunks.
        .read_buf_size(1024 * 1024);

    // Optional URL prefix, e.g. serve under https://host:8443/dav/...
    if !cfg.prefix.trim().is_empty() {
        let mut prefix = cfg.prefix.trim().trim_end_matches('/').to_string();
        if !prefix.starts_with('/') {
            prefix.insert(0, '/');
        }
        builder = builder.strip_prefix(prefix);
    }

    Ok(builder.build_handler())
}

/// Serve WebDAV(S) until SIGINT/SIGTERM, then drain in-flight connections.
pub async fn serve(cfg: Config, handler: DavHandler, tls: Option<Arc<ServerConfig>>) -> Result<()> {
    let auth = match (&cfg.auth_user, &cfg.auth_pass) {
        (Some(user), Some(pass)) => Some(BasicAuth::new(user.clone(), pass.clone(), "nzk-webdavs")),
        _ => None,
    };

    let webhook: Option<(String, PathBuf)> = cfg
        .update_secret
        .as_ref()
        .map(|secret| (secret.clone(), cfg.update_cmd.clone()));
    if webhook.is_some() {
        info!("auto-update webhook enabled at {WEBHOOK_PATH}");
    }

    let listener = TcpListener::bind(&cfg.bind)
        .await
        .with_context(|| format!("bind {}", cfg.bind))?;
    let local = listener.local_addr().context("local address")?;

    info!("nzk-webdavs listening on {local}");
    info!(
        "serving {} over {}",
        cfg.root.display(),
        if cfg.no_tls {
            "plain HTTP (TLS disabled)"
        } else {
            "WebDAVS (TLS)"
        }
    );
    if auth.is_some() {
        info!("basic authentication enabled");
    } else {
        info!("no authentication configured - anyone with network access can read/write");
    }

    let mut tasks: JoinSet<Result<()>> = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                // Send TCP keep-alive probes on idle connections so they stay
                // alive through NATs/firewalls during long user pauses (e.g.
                // KIO's overwrite dialog) instead of being dropped mid-session.
                if cfg.keepalive_secs > 0 {
                    set_tcp_keepalive(&stream, cfg.keepalive_secs);
                }
                // The target (local) address the client connected to, for the
                // request log. Overridable with --target-ip (e.g. a public IP).
                let local = stream.local_addr().unwrap_or(peer);
                let target = match &cfg.target_ip {
                    Some(ip) if !ip.is_empty() => ip.clone(),
                    _ => local.to_string(),
                };
                let handler = handler.clone();
                let auth = auth.clone();
                let tls = tls.clone();
                let webhook = webhook.clone();
                tasks.spawn(async move {
                    match tls {
                        Some(tls) => {
                            let acceptor = TlsAcceptor::from(tls);
                            let tls_stream = acceptor
                                .accept(stream)
                                .await
                                .with_context(|| format!("TLS handshake with {peer}"))?;
                            serve_conn(
                                TokioIo::new(tls_stream),
                                peer,
                                target,
                                handler,
                                auth,
                                webhook,
                            )
                            .await
                        }
                        None => {
                            serve_conn(TokioIo::new(stream), peer, target, handler, auth, webhook)
                                .await
                        }
                    }
                });
            }
            _ = shutdown_signal() => {
                info!("shutdown signal received, draining in-flight connections ...");
                break;
            }
        }
    }

    // Give in-flight transfers a bounded grace period to finish cleanly so a
    // mid-upload restart doesn't truncate the file KIO is writing.
    let grace = Duration::from_secs(30);
    let drained = tokio::time::timeout(grace, async {
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                debug!("connection task error: {e}");
            }
        }
    })
    .await;
    if drained.is_err() {
        warn!("grace period elapsed with connections still active; exiting anyway");
    }

    Ok(())
}

/// Serve a single (optionally TLS-wrapped) connection until it closes.
async fn serve_conn<I>(
    io: I,
    peer: SocketAddr,
    target: String,
    handler: DavHandler,
    auth: Option<BasicAuth>,
    webhook: Option<(String, PathBuf)>,
) -> Result<()>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |req: Request<Incoming>| {
        let handler = handler.clone();
        let auth = auth.clone();
        let target = target.clone();
        let webhook = webhook.clone();
        async move {
            let method = req.method().clone();
            let path = req.uri().path().to_string();
            let start = std::time::Instant::now();

            // Auto-update webhook: POST /.nzk-webdavs-update with a matching
            // X-Nzk-Update-Token triggers a git pull + rebuild + restart so the
            // server updates the moment a client pushes. Guarded by its own
            // secret (not Basic auth), compared in constant time.
            if method == Method::POST && path == WEBHOOK_PATH {
                let resp = match &webhook {
                    Some((secret, cmd)) => {
                        let token = req
                            .headers()
                            .get("x-nzk-update-token")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("");
                        if token.as_bytes().ct_eq(secret.as_bytes()).into() {
                            let cmd = cmd.to_string_lossy().to_string();
                            tokio::task::spawn_blocking(move || {
                                let _ = std::process::Command::new("sh")
                                    .arg("-c")
                                    .arg(format!(
                                        "nohup '{cmd}' >/tmp/nzk-webdavs-update.log 2>&1 &"
                                    ))
                                    .spawn();
                            });
                            Response::builder()
                                .status(StatusCode::ACCEPTED)
                                .body(Body::empty())
                                .unwrap()
                        } else {
                            Response::builder()
                                .status(StatusCode::FORBIDDEN)
                                .body(Body::empty())
                                .unwrap()
                        }
                    }
                    None => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Body::empty())
                        .unwrap(),
                };
                info!(
                    "REQ {method} {path} -> {} in {}ms from {peer} to {target}",
                    resp.status(),
                    start.elapsed().as_millis()
                );
                return Ok::<_, Infallible>(resp);
            }

            let resp = if let Some(auth) = &auth
                && !auth.is_authorized(req.headers())
            {
                debug!("rejecting unauthenticated request from {peer}");
                auth.challenge()
            } else {
                handler.handle(req).await
            };
            info!(
                "REQ {method} {path} -> {} in {}ms from {peer} to {target}",
                resp.status(),
                start.elapsed().as_millis()
            );
            Ok::<_, Infallible>(resp)
        }
    });

    http1::Builder::new()
        .keep_alive(true)
        .serve_connection(io, service)
        .await
        .context("HTTP connection")?;
    Ok(())
}

/// Listen for SIGINT (Ctrl-C) and SIGTERM (systemd `stop`).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

/// Enable TCP keep-alive on a connection so it stays alive through NATs and
/// firewalls during long idle gaps (e.g. KIO's overwrite dialog). On Linux this
/// also sets the idle/interval/retry knobs; elsewhere only `SO_KEEPALIVE`.
fn set_tcp_keepalive(stream: &tokio::net::TcpStream, idle_secs: u64) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let one: libc::c_int = 1;
        let idle: libc::c_int = idle_secs.min(i32::MAX as u64) as libc::c_int;
        let intvl: libc::c_int = 75;
        let cnt: libc::c_int = 9;
        let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                &one as *const libc::c_int as *const libc::c_void,
                len,
            );
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPIDLE,
                    &idle as *const libc::c_int as *const libc::c_void,
                    len,
                );
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPINTVL,
                    &intvl as *const libc::c_int as *const libc::c_void,
                    len,
                );
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPCNT,
                    &cnt as *const libc::c_int as *const libc::c_void,
                    len,
                );
            }
            #[cfg(target_os = "macos")]
            {
                let idle_ms: libc::c_int = (idle as libc::c_int).saturating_mul(1000);
                libc::setsockopt(
                    fd,
                    libc::IPPROTO_TCP,
                    libc::TCP_KEEPALIVE,
                    &idle_ms as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (stream, idle_secs);
    }
}
