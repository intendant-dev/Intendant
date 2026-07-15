//! Build script for the intendant binary.
//!
//! Two generated-into-`static/` jobs, both feeding `include_str!`/
//! `include_bytes!` embeds in the gateway:
//!
//! - Assembles `static/app.html` from the ordered fragments in `static/app/`
//!   (via `crates/app-html-assembler` — the same crate CI runs as the regen
//!   gate). Write-if-different, so unchanged fragments never dirty the
//!   artifact's mtime.
//! - Checks whether the compiled WASM artifacts of each browser WASM crate
//!   (`crates/presence-web` → `static/wasm-web/`, `crates/station-web` →
//!   `static/wasm-station/`) are older than their Rust sources. If stale,
//!   auto-rebuilds via `wasm-pack build` using a separate target directory to
//!   avoid deadlocking with the parent cargo process.

use std::path::Path;
use std::process::Command;

/// The wasm-pack version every committed artifact must be built with —
/// single-sourced from `.wasm-pack-version` (the setup scripts install
/// from the same file). Different wasm-pack releases emit byte-different
/// output, and the artifacts are committed: a cross-version rebuild
/// churns them and conflicts every concurrent landing that also rebuilt.
/// To upgrade, bump the file and regenerate BOTH crates' artifacts in
/// the same commit.
const PINNED_WASM_PACK_VERSION: &str = include_str!(".wasm-pack-version");

/// `wasm-pack --version` → the bare version string, or None when the
/// binary is missing/unrunnable.
fn installed_wasm_pack_version() -> Option<String> {
    let out = Command::new("wasm-pack").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    // "wasm-pack 0.14.0"
    String::from_utf8(out.stdout)
        .ok()?
        .split_whitespace()
        .nth(1)
        .map(str::to_string)
}

/// A browser WASM crate whose wasm-pack artifacts are embedded into the
/// gateway binary via `include_str!`/`include_bytes!`.
struct WasmCrate {
    /// Crate directory, relative to the repo root.
    crate_dir: &'static str,
    /// wasm-pack output directory, relative to the repo root.
    artifact_dir: &'static str,
    /// `--out-name` passed to wasm-pack (artifact file stem).
    out_name: &'static str,
    /// Additional source directories that feed this crate (path deps).
    extra_src_dirs: &'static [&'static str],
}

const WASM_CRATES: &[WasmCrate] = &[
    WasmCrate {
        crate_dir: "crates/presence-web",
        artifact_dir: "static/wasm-web",
        out_name: "presence_web",
        extra_src_dirs: &["crates/presence-core/src"],
    },
    WasmCrate {
        crate_dir: "crates/station-web",
        artifact_dir: "static/wasm-station",
        out_name: "station_web",
        extra_src_dirs: &[],
    },
];

impl WasmCrate {
    fn src_dir(&self) -> String {
        format!("{}/src", self.crate_dir)
    }

    fn wasm_bin(&self) -> String {
        format!("{}/{}_bg.wasm", self.artifact_dir, self.out_name)
    }

    fn js_glue(&self) -> String {
        format!("{}/{}.js", self.artifact_dir, self.out_name)
    }

    /// The manual fallback command printed when the auto-rebuild fails.
    fn manual_build_command(&self) -> String {
        format!(
            "cd {} && wasm-pack build --target web --out-dir ../../{} --out-name {}",
            self.crate_dir, self.artifact_dir, self.out_name
        )
    }

    /// Re-run the build script if the crate's sources or compiled artifacts
    /// change.
    fn emit_rerun_directives(&self) {
        println!("cargo:rerun-if-changed={}/", self.src_dir());
        for dir in self.extra_src_dirs {
            println!("cargo:rerun-if-changed={}/", dir);
        }
        println!("cargo:rerun-if-changed={}", self.wasm_bin());
        println!("cargo:rerun-if-changed={}", self.js_glue());
    }

