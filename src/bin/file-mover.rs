//! file-mover: local two-panel drag-and-drop file mover (Rust).
//!
//! Serves the two-panel UI at http://0.0.0.0:8787 and moves/copies files
//! between local folders (defaults to the rclone WebDAV mounts:
//! `~/webdav` = Target PC / nas, `~/webdav-local` = This PC).
//!
//! Privileged access: when an operation hits a permission-denied path and the
//! UI supplies a sudo password, the mover retries the operation as root via
//! the sibling `file-mover-sudo` binary (`sudo -S`). The password is used once
//! and never stored or logged.
//!
//! Run:  cargo run --bin file-mover

use std::collections::HashMap;
use std::convert::Infallible;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use dav_server::body::Body;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

const HTML: &str = include_str!("../../tools/file-mover/index.html");
const HOST: &str = "0.0.0.0";
const PORT: u16 = 8787;
// Short git commit this binary was built from. Embedded via build.rs +
// `GIT_COMMIT=$(git rev-parse --short HEAD)` in deploy-file-mover.sh, so the
// running daemon can report its exact version (see GET /api/version).
fn git_commit() -> &'static str {
    option_env!("GIT_COMMIT").unwrap_or("unknown")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn roots() -> (String, String) {
    let h = home();
    (
        h.join("webdav").to_string_lossy().into_owned(),
        h.join("webdav-local").to_string_lossy().into_owned(),
    )
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = percent_decode(it.next().unwrap_or(""));
        let v = percent_decode(it.next().unwrap_or(""));
        m.insert(k, v);
    }
    m
}

/// Map a KDE/Dolphin `webdav://` / `webdavs://` URL to a local mount path. Each
/// rclone mount stands in for one WebDAV server:
///   root A (~/webdav = "Target PC"/nas)          <- NZK_WEBDAVS_URL_A (default 192.0.2.1:51337)
///   root B (~/webdav-local = "This PC")          <- NZK_WEBDAVS_URL_B (default 192.0.2.2:51337)
/// so pasting a KIO shortcut URL into the path box just works. Replace the
/// default host:port below (or set NZK_WEBDAVS_URL_A/B) with your servers.
fn webdav_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("webdavs://")
        .or_else(|| url.strip_prefix("webdav://"))?;
    let after_at = rest.rsplit('@').next()?; // strip optional user[:pass]@
    let (authority, path) = match after_at.find('/') {
        Some(i) => (&after_at[..i], &after_at[i..]),
        None => (after_at, ""),
    };
    let host = authority.split(':').next().unwrap_or("");
    let port = authority.split(':').nth(1).unwrap_or("");
    let hostport = format!("{host}:{port}");
    let (root_a, root_b) = roots();
    let url_a = std::env::var("NZK_WEBDAVS_URL_A").unwrap_or_else(|_| "192.0.2.1:51337".into());
    let url_b = std::env::var("NZK_WEBDAVS_URL_B").unwrap_or_else(|_| "192.0.2.2:51337".into());
    let root = if hostport == url_a {
        PathBuf::from(root_a)
    } else if hostport == url_b {
        PathBuf::from(root_b)
    } else {
        return None;
    };
    Some(root.join(path.trim_start_matches('/')))
}

/// Turn any path-ish string into a local path; WebDAV URLs are resolved to the
/// matching rclone mount path.
fn resolve_path(p: &str) -> PathBuf {
    if (p.starts_with("webdav://") || p.starts_with("webdavs://"))
        && let Some(r) = webdav_url_to_path(p)
    {
        return r;
    }
    PathBuf::from(p)
}

// ---------- filesystem helpers ----------

