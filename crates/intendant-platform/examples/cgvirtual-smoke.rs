//! MANUAL ONLY: hotplugs a monitor in the logged-in WindowServer session.
//! Never invoked by tests; does not start or contact an Intendant daemon.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args != ["--create-shared-session-monitor"] {
        eprintln!(
            "MANUAL ONLY: this changes the logged-in desktop's monitor layout.\n\
            It shares focus, cursor and clipboard; it is NOT a security sandbox.\n\
            Opt in with exactly: --create-shared-session-monitor"
        );
        std::process::exit(2);
    }
    run()
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use intendant_platform::cgvirtual::{Dimensions, Error, VirtualDisplays};
    use std::time::{Duration, Instant};

    let mut displays = VirtualDisplays::open()?;
    println!("Private class/selector ABI accepted; capture and input remain unwired.");
    let dimensions = Dimensions {
        width: 1024,
        height: 768,
    };
    let first = displays.create(dimensions)?;
    println!(
        "Created {:?}; inspect the monitor manually for three seconds.",
        displays.info(&first)?
    );
    std::thread::sleep(Duration::from_secs(3));
    let start = Instant::now();
    displays.destroy(&first)?;
    println!(
        "Explicit native removal observed after {:?}.",
        start.elapsed()
    );

    let second = displays.create(dimensions)?;
    assert_eq!(displays.destroy(&first), Err(Error::StaleHandle));
    println!(
        "Created replacement {:?}; stale generation refused.",
        displays.info(&second)?
    );
    drop(displays); // Exercise implicit RAII cleanup of the replacement.
                    // Reopening fails if Drop latched unconfirmed native teardown.
    let _after_drop = VirtualDisplays::open()?;
    println!("Explicit and RAII removal observed. No capture, input or TCC calls were made.");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    Err(intendant_platform::cgvirtual::Error::UnsupportedPlatform.into())
}