    /// Rebuild the WASM artifacts via wasm-pack when any source file is newer
    /// than the compiled `.wasm`.
    fn rebuild_if_stale(&self) {
        let wasm_bin = self.wasm_bin();
        let wasm_bin = Path::new(&wasm_bin);
        let src_dir = self.src_dir();
        let src_dir = Path::new(&src_dir);

        if !wasm_bin.exists() || !src_dir.exists() {
            return;
        }

        let wasm_modified = wasm_bin.metadata().and_then(|m| m.modified()).ok();

        let src_modified = std::iter::once(src_dir.to_path_buf())
            .chain(self.extra_src_dirs.iter().map(std::path::PathBuf::from))
            .filter_map(|d| newest_in_dir(&d))
            .max();

        let stale = match (wasm_modified, src_modified) {
            (Some(w), Some(s)) => s > w,
            _ => false,
        };

        if !stale {
            return;
        }

        // Version gate: only the pinned wasm-pack may regenerate the
        // committed artifacts (other releases emit byte-different output
        // — see PINNED_WASM_PACK_VERSION). A mismatched or missing
        // wasm-pack keeps the committed artifacts instead of churning
        // them; the daemon still builds, just with stale WASM, and the
        // warning names the one command that fixes it.
        let pinned = PINNED_WASM_PACK_VERSION.trim();
        match installed_wasm_pack_version() {
            Some(v) if v == pinned => {}
            got => {
                println!(
                    "cargo:warning={} WASM is stale but wasm-pack {} doesn't match the pin {} — SKIPPING the rebuild so the committed artifacts don't churn. Fix: cargo install wasm-pack --version {} --locked",
                    self.crate_dir,
                    got.as_deref().unwrap_or("(not installed)"),
                    pinned,
                    pinned
                );
                return;
            }
        }

        println!(
            "cargo:warning={} WASM is stale — auto-rebuilding via wasm-pack...",
            self.crate_dir
        );

        // Use a separate target directory to avoid deadlocking with the parent
        // cargo process. The parent holds a lock on `target/`, so wasm-pack's
        // internal `cargo build --target wasm32` must write elsewhere. Create
        // it up front and pass an absolute path: a relative CARGO_TARGET_DIR
        // would resolve against the wasm crate dir, not the repo root.
        let wasm_target = Path::new("target/wasm-build");
        if let Err(err) = std::fs::create_dir_all(wasm_target) {
            println!(
                "cargo:warning=failed to create WASM target dir {}: {}",
                wasm_target.display(),
                err
            );
        }
        let wasm_target_abs = std::fs::canonicalize(wasm_target).unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|d| d.join(wasm_target))
                .unwrap_or_else(|_| wasm_target.to_path_buf())
        });

        let result = Command::new("wasm-pack")
            .args([
                "build",
                "--target",
                "web",
                "--out-dir",
                &format!("../../{}", self.artifact_dir),
                "--out-name",
                self.out_name,
            ])
            .current_dir(self.crate_dir)
            // Cargo exports the host build's resolved rustflags to build
            // scripts via CARGO_ENCODED_RUSTFLAGS. The nested cargo inside
            // wasm-pack would apply them to the wasm32 target (env rustflags
            // beat config), so host-only link args like the macOS
            // `-Wl,-rpath,/usr/lib/swift` from .cargo/config.toml break
            // rust-lld. Scrub them so the inner build resolves flags fresh.
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            // Then set exactly the canonical artifact flags — keep in
            // LOCKSTEP with scripts/build-wasm.sh (the CI drift gate
            // rebuilds through that script and byte-diffs the result, so
            // any divergence here fails the gate rather than shipping):
            // dependency panic-locations embed the building account's
            // cargo registry path; remapping it is what makes artifact
            // bytes account-independent.
            .env("RUSTFLAGS", {
                let cargo_home = std::env::var_os("CARGO_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .or_else(|| std::env::var_os("USERPROFILE"))
                            .map(|h| std::path::PathBuf::from(h).join(".cargo"))
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from(".cargo"));
                format!(
                    "--remap-path-prefix {}=/cargo/registry/src",
                    cargo_home.join("registry").join("src").display()
                )
            })
            .env("CARGO_TARGET_DIR", &wasm_target_abs)
            .status();

        match result {
            Ok(status) if status.success() => {
                println!("cargo:warning={} WASM rebuilt successfully", self.crate_dir);
            }
            Ok(status) => {
                println!(
                    "cargo:warning=wasm-pack failed (exit {}) for {}. Run manually: {}",
                    status,
                    self.crate_dir,
                    self.manual_build_command()
                );
            }
            Err(_) => {
                println!(
                    "cargo:warning=wasm-pack not found; {} WASM stays stale. Install: cargo install wasm-pack, or run manually: {}",
                    self.crate_dir,
                    self.manual_build_command()
                );
            }
        }
    }

    /// Write a content hash of the WASM artifacts to OUT_DIR. Cargo always
    /// tracks OUT_DIR for changes, so when the WASM is rebuilt the hash file
    /// changes and cargo recompiles the crate (re-running `include_bytes!`).
    /// `rerun-if-changed` on binary files can be flaky across worktrees;
    /// writing a derived file to OUT_DIR is bulletproof because cargo always
    /// checks OUT_DIR contents.
    fn write_artifact_hash(&self) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::io::Read;

        let out_dir = match std::env::var("OUT_DIR") {
            Ok(d) => d,
            Err(_) => return,
        };

        let mut hasher = DefaultHasher::new();
        for path in [self.wasm_bin(), self.js_glue()] {
            if let Ok(mut f) = std::fs::File::open(&path) {
                let mut buf = Vec::new();
                match f.read_to_end(&mut buf) {
                    Ok(_) => buf.hash(&mut hasher),
                    Err(err) => println!(
                        "cargo:warning=failed to read WASM artifact {} for hashing: {}",
                        path, err
                    ),
                }
            }
        }
        let hash = format!("{:016x}", hasher.finish());

        let hash_path = Path::new(&out_dir).join(format!("{}_hash.txt", self.out_name));
        // Only write if changed, to avoid unnecessary rebuilds
        let existing = std::fs::read_to_string(&hash_path).unwrap_or_default();
        if existing.trim() != hash {
            if let Err(err) = std::fs::write(&hash_path, &hash) {
                println!(
                    "cargo:warning=failed to write WASM artifact hash {}: {}",
                    hash_path.display(),
                    err
                );
            }
        }
    }
}