fn list_entries(p: &Path) -> std::io::Result<Vec<Value>> {
    let mut names: Vec<String> = Vec::new();
    for e in std::fs::read_dir(p)?.flatten() {
        names.push(e.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    let mut entries = Vec::new();
    for name in names {
        let fp = p.join(&name);
        if let Ok(md) = std::fs::symlink_metadata(&fp) {
            let is_dir = md.file_type().is_dir();
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entries.push(json!({
                "name": name,
                "is_dir": is_dir,
                "size": if is_dir { 0 } else { md.len() },
                "mtime": mtime,
                "link": md.file_type().is_symlink(),
            }));
        }
    }
    Ok(entries)
}

fn dest_for(src: &Path, dst: &Path) -> PathBuf {
    if dst.is_dir() {
        dst.join(src.file_name().unwrap_or_default())
    } else {
        dst.to_path_buf()
    }
}

/// Pick a non-existing destination by appending `_2`, `_3`, ... (e.g.
/// `Decompiled_Renamed_2`). Used when the user chooses the "rename" strategy.
fn free_dst(dst: &Path) -> PathBuf {
    if !dst.exists() {
        return dst.to_path_buf();
    }
    let parent = dst.parent().unwrap_or(Path::new(""));
    let stem = dst
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = dst
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 2..1000u32 {
        let cand = parent.join(format!("{stem}_{i}{ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    parent.join(format!("{stem}_999{ext}"))
}

#[derive(Clone)]
struct Job {
    op: String,
    src: String,
    dst: String,
    total_files: u64,
    done_files: u64,
    total_dirs: u64,
    done_dirs: u64,
    total_bytes: u64,
    done_bytes: u64,
    current: Vec<String>,
    status: String,
    error: Option<String>,
}

static JOBS: LazyLock<Arc<Mutex<HashMap<u64, Job>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn job_update(jobs: &Arc<Mutex<HashMap<u64, Job>>>, id: u64, f: impl FnOnce(&mut Job)) {
    if let Ok(mut m) = jobs.lock()
        && let Some(j) = m.get_mut(&id)
    {
        f(j);
    }
}

fn walk_files(
    src: &Path,
    rel: &Path,
    files: &mut Vec<(PathBuf, PathBuf)>,
    dirs: &mut Vec<PathBuf>,
    bytes: &mut u64,
) -> std::io::Result<()> {
    if src.is_dir() {
        dirs.push(rel.to_path_buf());
        for e in std::fs::read_dir(src)? {
            let e = e?;
            walk_files(&e.path(), &rel.join(e.file_name()), files, dirs, bytes)?;
        }
    } else {
        *bytes += std::fs::symlink_metadata(src)?.len();
        files.push((src.to_path_buf(), rel.to_path_buf()));
    }
    Ok(())
}

/// Copy a single file with a plain read/write loop. Rust's `std::fs::copy`
/// uses copy_file_range/sendfile on Linux, which rclone FUSE mounts don't
/// handle properly (it can report success while writing nothing). A plain
/// loop behaves like `cp`/`cat` and works on rclone mounts.
fn copy_file(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut r = std::fs::File::open(src)?;
    let mut w = std::fs::File::create(dst)?;
    let perm = r.metadata()?.permissions();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        total += n as u64;
    }
    let _ = w.set_permissions(perm);
    w.flush()?;
    Ok(total)
}

/// Join a relative path onto a destination. `rel == ""` (a single-file source,
/// or the tree root) must map to `dst` itself — `dst.join("")` yields a
/// trailing slash which the OS treats as a directory (EISDIR on open/create).
fn target_path(dst: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() {
        dst.to_path_buf()
    } else {
        dst.join(rel)
    }
}

/// Split a search query into a name substring and extension/suffix filters.
///   "foo"          -> substring "foo"
///   "*.png"        -> suffix ".png"
///   "*.png;*.jpg"  -> suffixes ".png", ".jpg"
/// Matching is case-insensitive.
fn parse_search_query(q: &str) -> (String, Vec<String>) {
    let mut suffixes = Vec::new();
    let mut subs = Vec::new();
    for tok in q.split(';') {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(sfx) = t.strip_prefix("*.") {
            suffixes.push(format!(".{}", sfx.to_lowercase()));
        } else {
            subs.push(t.to_lowercase());
        }
    }
    (subs.join(" "), suffixes)
}

const MAX_RESULTS: usize = 1000;

struct Search {
    root: String,
    q: String,
    status: String,
    done_dirs: u64,
    matches: Vec<Value>,
    truncated: bool,
    error: Option<String>,
}

static SEARCHES: LazyLock<Arc<Mutex<HashMap<u64, Search>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
static NEXT_SEARCH: AtomicU64 = AtomicU64::new(1);

fn search_update(
    searches: &Arc<Mutex<HashMap<u64, Search>>>,
    id: u64,
    f: impl FnOnce(&mut Search),
) {
    if let Ok(mut m) = searches.lock()
        && let Some(s) = m.get_mut(&id)
    {
        f(s);
    }
}

/// Recursively walk `dir`, pushing matching entries straight into the shared
/// Search record so the UI can stream results while the walk is still running.
fn walk_search(
    searches: &Arc<Mutex<HashMap<u64, Search>>>,
    id: u64,
    dir: &Path,
    sub: &str,
    suffixes: &[String],
) {
    if let Ok(m) = searches.lock()
        && let Some(s) = m.get(&id)
        && (s.truncated || s.matches.len() >= MAX_RESULTS)
    {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        let matched = if suffixes.is_empty() {
            sub.is_empty() || lower.contains(sub)
        } else {
            suffixes.iter().any(|s| lower.ends_with(s))
        };
        if matched {
            let fp = e.path();
            let md = std::fs::symlink_metadata(&fp).ok();
            let is_dir = md.as_ref().map(|m| m.file_type().is_dir()).unwrap_or(false);
            let size = md
                .as_ref()
                .map(|m| if is_dir { 0 } else { m.len() })
                .unwrap_or(0);
            let mut m = searches.lock().unwrap();
            if let Some(s) = m.get_mut(&id) {
                if s.matches.len() >= MAX_RESULTS {
                    s.truncated = true;
                    break;
                }
                s.matches.push(json!({
                    "name": name,
                    "path": fp.to_string_lossy(),
                    "is_dir": is_dir,
                    "size": size,
                }));
            }
        }
        // DirEntry::file_type does not follow symlinks, so this can't loop.
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            search_update(searches, id, |s| s.done_dirs += 1);
            walk_search(searches, id, &e.path(), sub, suffixes);
        }
    }
}

fn run_search(searches: Arc<Mutex<HashMap<u64, Search>>>, id: u64, root: PathBuf, q: String) {
    let (sub, suffixes) = parse_search_query(&q);
    walk_search(&searches, id, &root, &sub, &suffixes);
    if let Ok(mut m) = searches.lock()
        && let Some(s) = m.get_mut(&id)
    {
        s.matches.sort_by(|a, b| {
            let ap = a.get("path").and_then(|x| x.as_str()).unwrap_or("");
            let bp = b.get("path").and_then(|x| x.as_str()).unwrap_or("");
            ap.cmp(bp)
        });
        s.status = "done".into();
    }
}

async fn handle_search_start(req: Request<Incoming>) -> Response<Body> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": "bad json"})),
    };
    let root = v
        .get("root")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let q = v
        .get("q")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if root.is_empty() || q.is_empty() {
        return json_resp(StatusCode::BAD_REQUEST, json!({"error": "bad request"}));
    }
    let id = NEXT_SEARCH.fetch_add(1, Ordering::Relaxed);
    {
        let mut m = SEARCHES.lock().unwrap();
        m.insert(
            id,
            Search {
                root: root.clone(),
                q: q.clone(),
                status: "running".into(),
                done_dirs: 0,
                matches: Vec::new(),
                truncated: false,
                error: None,
            },
        );
    }
    let root_p = resolve_path(&root);
    let searches = SEARCHES.clone();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || run_search(searches, id, root_p, q)).await;
    });
    json_resp(StatusCode::ACCEPTED, json!({"search_id": id}))
}

