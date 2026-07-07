//! Shared network vocabulary: local interface probes (unix walks
//! `getifaddrs(3)` via libc; Windows goes through the `if-addrs`
//! crate), the daemon's canonical gateway port, and the TCP-listener
//! accept-failure policy + rebind helper shared by the web gateway,
//! the control socket, and the enrollment cert server. No dependency
//! on any daemon subsystem.

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
///
/// Implementation walks `getifaddrs(3)` directly via libc — same crate
/// the codebase already depends on for other unix interop.
pub fn routable_local_addrs(include_loopback: bool) -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr};

    let mut out: Vec<IpAddr> = Vec::new();
    if include_loopback {
        out.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe {
            let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
            if libc::getifaddrs(&mut ifap) == 0 && !ifap.is_null() {
                let mut cur = ifap;
                while !cur.is_null() {
                    let ifa = &*cur;
                    if !ifa.ifa_addr.is_null() {
                        let family = (*ifa.ifa_addr).sa_family as i32;
                        let _name = if ifa.ifa_name.is_null() {
                            String::new()
                        } else {
                            CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned()
                        };
                        if family == libc::AF_INET {
                            let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                            let octets = (*sin).sin_addr.s_addr.to_ne_bytes();
                            let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]);
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                out.push(IpAddr::V4(ip));
                            }
                        } else if family == libc::AF_INET6 {
                            let sin6 = ifa.ifa_addr as *const libc::sockaddr_in6;
                            let segs = (*sin6).sin6_addr.s6_addr;
                            let ip = std::net::Ipv6Addr::from(segs);
                            if !ip.is_loopback() && !ip.is_unspecified() && !is_link_local_v6(&ip) {
                                out.push(IpAddr::V6(ip));
                            }
                        }
                    }
                    cur = (*cur).ifa_next;
                }
                libc::freeifaddrs(ifap);
            }
        }
    }

    // Windows has no `getifaddrs(3)`; the OS API is `GetAdaptersAddresses`.
    // Rather than hand-roll that FFI walk we use the `if-addrs` crate, which
    // wraps it and yields the same per-interface address list. The filtering
    // mirrors the unix arm: drop loopback (unless requested), link-local
    // (IPv6 fe80::/10 and IPv4 169.254/16 — neither is a useful advertised
    // endpoint), and unspecified addresses. Enumeration order is preserved so
    // the caller's later stable sort keeps a multi-NIC host's primary NIC
    // first, matching the unix behaviour.
    #[cfg(windows)]
    {
        if let Ok(ifaces) = if_addrs::get_if_addrs() {
            for iface in ifaces {
                if iface.is_link_local() {
                    continue;
                }
                let ip = iface.ip();
                if ip.is_unspecified() {
                    continue;
                }
                if ip.is_loopback() {
                    // Loopback is added once up-front (as 127.0.0.1) when
                    // requested; skip the per-interface loopback entries so
                    // we don't emit duplicates or ::1 alongside it.
                    continue;
                }
                out.push(ip);
            }
        }
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
pub fn rebind_dead_tcp_listener(
    addr: std::net::SocketAddr,
) -> std::io::Result<TcpListener> {
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
    use tokio::net::TcpListener;

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
    async fn rebind_with_patience(
        addr: std::net::SocketAddr,
    ) -> std::io::Result<TcpListener> {
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

        let (client, (server, _peer)) = tokio::join!(
            tokio::net::TcpStream::connect(addr),
            async { rebound.accept().await.unwrap() },
        );
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
