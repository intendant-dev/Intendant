//! Shared network vocabulary: safe cross-platform local interface probes,
//! the daemon's canonical gateway port, and the TCP-listener accept-failure
//! policy + rebind helper shared by the web gateway, the control socket, and
//! the enrollment cert server. No dependency on any daemon subsystem.

use tokio::net::TcpListener;

/// Default TCP port for the daemon's web gateway (dashboard + API).
/// Canonical here so access/ and peer/ can name it without reaching
/// upward into the gateway module.
pub const DEFAULT_GATEWAY_PORT: u16 = 8765;

/// Enumerate the local machine's routable IP addresses (one entry per
/// interface address that's globally usable). Used by:
///
/// - The federation advertise side (`resolve_advertise_urls` in the caller)
///   to auto-populate the Agent Card with one URL per interface — the
///   ICE host-candidate-gathering pattern, applied to peer discovery.
/// - The WebRTC display path (`WebRtcPeer::new`) to bind one UDP socket
///   per interface and emit a matching host candidate. WebRTC needs
///   loopback so a browser running on the same machine can pair
///   against it; federation doesn't (advertising loopback to remote
///   peers is useless), hence the `include_loopback` parameter.
///
/// Excludes IPv6 link-local (fe80::/10), IPv4 loopback when
/// `!include_loopback`, and unspecified addresses (0.0.0.0 / ::) which
/// aren't real bind targets.
pub fn routable_local_addrs(include_loopback: bool) -> Vec<std::net::IpAddr> {
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    filtered_interface_addrs(
        include_loopback,
        interfaces
            .into_iter()
            .map(|interface| (interface.ip(), interface.is_link_local())),
    )
}

/// Apply the platform's historical interface-address filters while retaining
/// the OS enumeration order (and any duplicate entries). Callers rely on that
/// order when they perform a stable IPv4-before-IPv6 sort.
fn filtered_interface_addrs(
    include_loopback: bool,
    interfaces: impl IntoIterator<Item = (std::net::IpAddr, bool)>,
) -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr};

    let mut out = Vec::new();
    if include_loopback {
        out.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    for (ip, interface_is_link_local) in interfaces {
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }

        // Preserve the existing platform behavior: Unix historically filtered
        // IPv6 fe80::/10 but retained IPv4 169.254/16, while the Windows
        // if-addrs path filtered every address the crate marks link-local.
        #[cfg(unix)]
        if matches!(ip, IpAddr::V6(ip) if is_link_local_v6(&ip)) {
            continue;
        }
        #[cfg(windows)]
        if interface_is_link_local {
            continue;
        }
        #[cfg(unix)]
        let _ = interface_is_link_local;

        out.push(ip);
    }

    out
}

/// `true` for IPv6 link-local addresses (fe80::/10). Link-local is
/// scoped to one link and isn't useful as an advertised endpoint.
pub fn is_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    let segs = ip.segments();
    (segs[0] & 0xffc0) == 0xfe80
}

/// Consecutive "fatal-class" accept failures tolerated on the same socket
/// before it is dropped and rebound. EINVAL has been observed twice on
/// macOS (2026-07-04, both times within ~1s of an external-agent spawn)
/// on a listener that remained LISTEN at the kernel afterwards — treating
/// the first one as fatal is what actually broke the dashboard. A short
/// streak (~2s) absorbs the spurious case; a genuinely dead socket fails
/// every retry and reaches the rebind path.
pub const FATAL_ACCEPT_REBIND_THRESHOLD: u32 = 8;

pub fn should_continue_after_accept_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::WouldBlock
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => return true,
        std::io::ErrorKind::InvalidInput
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::PermissionDenied => return false,
        _ => {}
    }

    match error.raw_os_error() {
        // The listener file descriptor/socket is invalid or no longer a
        // listening socket (EBADF/EINVAL/ENOTSOCK). Retrying accept() on it
        // would spin forever — the caller rebinds a fresh listener instead.
        Some(9 | 22 | 38) => false,
        // Process/system descriptor pressure and socket buffer pressure are
        // recoverable after current connections close. Keep the gateway alive
        // so the dashboard recovers instead of becoming half-alive.
        Some(23 | 24 | 55) => true,
        // Unknown accept errors are safer to treat as per-connection failures:
        // losing one inbound connection is better than dropping the dashboard
        // listener while existing WebSocket tasks make the UI look alive.
        _ => true,
    }
}