async fn handle_search(params: &HashMap<String, String>) -> Response<Body> {
    let id = match params.get("id").and_then(|x| x.parse::<u64>().ok()) {
        Some(id) => id,
        None => return json_resp(StatusCode::BAD_REQUEST, json!({"error": "missing id"})),
    };
    let m = SEARCHES.lock().unwrap();
    match m.get(&id) {
        Some(s) => json_resp(
            StatusCode::OK,
            json!({
                "search_id": id,
                "root": s.root,
                "q": s.q,
                "status": s.status,
                "done_dirs": s.done_dirs,
                "matches": s.matches,
                "truncated": s.truncated,
                "error": s.error,
            }),
        ),
        None => json_resp(StatusCode::NOT_FOUND, json!({"error": "no such search"})),
    }
}

/// Return the top-level "drive" (first path component after the mount root,
/// e.g. `sda4` / `sdc2`) that a path sits on, if it's inside one of the two
/// mount roots (`~/webdav`, `~/webdav-local`). Used to tell same-drive moves
/// (which can be a single fast rename) from cross-drive moves (which must
/// copy data).
fn path_drive(mounts: &[PathBuf], p: &Path) -> Option<PathBuf> {
    for m in mounts {
        if let Ok(rest) = p.strip_prefix(m) {
            return rest
                .components()
                .next()
                .map(|c| PathBuf::from(c.as_os_str()));
        }
    }
    None
}