fn main() {
    // Assemble static/app.html from the static/app/ fragments (see
    // crates/app-html-assembler) before anything compiles, so the
    // `include_str!` embed in web_gateway.rs always matches the fragment
    // sources. Watching the artifact itself means a stray hand-edit to the
    // generated file is reverted to fragment truth on the next build rather
    // than silently shipping. Fail loudly on manifest ↔ directory mismatch:
    // a silently dropped fragment would embed a broken dashboard.
    println!(
        "cargo:rerun-if-changed={}/",
        app_html_assembler::FRAGMENT_DIR
    );
    println!("cargo:rerun-if-changed={}", app_html_assembler::OUTPUT);
    // The vault crypto kernel's sha256 is pinned into the assembled
    // app.html (VAULT_KERNEL_SHA256), so a kernel edit must re-assemble.
    println!(
        "cargo:rerun-if-changed={}",
        app_html_assembler::VAULT_KERNEL_PATH
    );
    // The wasm-pack pin gates artifact rebuilds; a pin bump must re-run
    // the staleness/version checks.
    println!("cargo:rerun-if-changed=.wasm-pack-version");
    if let Err(err) = app_html_assembler::assemble(Path::new(".")) {
        panic!("app.html assembly failed: {err}");
    }

    // Re-run if any WASM crate source or artifact changes.
    for krate in WASM_CRATES {
        krate.emit_rerun_directives();
    }

    // Expose the current git commit SHA as an env var so `/config` and
    // `intendant --version` can report it. The multi-host dashboard compares
    // the primary's SHA against each secondary's SHA and warns on mismatch —
    // same class of version-skew confusion we just hit when the mac guest
    // was running stale code without CORS headers.
    //
    // rerun-if-changed on HEAD + the branch ref file covers the common
    // "committed but didn't recompile" path. If the git command fails
    // (no .git, binary missing, detached head in weird state) the value
    // falls back to "unknown".
    emit_git_rerun_paths();
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(o.stdout)
            } else {
                None
            }
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    // Append `-dirty` when the working tree has uncommitted changes, so
    // the multi-host skew detector catches "I rebuilt but didn't commit"
    // cases. Without this, a dev rebuilding locally on top of HEAD
    // would report the same SHA as a sibling daemon still on that
    // commit, and the yellow warning wouldn't fire.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let sha_with_dirty = if dirty {
        format!("{git_sha}-dirty")
    } else {
        git_sha
    };
    println!("cargo:rustc-env=INTENDANT_GIT_SHA={sha_with_dirty}");

    // Build provenance for `--version`: timestamp + target triple. The
    // timestamp is cached in OUT_DIR keyed by the SHA (see the helper), so
    // its rustc-env value only changes when INTENDANT_GIT_SHA changes —
    // a fresh wall-clock stamp on every build-script re-run would force a
    // full crate recompile even when nothing else changed (defeating the
    // write-if-different app.html/WASM-hash machinery above).
    println!(
        "cargo:rustc-env=INTENDANT_BUILD_TIMESTAMP={}",
        provenance_build_timestamp(&sha_with_dirty)
    );
    // TARGET is set by cargo for every build-script invocation; per-target
    // OUT_DIRs make it stable within a build directory.
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=INTENDANT_TARGET_TRIPLE={target}");
    }

    // Rebuild stale WASM first, then hash the (possibly fresh) artifacts so
    // OUT_DIR reflects what `include_bytes!` will embed in this build.
    for krate in WASM_CRATES {
        krate.rebuild_if_stale();
        krate.write_artifact_hash();
    }
}

