//! D-1 trace tool: poll X11 XDamage on a target display, partition
//! into tiles via [`TileGrid`], emit per-tick stats. Run on the X11
//! peer to verify the damage backend produces non-trivial output
//! during an `xdotool` sweep.
//!
//! Usage:
//!
//! ```sh
//! # On the X11 peer:
//! cargo run --release --example damage-trace
//! cargo run --release --example damage-trace -- --display :0 --tile-size 64 --interval-ms 33
//! ```
//!
//! Strict non-goals (per D-1 scope): no datachannels, no encoder, no
//! browser, no integration with the existing capture pipeline. This
//! binary opens its own X11 connection to observe damage events
//! independently of the production capture path.
//!
//! No behavior change to the existing VP8-q display path: this is a
//! standalone `cargo example` target that's only built on demand.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("damage-trace is X11/Linux only — D-1 scope. Other platforms get None backend.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    // The example reaches into private modules of the binary crate
    // via the published `intendant` artifact's source tree. The
    // simplest portable path is to declare the modules inline here
    // mirroring the bin/caller layout, but that's brittle. Instead
    // we use the same trick as the existing tests: include the
    // source files directly through #[path] and skip CallerError so
    // the example is self-contained.
    //
    // Concretely, this binary doesn't depend on the rest of the
    // intendant codebase — only on the three D-1 modules. Inlining
    // them with #[path] means we don't need to expose them as a lib
    // crate for the example to consume.

    #[path = "../src/bin/caller/display/capture/damage.rs"]
    mod damage;
    #[path = "../src/bin/caller/display/capture/x11_damage.rs"]
    mod x11_damage;
    #[path = "../src/bin/caller/display/tile/grid.rs"]
    mod grid;
    #[path = "../src/bin/caller/display/tile/synthetic_dirty.rs"]
    mod synthetic_dirty;

    use damage::{DamageBackend, DamageCapability, DamageError, NullDamageBackend};
    use grid::TileGrid;
    use std::time::{Duration, Instant};

    let mut display: String =
        std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let mut tile_size_px: u16 = 64;
    let mut interval_ms: u64 = 33;
    let mut duration_secs: u64 = 60;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--display" => {
                display = args.get(i + 1).cloned().unwrap_or(display);
                i += 2;
            }
            "--tile-size" => {
                tile_size_px = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(tile_size_px);
                i += 2;
            }
            "--interval-ms" => {
                interval_ms = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(interval_ms);
                i += 2;
            }
            "--duration" => {
                duration_secs = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(duration_secs);
                i += 2;
            }
            "-h" | "--help" => {
                eprintln!("usage: damage-trace [--display :N] [--tile-size N] [--interval-ms N] [--duration SECS]");
                eprintln!("defaults: DISPLAY={} tile_size=64 interval_ms=33 duration=60", display);
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    eprintln!(
        "damage-trace: display={} tile_size={} interval_ms={} duration={}s",
        display, tile_size_px, interval_ms, duration_secs
    );

    // Try the real X11 backend; fall back to Null on ExtensionMissing
    // (the explicit-degradation path the user wants).
    let mut backend: Box<dyn DamageBackend> =
        match x11_damage::X11DamageBackend::new(&display) {
            Ok(b) => {
                let (w, h) = b.screen_geometry();
                eprintln!(
                    "  backend: X11DamageBackend  capability={:?}  geometry={}x{}",
                    b.capability(),
                    w,
                    h
                );
                Box::new(b)
            }
            Err(DamageError::ExtensionMissing(ext)) => {
                eprintln!(
                    "  backend: NullDamageBackend (X11 extension '{ext}' missing — explicit degradation)"
                );
                // We don't know the screen size if we can't connect
                // properly; emit a sensible default for the trace.
                Box::new(NullDamageBackend::new(1920, 1080))
            }
            Err(e) => {
                eprintln!("  fatal: cannot construct any backend: {e}");
                std::process::exit(3);
            }
        };

    let (sw, sh) = backend.screen_geometry();
    let grid = TileGrid::new(sw, sh, tile_size_px).expect("valid grid params");
    let total_tiles = grid.total_tiles();
    eprintln!(
        "  grid: {}x{} tiles ({} total) @ {}px",
        grid.width_tiles, grid.height_tiles, total_tiles, tile_size_px
    );
    eprintln!();
    eprintln!(
        "ts_ms\tcapability\tdirty_rects\tdirty_tiles\tdirty_fraction"
    );

    let start = Instant::now();
    let interval = Duration::from_millis(interval_ms);
    let deadline = start + Duration::from_secs(duration_secs);

    while Instant::now() < deadline {
        let tick_start = Instant::now();

        match backend.poll_damage() {
            Ok(rects) => {
                let tiles = grid.dirty_tiles(&rects);
                let fraction = grid.dirty_fraction(tiles.len());
                println!(
                    "{}\t{:?}\t{}\t{}\t{:.4}",
                    tick_start.duration_since(start).as_millis(),
                    backend.capability(),
                    rects.len(),
                    tiles.len(),
                    fraction,
                );
            }
            Err(e) => {
                eprintln!("poll error: {e}");
                // Transient errors don't kill the loop; the next
                // tick may succeed. Fatal errors usually surface
                // as construction failures.
            }
        }

        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    eprintln!("damage-trace: done after {:?}", start.elapsed());
}
