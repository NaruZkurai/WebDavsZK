// Rebuild (and re-embed GIT_COMMIT) whenever the deploy/build scripts pass a
// new commit. `deploy-file-mover.sh` (and any caller) sets
// `GIT_COMMIT=$(git rev-parse --short HEAD)` so the binaries report exactly
// which commit they were built from via `option_env!("GIT_COMMIT")`.
fn main() {
    // Re-run rustc when the commit env var changes.
    println!("cargo:rerun-if-env-changed=GIT_COMMIT");
    // Also nudge on git HEAD changes so `cargo build` alone stays current
    // enough (the env var above is the real trigger in practice).
    println!("cargo:rerun-if-changed=.git/HEAD");
}