/// Emit `rerun-if-changed` directives for the git files whose changes can
/// move `INTENDANT_GIT_SHA`: HEAD, the checked-out branch's loose ref file,
/// and packed-refs (a `git pack-refs`/gc can retire the loose file).
///
/// Worktree-aware: in a linked worktree `.git` is a FILE pointing at the
/// per-worktree gitdir, and refs live in the shared common dir — so the
/// paths are resolved through `git rev-parse` instead of assuming `.git/`
/// is a directory. Only paths that actually exist are emitted: cargo treats
/// a missing rerun-if-changed path as always-changed, which would re-run
/// this script on every build (the pre-worktree-fix behavior).
fn emit_git_rerun_paths() {
    let Some(git_dir) = git_rev_parse_path("--git-dir") else {
        // No git (tarball build, git binary missing): emit nothing rather
        // than a dangling path that forces a re-run every build.
        return;
    };
    // Refs are shared between worktrees; HEAD and index are per-worktree.
    let common_dir = git_rev_parse_path("--git-common-dir").unwrap_or_else(|| git_dir.clone());

    let head = git_dir.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(ref_path) = contents.strip_prefix("ref: ").map(str::trim) {
            let loose_ref = common_dir.join(ref_path);
            if loose_ref.exists() {
                println!("cargo:rerun-if-changed={}", loose_ref.display());
            }
        }
    }
    let packed_refs = common_dir.join("packed-refs");
    if packed_refs.exists() {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }
}

/// `git rev-parse --path-format=absolute <flag>` → an absolute PathBuf, or
/// None when git is unavailable or the flag fails. `--path-format=absolute`
/// (git ≥ 2.31) avoids joining relative answers against the cwd ourselves;
/// older gits fall back to manual resolution.
fn git_rev_parse_path(flag: &str) -> Option<std::path::PathBuf> {
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("git").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let raw = run(&["rev-parse", "--path-format=absolute", flag])
        .or_else(|| run(&["rev-parse", flag]))?;
    let path = std::path::PathBuf::from(raw);
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

/// The wall-clock timestamp (RFC 3339 UTC, second precision) of the first
/// build of the current `<sha>[-dirty]` provenance in this target dir,
/// cached in OUT_DIR so the emitted rustc-env value changes exactly when
/// the SHA value does (keeping rebuild-triggering minimal — see the call
/// site). Falls back to a fresh uncached stamp if OUT_DIR is unavailable.
fn provenance_build_timestamp(sha_with_dirty: &str) -> String {
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| rfc3339_utc(d.as_secs()))
            .unwrap_or_else(|_| "unknown".to_string())
    };
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return now();
    };
    let stamp_path = Path::new(&out_dir).join("provenance_stamp.txt");
    if let Ok(existing) = std::fs::read_to_string(&stamp_path) {
        let mut lines = existing.lines();
        if let (Some(sha), Some(ts)) = (lines.next(), lines.next()) {
            if sha == sha_with_dirty && !ts.trim().is_empty() {
                return ts.trim().to_string();
            }
        }
    }
    let ts = now();
    if let Err(err) = std::fs::write(&stamp_path, format!("{sha_with_dirty}\n{ts}\n")) {
        println!(
            "cargo:warning=failed to cache build timestamp {}: {}",
            stamp_path.display(),
            err
        );
    }
    ts
}

/// Format seconds since the Unix epoch as RFC 3339 UTC
/// (`YYYY-MM-DDTHH:MM:SSZ`) without a date/time dependency, using Howard
/// Hinnant's `civil_from_days` algorithm (exact for the proleptic Gregorian
/// calendar; build scripts stay std-only per the build-dependencies note in
/// Cargo.toml).
fn rfc3339_utc(secs_since_epoch: u64) -> String {
    let days = (secs_since_epoch / 86_400) as i64;
    let secs_of_day = secs_since_epoch % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs_of_day / 3_600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60
    )
}

/// Find the newest modification time among all files in a directory (recursive).
fn newest_in_dir(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(t) = newest_in_dir(&path) {
                    newest = Some(newest.map_or(t, |n: std::time::SystemTime| n.max(t)));
                }
            } else if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    newest =
                        Some(newest.map_or(modified, |n: std::time::SystemTime| n.max(modified)));
                }
            }
        }
    }
    newest
}