/// Move/copy a tree on the blocking pool, processing files in parallel and
/// updating the shared job state so the UI can show live progress.
fn run_job(jobs: Arc<Mutex<HashMap<u64, Job>>>, id: u64, op: String, src: PathBuf, dst: PathBuf) {
    job_update(&jobs, id, |j| j.status = "running".into());
    let (root_a, root_b) = roots();
    let mounts = [PathBuf::from(&root_a), PathBuf::from(&root_b)];
    let same_drive = path_drive(&mounts, &src) == path_drive(&mounts, &dst);

    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Err(e) = walk_files(&src, Path::new(""), &mut files, &mut dirs, &mut total_bytes) {
        job_update(&jobs, id, |j| {
            j.status = "error".into();
            j.error = Some(e.to_string());
        });
        return;
    }
    job_update(&jobs, id, |j| {
        j.total_files = files.len() as u64;
        j.total_dirs = dirs.len() as u64;
        j.total_bytes = total_bytes;
    });

    // FAST PATH for a move: renaming the WHOLE source tree in one shot is a
    // pure metadata operation (`mv -r`) — no data is copied at all, which is
    // ideal for SAME-DRIVE moves of large trees (e.g. sda4 -> sda4). We only
    // take it when the destination doesn't already exist AND the move stays on
    // the same drive; a cross-drive move (e.g. sda4 -> sdc2) must copy data,
    // so it falls through to the per-file copy path below.
    if op == "move"
        && same_drive
        && !dst.exists()
        && let Ok(()) = std::fs::rename(&src, &dst)
    {
        job_update(&jobs, id, |j| {
            j.done_files = j.total_files;
            j.done_dirs = j.total_dirs;
            j.done_bytes = j.total_bytes;
            j.status = "done".into();
        });
        return;
    }

    for rel in &dirs {
        let df = target_path(&dst, rel);
        if std::fs::create_dir_all(&df).is_ok() {
            job_update(&jobs, id, |j| {
                j.done_dirs += 1;
                j.current.push(df.to_string_lossy().into_owned());
                if j.current.len() > 6 {
                    j.current.remove(0);
                }
            });
        }
    }

    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let files = Arc::new(files);
    let next = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let dst = Arc::new(dst);

    std::thread::scope(|s| {
        for _ in 0..nthreads {
            let files = files.clone();
            let next = next.clone();
            let failed = failed.clone();
            let dst = dst.clone();
            let jobs = jobs.clone();
            let op = op.clone();
            s.spawn(move || {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= files.len() {
                        break;
                    }
                    let (sf, rel) = &files[idx];
                    let df = target_path(&dst, rel);
                    let size = std::fs::symlink_metadata(sf).map(|m| m.len()).unwrap_or(0);
                    job_update(&jobs, id, |j| {
                        j.current.push(sf.to_string_lossy().into_owned());
                        if j.current.len() > 6 {
                            j.current.remove(0);
                        }
                    });
                    let (ok, err) = if op == "move" && same_drive {
                        match std::fs::rename(sf, &df) {
                            Ok(_) => (true, None),
                            // rclone FUSE returns EIO (os error 5) for a rename
                            // of files it hasn't fully cached/flushed, and
                            // EXDEV when the paths are on different backends.
                            // In both cases fall back to copy + delete so large
                            // same-mount moves still complete.
                            Err(e)
                                if e.kind() == ErrorKind::CrossesDevices
                                    || e.raw_os_error() == Some(5) =>
                            {
                                match copy_file(sf, &df) {
                                    Ok(_) => (std::fs::remove_file(sf).is_ok(), None),
                                    Err(e2) => (false, Some(e2.to_string())),
                                }
                            }
                            Err(e) => (false, Some(e.to_string())),
                        }
                    } else {
                        // Cross-drive move (or plain copy): always transfer data.
                        match copy_file(sf, &df) {
                            // A cross-drive MOVE also removes the source file.
                            Ok(_) => (
                                if op == "move" {
                                    std::fs::remove_file(sf).is_ok()
                                } else {
                                    true
                                },
                                None,
                            ),
                            Err(e) => (false, Some(e.to_string())),
                        }
                    };
                    if !ok {
                        failed.fetch_add(1, Ordering::Relaxed);
                        job_update(&jobs, id, |j| {
                            if j.error.is_none() {
                                j.error = Some(format!(
                                    "{} -> {}: {}",
                                    sf.display(),
                                    df.display(),
                                    err.unwrap_or_default()
                                ));
                            }
                        });
                    }
                    job_update(&jobs, id, |j| {
                        j.done_files += 1;
                        if ok {
                            j.done_bytes += size;
                        }
                    });
                }
            });
        }
    });

    // A move removes the now-empty source tree, but only when every file landed
    // safely — if anything failed, leave the source behind for inspection.
    if op == "move" && failed.load(Ordering::Relaxed) == 0 {
        let _ = std::fs::remove_dir_all(&src);
    }

    job_update(&jobs, id, |j| j.status = "done".into());
}

