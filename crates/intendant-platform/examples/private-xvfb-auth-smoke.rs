//! Manual live acceptance smoke for Intendant's private Xvfb boundary.
//!
//! This is deliberately an example rather than a Rust test: it inspects the
//! host's real X11 namespace, launches installed binaries, and must never run
//! as part of the repository's hermetic unit suite.

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    use intendant_platform::{vision, DisplayTarget};
    use std::process::Stdio;

    let mut excluded = Vec::new();
    let config = loop {
        let config =
            vision::virtual_display_config(640, 480, &excluded).expect("no free managed X display");
        let DisplayTarget::Virtual { id } = config.target else {
            panic!("virtual display allocator returned the user session");
        };
        let tcp = std::net::SocketAddr::from(([127, 0, 0, 1], 6000 + id as u16));
        if std::net::TcpStream::connect_timeout(&tcp, std::time::Duration::from_millis(50)).is_err()
        {
            break config;
        }
        excluded.push(id);
    };
    let DisplayTarget::Virtual { id } = config.target else {
        unreachable!();
    };
    let guard = vision::launch_private_display(&config)
        .await
        .expect("launch private Xvfb");
    let authorization =
        vision::virtual_display_x11_authorization(id).expect("live X11 authorization");
    let display = format!(":{id}");

    let unauthorized = tokio::process::Command::new("xdpyinfo")
        .args(["-display", &display])
        .env_remove("XAUTHORITY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("run unauthenticated xdpyinfo");
    assert!(
        !unauthorized.success(),
        "unauthenticated X11 access succeeded"
    );

    let authorized = tokio::process::Command::new("xdpyinfo")
        .args(["-display", &display])
        .env("XAUTHORITY", authorization.xauthority_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .expect("run authenticated xdpyinfo");
    assert!(authorized.success(), "authenticated X11 access failed");

    let tcp = std::net::SocketAddr::from(([127, 0, 0, 1], 6000 + id as u16));
    assert!(
        std::net::TcpStream::connect_timeout(&tcp, std::time::Duration::from_millis(100)).is_err(),
        "private Xvfb unexpectedly exposed TCP port {}",
        tcp.port()
    );

    let credential_path = authorization.xauthority_path().to_path_buf();
    guard.shutdown().await;
    assert!(vision::virtual_display_x11_authorization(id).is_none());
    assert!(!credential_path.exists(), "Xauthority survived teardown");
    println!("private Xvfb authorization smoke passed on {display}");
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("private Xvfb authorization smoke is Linux-only");
    std::process::exit(2);
}