/// Rebind a TCP listener on its original address after the previous
/// socket became unusable — seen in the wild on macOS as `accept()`
/// returning EINVAL a minute into an app-spawned daemon's life, which
/// used to kill the listener task and leave the dashboard half-alive
/// (established WebSockets kept flowing while every new connection —
/// session details, files, uploads, Station assets — failed). Mirrors
/// `bind_dual_stack_or_v4`: dual-stack for the IPv6 wildcard,
/// `SO_REUSEADDR` so lingering TIME_WAIT sockets don't block the port.
/// Shared by the dashboard gateway and the enrollment cert server.
pub fn rebind_dead_tcp_listener(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() && addr.ip().is_unspecified() {
        let _ = socket.set_only_v6(false);
    }
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(std_listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use tokio::net::TcpListener;

    #[test]
    fn interface_filter_preserves_encounter_order_and_duplicates() {
        let first = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let second = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2));
        let interfaces = [
            (IpAddr::V4(Ipv4Addr::UNSPECIFIED), false),
            (first, false),
            (IpAddr::V6(Ipv6Addr::LOCALHOST), false),
            (second, false),
            (first, false),
        ];

        assert_eq!(
            filtered_interface_addrs(true, interfaces),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST), first, second, first]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_filter_preserves_historical_link_local_behavior() {
        let ipv4_link_local = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2));
        let ipv6_link_local = IpAddr::V6(Ipv6Addr::new(0xfea0, 0, 0, 0, 0, 0, 0, 1));

        assert_eq!(
            filtered_interface_addrs(
                false,
                [
                    (ipv4_link_local, true),
                    // `if-addrs` currently marks only fe80::/16 link-local;
                    // our historical Unix filter covers the full fe80::/10.
                    (ipv6_link_local, false),
                ],
            ),
            vec![ipv4_link_local]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_filter_uses_if_addrs_link_local_classification() {
        let ipv4_link_local = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2));
        let regular = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert_eq!(
            filtered_interface_addrs(false, [(ipv4_link_local, true), (regular, false)]),
            vec![regular]
        );
    }

    #[test]
    fn accept_error_classifier_keeps_listener_alive_for_transient_errors() {
        assert!(should_continue_after_accept_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
        assert!(should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(24)
        ));
        assert!(!should_continue_after_accept_error(
            &std::io::Error::from_raw_os_error(9)
        ));
    }

    /// Rebind with a bounded retry on `AddrInUse` only. The drop→rebind
    /// window in these tests can lose the ephemeral port to a concurrent
    /// `bind(:0)` in a parallel test — the kernel recycles just-freed ports
    /// eagerly, and a loaded CI box makes the theft real (a merge-group
    /// ejection on 2026-07-07 was exactly this). The helper under test sets
    /// SO_REUSEADDR, so the only systematic `AddrInUse` source is a socket
    /// that is genuinely still bound — which keeps failing past the
    /// deadline and still fails the test.
    async fn rebind_with_patience(addr: std::net::SocketAddr) -> std::io::Result<TcpListener> {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            match rebind_dead_tcp_listener(addr) {
                Err(err)
                    if err.kind() == std::io::ErrorKind::AddrInUse
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
                other => return other,
            }
        }
    }

    /// The gateway must be able to re-establish its listener on the exact
    /// address a dead one occupied (accept() EINVAL/EBADF recovery path),
    /// and the fresh listener must actually accept connections.
    #[tokio::test]
    async fn rebind_dead_tcp_listener_restores_reachability() {
        let original = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = original.local_addr().unwrap();
        drop(original);

        let rebound = rebind_with_patience(addr)
            .await
            .expect("rebind on the freed address");
        assert_eq!(rebound.local_addr().unwrap(), addr);

        let (client, (server, _peer)) = tokio::join!(tokio::net::TcpStream::connect(addr), async {
            rebound.accept().await.unwrap()
        },);
        client.expect("client connects to rebound listener");
        drop(server);
    }

    /// SO_REUSEADDR does not override an actively bound listener on Unix —
    /// the accept-loop recovery MUST drop the dead socket before rebinding,
    /// or every attempt self-inflicts EADDRINUSE (seen live: a daemon whose
    /// accept loop died spun on rebind for over an hour while its own dead
    /// listener still owned the port). Windows semantics differ, so the
    /// still-bound assertion is Unix-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn rebind_fails_while_dead_listener_is_still_bound() {
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = holder.local_addr().unwrap();

        let err = rebind_dead_tcp_listener(addr)
            .expect_err("rebinding must fail while the previous listener still holds the address");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        drop(holder);
        assert!(rebind_with_patience(addr).await.is_ok());
    }
}