/// Delete a file or folder tree as a background job, reporting per-file and
/// per-folder progress (files first, then directories deepest-first) into the
/// shared job registry.
fn run_delete_job(jobs: Arc<Mutex<HashMap<u64, Job>>>, id: u64, src: &Path) {
    job_update(&jobs, id, |j| j.status = "running".into());

    if src.is_file() || src.is_symlink() {
        match std::fs::remove_file(src) {
            Ok(_) => job_update(&jobs, id, |j| {
                j.done_files += 1;
                j.total_files = 1;
                j.status = "done".into();
            }),
            Err(e) => job_update(&jobs, id, |j| {
                j.status = "error".into();
                j.error = Some(e.to_string());
            }),
        }
        return;
    }

    // Collect the tree first so we know the totals (files count, then folders).
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut total_bytes: u64 = 0;
    if let Err(e) = walk_delete(src, &mut files, &mut dirs, &mut total_bytes) {
        job_update(&jobs, id, |j| {
            j.status = "error".into();
            j.error = Some(e.to_string());
        });
        return;
    }
    job_update(&jobs, id, |j| {
        j.total_files = files.len() as u64;
        j.total_dirs = dirs.len() as u64;
        j.total_bytes = total_bytes;
    });

    // Delete the files in PARALLEL (delete-heavy workloads over a FUSE mount /
    // many small files speed up dramatically), then remove the now-empty
    // directories bottom-up (which must happen after all their files are gone).
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let files = Arc::new(files);
    let next = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..nthreads {
            let files = files.clone();
            let next = next.clone();
            let failed = failed.clone();
            let jobs = jobs.clone();
            s.spawn(move || {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= files.len() {
                        break;
                    }
                    let f = &files[idx];
                    job_update(&jobs, id, |j| {
                        j.current.push(f.to_string_lossy().into_owned());
                        if j.current.len() > 6 {
                            j.current.remove(0);
                        }
                    });
                    match std::fs::remove_file(f) {
                        Ok(_) => job_update(&jobs, id, |j| j.done_files += 1),
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            job_update(&jobs, id, |j| {
                                if j.error.is_none() {
                                    j.error = Some(format!("{}: {}", f.display(), e));
                                }
                            });
                        }
                    }
                }
            });
        }
    });
    let mut failed = failed.load(Ordering::Relaxed);

    // dirs is pushed parent-first by walk_delete, so iterate in reverse
    // (deepest first) so remove_dir succeeds.
    for d in dirs.iter().rev() {
        job_update(&jobs, id, |j| {
            j.current.push(d.to_string_lossy().into_owned());
            if j.current.len() > 6 {
                j.current.remove(0);
            }
        });
        match std::fs::remove_dir(d) {
            Ok(_) => job_update(&jobs, id, |j| j.done_dirs += 1),
            Err(_) => failed += 1,
        }
    }

    job_update(&jobs, id, |j| {
        j.status = if failed == 0 { "done" } else { "error" }.into();
    });
}

/// Walk a tree, collecting (in post-order) all files and directories for
/// deletion. `dirs` are recorded parent-first and `bytes` is the total size of
/// files in the tree.
fn walk_delete(
    src: &Path,
    files: &mut Vec<PathBuf>,
    dirs: &mut Vec<PathBuf>,
    bytes: &mut u64,
) -> std::io::Result<()> {
    if src.is_dir() {
        dirs.push(src.to_path_buf());
        for e in std::fs::read_dir(src)? {
            let e = e?;
            walk_delete(&e.path(), files, dirs, bytes)?;
        }
    } else {
        let md = std::fs::symlink_metadata(src)?;
        if !md.file_type().is_symlink() {
            *bytes += md.len();
        }
        files.push(src.to_path_buf());
    }
    Ok(())
}

// ---------- sudo elevation ----------

async fn sudo_run(passwd: &str, req: &Value) -> (i32, String) {
    let helper = std::env::current_exe()
        .ok()
        .map(|e| e.with_file_name("file-mover-sudo"))
        .unwrap_or_default();
    let mut child = match tokio::process::Command::new("sudo")
        .args(["-S", "-p", ""])
        .arg(&helper)
        .arg(req.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return (1, String::new()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(format!("{}\n", passwd).as_bytes()).await;
        let _ = stdin.flush().await;
    }
    match child.wait_with_output().await {
        Ok(out) => (
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        ),
        Err(_) => (1, String::new()),
    }
}

// ---------- response helpers ----------

fn text_resp(status: StatusCode, ct: &str, data: Vec<u8>) -> Response<Body> {
    let mut res = Response::new(Body::from(bytes::Bytes::from(data)));
    *res.status_mut() = status;
    res.headers_mut()
        .insert("content-type", ct.parse().unwrap());
    res
}

fn json_resp(status: StatusCode, v: Value) -> Response<Body> {
    text_resp(status, "application/json", v.to_string().into_bytes())
}

// ---------- handlers ----------

async fn handle_ls(params: &HashMap<String, String>) -> Response<Body> {
    // Empty/blank path (the UI always sends `p=` even when the box is empty)
    // defaults to the A/root, so an empty path box lists the root instead of
    // erroring with "No such file or directory (os error 2)".
    let p = params
        .get("p")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| roots().0);
    let pp = resolve_path(&p);
    let pp_s = pp.to_string_lossy().into_owned();
    match list_entries(&pp) {
        Ok(entries) => json_resp(StatusCode::OK, json!({"path": pp_s, "entries": entries})),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            let sp = params.get("sudopass").cloned().unwrap_or_default();
            if !sp.is_empty() {
                let (rc, out) = sudo_run(&sp, &json!({"op": "ls", "p": pp_s})).await;
                if rc == 0
                    && let Ok(v) = serde_json::from_str::<Value>(&out)
                {
                    return json_resp(
                        StatusCode::OK,
                        json!({"path": pp_s, "entries": v["entries"]}),
                    );
                }
                json_resp(
                    StatusCode::FORBIDDEN,
                    json!({"error": "permission denied / bad password"}),
                )
            } else {
                json_resp(
                    StatusCode::FORBIDDEN,
                    json!({"error": "permission denied", "elevation": true}),
                )
            }
        }
        Err(e) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

async fn handle_mkdir(params: &HashMap<String, String>) -> Response<Body> {
    let dst = params.get("dst").cloned().unwrap_or_default();
    let sp = params.get("sudopass").cloned().unwrap_or_default();
    match std::fs::create_dir_all(Path::new(&dst)) {
        Ok(_) => json_resp(StatusCode::OK, json!({"ok": true, "dst": dst})),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            if !sp.is_empty() {
                let (rc, _) = sudo_run(&sp, &json!({"op": "mkdir", "dst": dst})).await;
                if rc == 0 {
                    return json_resp(StatusCode::OK, json!({"ok": true, "dst": dst}));
                }
                return json_resp(
                    StatusCode::FORBIDDEN,
                    json!({"error": "permission denied / bad password"}),
                );
            }
            json_resp(
                StatusCode::FORBIDDEN,
                json!({"error": "permission denied", "elevation": true}),
            )
        }
        Err(e) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        ),
    }
}

