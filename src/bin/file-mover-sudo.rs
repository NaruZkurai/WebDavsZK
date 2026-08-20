//! file-mover-sudo: performs a file operation as root, invoked by the
//! file-mover server through `sudo -S`. Reads a JSON request from argv[1],
//! prints a JSON result on stdout. The password itself is never passed here
//! (it goes to sudo's stdin), so this binary never sees it.
//!
//! Usage: file-mover-sudo '<json>'
//!   {"op":"ls","p":...}
//!   {"op":"move"|"copy","src":...,"dst":...}
//!   {"op":"mkdir","dst":...}
//!   {"op":"put","dst":...,"tmp":...}

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::process::exit;

use serde_json::{Value, json};

fn list_entries(p: &Path) -> Result<Value, String> {
    let mut names: Vec<String> = Vec::new();
    for e in std::fs::read_dir(p).map_err(|e| e.to_string())?.flatten() {
        names.push(e.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    let mut entries = Vec::new();
    for name in names {
        let fp = p.join(&name);
        if let Ok(md) = std::fs::symlink_metadata(&fp) {
            let is_dir = md.file_type().is_dir();
            entries.push(json!({
                "name": name,
                "is_dir": is_dir,
                "size": if is_dir { 0 } else { md.len() },
                "mtime": 0,
                "link": md.file_type().is_symlink(),
            }));
        }
    }
    Ok(json!({ "entries": entries }))
}

/// Copy a single file with a plain read/write loop (rclone FUSE mounts don't
/// handle std::fs::copy's copy_file_range/sendfile correctly).
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

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        copy_file(src, dst)?;
    }
    Ok(())
}

fn remove_recursive(p: &Path) -> std::io::Result<()> {
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

fn dest_for(src: &Path, dst: &Path) -> std::path::PathBuf {
    if dst.is_dir() {
        dst.join(src.file_name().unwrap_or_default())
    } else {
        dst.to_path_buf()
    }
}

fn free_dst(dst: &Path) -> std::path::PathBuf {
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

fn run(op: &str, req: &Value) -> Result<Value, String> {
    let s = |k: &str| {
        req.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    match op {
        "ls" => list_entries(Path::new(&s("p"))),
        "move" | "copy" => {
            let src_s = s("src");
            let dst_s = s("dst");
            let strategy = s("strategy");
            let src = Path::new(&src_s);
            let mut dst = dest_for(src, Path::new(&dst_s));
            if dst.exists() {
                if strategy == "rename" {
                    dst = free_dst(&dst);
                } else if strategy == "merge" || strategy == "overwrite" {
                    // merge into the existing destination
                } else {
                    return Ok(json!({
                        "conflict": true,
                        "dst": dst.to_string_lossy(),
                        "error": format!("destination exists: {}", dst.display()),
                    }));
                }
            }
            if op == "move" {
                if let Err(e) = std::fs::rename(src, &dst) {
                    if e.kind() == ErrorKind::CrossesDevices {
                        copy_recursive(src, &dst).map_err(|e| e.to_string())?;
                        remove_recursive(src).map_err(|e| e.to_string())?;
                    } else {
                        return Err(e.to_string());
                    }
                }
            } else {
                copy_recursive(src, &dst).map_err(|e| e.to_string())?;
            }
            Ok(json!({ "dst": dst.to_string_lossy() }))
        }
        "rename" => {
            let src_s = s("src");
            let name = s("name");
            let src = Path::new(&src_s);
            let parent = src.parent().ok_or_else(|| "no parent".to_string())?;
            let dst = parent.join(&name);
            if dst.exists() {
                return Err(format!("destination exists: {}", dst.display()));
            }
            std::fs::rename(src, &dst).map_err(|e| e.to_string())?;
            Ok(json!({ "dst": dst.to_string_lossy() }))
        }
        "mkdir" => {
            std::fs::create_dir_all(Path::new(&s("dst"))).map_err(|e| e.to_string())?;
            Ok(json!({ "dst": s("dst") }))
        }
        "delete" => {
            let src_s = s("src");
            let p = Path::new(&src_s);
            let md = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
            if md.is_dir() {
                std::fs::remove_dir_all(p).map_err(|e| e.to_string())?;
            } else {
                std::fs::remove_file(p).map_err(|e| e.to_string())?;
            }
            Ok(json!({ "ok": true }))
        }
        "put" => {
            let dst_s = s("dst");
            let tmp_s = s("tmp");
            let dst = Path::new(&dst_s);
            let tmp = Path::new(&tmp_s);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            copy_file(tmp, dst).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_file(tmp);
            Ok(json!({ "dst": dst.to_string_lossy() }))
        }
        _ => Err("unknown op".into()),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: file-mover-sudo <json>");
        exit(1);
    }
    let req: Value = match serde_json::from_str(&args[1]) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", json!({ "error": e.to_string() }));
            exit(1);
        }
    };
    let op = req.get("op").and_then(|x| x.as_str()).unwrap_or("");
    let res = run(op, &req).unwrap_or_else(|e| json!({ "error": e }));
    println!("{res}");
}