async fn handle_put(req: Request<Incoming>, params: &HashMap<String, String>) -> Response<Body> {
    let dst = params.get("dst").cloned().unwrap_or_default();
    let sp = params.get("sudopass").cloned().unwrap_or_default();
    let dstp = PathBuf::from(&dst);
    let tmp = std::env::temp_dir().join(format!(
        "mover-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let mut body = req.into_body();
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            return json_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            );
        }
    };
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return json_resp(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": e.to_string()}),
                );
            }
        };
        if let Some(data) = frame.data_ref()
            && let Err(e) = file.write_all(data).await
        {
            let _ = std::fs::remove_file(&tmp);
            return json_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            );
        }
    }
    if let Err(e) = file.flush().await {
        let _ = std::fs::remove_file(&tmp);
        return json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": e.to_string()}),
        );
    }
    drop(file);

    if let Some(parent) = dstp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::rename(&tmp, &dstp) {
        Ok(_) => json_resp(StatusCode::OK, json!({"ok": true, "dst": dst})),
        Err(e)
            if e.kind() == ErrorKind::PermissionDenied || e.kind() == ErrorKind::CrossesDevices =>
        {
            // CrossesDevices (e.g. /tmp -> home): copy then remove instead.
            if e.kind() == ErrorKind::CrossesDevices {
                match copy_file(&tmp, &dstp) {
                    Ok(_) => {
                        let _ = std::fs::remove_file(&tmp);
                        return json_resp(StatusCode::OK, json!({"ok": true, "dst": dst}));
                    }
                    Err(e2) if e2.kind() == ErrorKind::PermissionDenied => {
                        // fall through to sudo path
                    }
                    Err(e2) => {
                        let _ = std::fs::remove_file(&tmp);
                        return json_resp(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            json!({"error": e2.to_string()}),
                        );
                    }
                }
            }
            if !sp.is_empty() {
                let (rc, _) = sudo_run(
                    &sp,
                    &json!({"op": "put", "dst": dst, "tmp": tmp.to_string_lossy()}),
                )
                .await;
                if rc == 0 {
                    return json_resp(StatusCode::OK, json!({"ok": true, "dst": dst}));
                }
                let _ = std::fs::remove_file(&tmp);
                return json_resp(
                    StatusCode::FORBIDDEN,
                    json!({"error": "permission denied / bad password"}),
                );
            }
            let _ = std::fs::remove_file(&tmp);
            json_resp(
                StatusCode::FORBIDDEN,
                json!({"error": "permission denied", "elevation": true}),
            )
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            json_resp(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        }
    }
}

async fn handle_move_copy(req: Request<Incoming>, is_move: bool) -> Response<Body> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_resp(StatusCode::BAD_REQUEST, json!({"error": "bad json"})),
    };
    let src = v
        .get("src")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let dst = v
        .get("dst")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let sp = v
        .get("sudopass")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let strategy = v
        .get("strategy")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if src.is_empty() || dst.is_empty() {
        return json_resp(StatusCode::BAD_REQUEST, json!({"error": "bad path"}));
    }
    let op_name = if is_move { "move" } else { "copy" };
    let src_p = PathBuf::from(&src);
    let mut final_dst = dest_for(&src_p, Path::new(&dst));
    if src_p == final_dst {
        return json_resp(
            StatusCode::BAD_REQUEST,
            json!({ "error": "source and destination are the same" }),
        );
    }
    if !src_p.exists() {
        return json_resp(StatusCode::NOT_FOUND, json!({ "error": "source missing" }));
    }
    if final_dst.exists() {
        if strategy == "rename" {
            final_dst = free_dst(&final_dst);
        } else if strategy == "merge" || strategy == "overwrite" {
            // merge into the existing destination: run_job creates dirs
            // idempotently and overwrites conflicting files, then (for a move)
            // cleans up the source tree only when everything landed.
        } else {
            return json_resp(
                StatusCode::CONFLICT,
                json!({
                    "error": format!("destination exists: {}", final_dst.display()),
                    "conflict": true,
                    "dst": final_dst.to_string_lossy(),
                }),
            );
        }
    }
    // Elevated path (sudo): run synchronously via the privileged helper.
    if !sp.is_empty() {
        let (rc, out) = sudo_run(
            &sp,
            &json!({
                "op": op_name,
                "src": src,
                "dst": final_dst.to_string_lossy(),
                "strategy": strategy,
            }),
        )
        .await;
        if rc == 0
            && let Ok(j) = serde_json::from_str::<Value>(&out)
        {
            if let Some(d) = j.get("dst").and_then(|x| x.as_str()) {
                return json_resp(
                    StatusCode::OK,
                    json!({ "ok": true, "op": op_name, "dst": d }),
                );
            }
            if j.get("conflict").is_some() {
                return json_resp(
                    StatusCode::CONFLICT,
                    json!({
                        "error": "destination exists",
                        "conflict": true,
                        "dst": j.get("dst").and_then(|x| x.as_str()).unwrap_or(""),
                    }),
                );
            }
        }
        return json_resp(
            StatusCode::FORBIDDEN,
            json!({ "error": "permission denied / bad password" }),
        );
    }
    // DEDUPE + INSERT atomically under one lock. Never start a second job for
    // the same work: a move consumes its source, so a move already
    // queued/running for this src must not be started again; a copy is a
    // duplicate only if it targets the same dst. Holding the lock across both
    // the look-up AND the insert means two concurrent requests for the same
    // src (double-click, retry, or two threads) can NEVER both slip past the
    // check and register two jobs — which would copy/move the same file
    // twice and create duplicates. This is exactly the "build the task list
    // once, then hand each thread a distinct task" guarantee, applied at the
    // job-registration level.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    {
        let mut m = JOBS.lock().unwrap();
        if let Some((eid, _)) = m.iter().find(|(_, j)| {
            (j.status == "queued" || j.status == "running")
                && j.op == op_name
                && j.src == src
                && (op_name == "move" || j.dst == final_dst.to_string_lossy())
        }) {
            return json_resp(
                StatusCode::ACCEPTED,
                json!({ "ok": true, "job_id": eid, "op": op_name, "deduped": true }),
            );
        }
        // Normal path: background job with live progress; server stays
        // responsive. Inserted under the same lock so a concurrent request
        // can't race ahead and register a second job for this src.
        m.insert(
            id,
            Job {
                op: op_name.to_string(),
                src: src.clone(),
                dst: final_dst.to_string_lossy().into_owned(),
                total_files: 0,
                done_files: 0,
                total_dirs: 0,
                done_dirs: 0,
                total_bytes: 0,
                done_bytes: 0,
                current: Vec::new(),
                status: "queued".into(),
                error: None,
            },
        );
    }
    let jobs = JOBS.clone();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            run_job(jobs, id, op_name.to_string(), src_p, final_dst)
        })
        .await;
    });
    json_resp(
        StatusCode::ACCEPTED,
        json!({ "ok": true, "job_id": id, "op": op_name }),
    )
}

fn handle_status() -> Response<Body> {
    let list: Vec<Value> = {
        let m = JOBS.lock().unwrap();
        m.iter()
            .map(|(id, j)| {
                json!({
                    "id": id,
                    "op": j.op,
                    "src": j.src,
                    "dst": j.dst,
                    "total_files": j.total_files,
                    "done_files": j.done_files,
                    "total_dirs": j.total_dirs,
                    "done_dirs": j.done_dirs,
                    "total_bytes": j.total_bytes,
                    "done_bytes": j.done_bytes,
                    "current": j.current,
                    "status": j.status,
                    "error": j.error,
                })
            })
            .collect()
    };
    json_resp(StatusCode::OK, json!({ "jobs": list }))
}

async fn handle_rename(req: Request<Incoming>) -> Response<Body> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_resp(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "bad json" })),
    };
    let src = v
        .get("src")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let name = v
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let sp = v
        .get("sudopass")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if src.is_empty() || name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    let src_p = resolve_path(&src);
    let parent = match src_p.parent() {
        Some(p) => p.to_path_buf(),
        None => return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "no parent" })),
    };
    let dst = parent.join(&name);
    if dst.exists() {
        return json_resp(
            StatusCode::CONFLICT,
            json!({ "error": format!("destination exists: {}", dst.display()) }),
        );
    }
    match std::fs::rename(&src_p, &dst) {
        Ok(_) => json_resp(
            StatusCode::OK,
            json!({ "ok": true, "dst": dst.to_string_lossy() }),
        ),
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            if !sp.is_empty() {
                let (rc, out) = sudo_run(
                    &sp,
                    &json!({ "op": "rename", "src": src_p.to_string_lossy(), "name": name }),
                )
                .await;
                if rc == 0
                    && let Ok(j) = serde_json::from_str::<Value>(&out)
                    && let Some(d) = j.get("dst").and_then(|x| x.as_str())
                {
                    return json_resp(StatusCode::OK, json!({ "ok": true, "dst": d }));
                }
                return json_resp(
                    StatusCode::FORBIDDEN,
                    json!({ "error": "permission denied / bad password" }),
                );
            }
            json_resp(
                StatusCode::FORBIDDEN,
                json!({ "error": "permission denied", "elevation": true }),
            )
        }
        Err(e) => json_resp(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
    }
}

async fn handle_delete(req: Request<Incoming>) -> Response<Body> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return json_resp(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() })),
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "bad json" })),
    };
    let src = v
        .get("src")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let sp = v
        .get("sudopass")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if src.is_empty() {
        return json_resp(StatusCode::BAD_REQUEST, json!({ "error": "bad request" }));
    }
    let p = resolve_path(&src);
    if let Err(e) = std::fs::symlink_metadata(&p) {
        return json_resp(StatusCode::NOT_FOUND, json!({ "error": e.to_string() }));
    }

    // Elevated path (sudo): run synchronously via the privileged helper so the
    // UI gets a definitive result for privileged work.
    if !sp.is_empty() {
        let (rc, out) = sudo_run(&sp, &json!({ "op": "delete", "src": p.to_string_lossy() })).await;
        if rc == 0
            && let Ok(j) = serde_json::from_str::<Value>(&out)
            && j.get("ok").is_some()
        {
            return json_resp(StatusCode::OK, json!({ "ok": true }));
        }
        return json_resp(
            StatusCode::FORBIDDEN,
            json!({ "error": "permission denied / bad password" }),
        );
    }

    // Normal path: background job with live file/folder progress, so large (and
    // even recursive) deletions report counts and don't block the server.
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    {
        let mut m = JOBS.lock().unwrap();
        m.insert(
            id,
            Job {
                op: "delete".into(),
                src: p.to_string_lossy().into_owned(),
                dst: String::new(),
                total_files: 0,
                done_files: 0,
                total_dirs: 0,
                done_dirs: 0,
                total_bytes: 0,
                done_bytes: 0,
                current: Vec::new(),
                status: "queued".into(),
                error: None,
            },
        );
    }
    let jobs = JOBS.clone();
    let p2 = p.clone();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || run_delete_job(jobs, id, &p2)).await;
    });
    json_resp(
        StatusCode::ACCEPTED,
        json!({ "ok": true, "job_id": id, "op": "delete" }),
    )
}

async fn route(req: Request<Incoming>) -> Response<Body> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let params = parse_query(&query);
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => text_resp(
            StatusCode::OK,
            "text/html; charset=utf-8",
            HTML.as_bytes().to_vec(),
        ),
        ("GET", "/api/roots") => {
            let (a, b) = roots();
            json_resp(
                StatusCode::OK,
                json!({"root_a": a, "root_b": b, "target_pc": a, "this_pc": b}),
            )
        }
        ("GET", "/api/version") => json_resp(
            StatusCode::OK,
            json!({
                "commit": git_commit(),
                "version": env!("CARGO_PKG_VERSION"),
                "port": PORT,
            }),
        ),
        // Client asks the server to self-update: pull + rebuild + restart the
        // daemon, launched DETACHED so we can reply before the process is
        // replaced. The client should show an "updating server…" notification
        // and poll /api/version until the daemon comes back with the new
        // commit.
        ("POST", "/api/update") => {
            let script = std::env::var("FM_UPDATE_SCRIPT")
                .unwrap_or_else(|_| "./scripts/update-file-mover.sh".to_string());
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "nohup '{script}' >/tmp/file-mover-update.log 2>&1 &"
                ))
                .spawn();
            json_resp(
                StatusCode::ACCEPTED,
                json!({ "updating": true, "from": git_commit(), "log": "/tmp/file-mover-update.log" }),
            )
        }
        ("GET", "/api/status") => handle_status(),
        ("GET", "/api/ls") => handle_ls(&params).await,
        ("POST", "/api/mkdir") => handle_mkdir(&params).await,
        ("POST", "/api/put") => handle_put(req, &params).await,
        ("POST", "/api/move") | ("POST", "/api/copy") => {
            let is_move = path == "/api/move";
            handle_move_copy(req, is_move).await
        }
        ("POST", "/api/rename") => handle_rename(req).await,
        ("POST", "/api/delete") => handle_delete(req).await,
        ("POST", "/api/search") => handle_search_start(req).await,
        ("GET", "/api/search") => handle_search(&params).await,
        _ => json_resp(StatusCode::NOT_FOUND, json!({"error": "not found"})),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = TcpListener::bind((HOST, PORT)).await?;
    println!("file-mover (rust) → http://{HOST}:{PORT}");
    loop {
        let (stream, _peer) = listener.accept().await?;
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let svc = service_fn(move |req: Request<Incoming>| async move {
                Ok::<_, Infallible>(route(req).await)
            });
            if let Err(e) = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, svc)
                .await
            {
                eprintln!("conn error: {e}");
            }
        });
    }
}
