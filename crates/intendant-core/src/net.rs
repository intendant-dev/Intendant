//! Shared network vocabulary: safe cross-platform local interface probes,
//! the daemon's canonical gateway port, and the TCP-listener accept-failure
//! policy + rebind helper shared by the web gateway, the control socket, and
//! the enrollment cert server. No dependency on any daemon subsystem.

use tokio::net::TcpListener;

/// Bidirectionally splice two byte streams under one shared inactivity window
/// and an independent byte ceiling for each direction.
///
/// Progress in either direction keeps the connection alive. Reads and each
/// partial write both count, so an asymmetric active stream is not closed by
/// a quiet reverse direction, while a backpressured write becomes idle once
/// neither half can make further progress. The first EOF, I/O error, or cap
/// completion closes the whole splice, matching the relay's fail-closed
/// connection lifetime.
pub async fn splice_bidirectional_bounded<A, B>(
    left: A,
    right: B,
    max_bytes_per_direction: u64,
    idle: std::time::Duration,
) where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    async fn copy_direction<R, W>(
        mut reader: R,
        mut writer: W,
        max_bytes: u64,
        activity: tokio::sync::mpsc::Sender<()>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let note_progress = || {
            // One queued notification is enough: consuming it resets the
            // watchdog no earlier than the progress it represents.
            let _ = activity.try_send(());
        };
        let mut buf = vec![0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            note_progress();
            total = total.saturating_add(n as u64);
            if total > max_bytes {
                return;
            }
            let mut written = 0usize;
            while written < n {
                match writer.write(&buf[written..n]).await {
                    Ok(0) | Err(_) => return,
                    Ok(count) => {
                        written += count;
                        note_progress();
                    }
                }
            }
        }
    }

    async fn wait_for_idle(
        mut activity: tokio::sync::mpsc::Receiver<()>,
        idle: std::time::Duration,
    ) {
        let timer = tokio::time::sleep(idle);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                biased;
                event = activity.recv() => match event {
                    Some(()) => timer.as_mut().reset(tokio::time::Instant::now() + idle),
                    None => return,
                },
                _ = &mut timer => return,
            }
        }
    }

    let (left_read, left_write) = tokio::io::split(left);
    let (right_read, right_write) = tokio::io::split(right);
    let (activity_tx, activity_rx) = tokio::sync::mpsc::channel(1);
    let left_to_right = copy_direction(
        left_read,
        right_write,
        max_bytes_per_direction,
        activity_tx.clone(),
    );
    let right_to_left =
        copy_direction(right_read, left_write, max_bytes_per_direction, activity_tx);
    let watchdog = wait_for_idle(activity_rx, idle);
    tokio::pin!(left_to_right, right_to_left, watchdog);
    tokio::select! {
        _ = &mut left_to_right => {}
        _ = &mut right_to_left => {}
        _ = &mut watchdog => {}
    }
}

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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[tokio::test(start_paused = true)]
    async fn bounded_splice_keeps_one_way_progress_alive() {
        let idle = std::time::Duration::from_secs(10);
        let (mut left_peer, left_splice) = tokio::io::duplex(64);
        let (right_splice, mut right_peer) = tokio::io::duplex(64);
        let splice = tokio::spawn(splice_bidirectional_bounded(
            left_splice,
            right_splice,
            1024,
            idle,
        ));

        for byte in [b'a', b'b', b'c'] {
            left_peer.write_all(&[byte]).await.unwrap();
            let mut received = [0u8; 1];
            right_peer.read_exact(&mut received).await.unwrap();
            assert_eq!(received, [byte]);
            tokio::time::advance(std::time::Duration::from_secs(9)).await;
            tokio::task::yield_now().await;
            assert!(
                !splice.is_finished(),
                "progress in one direction must refresh the shared idle window"
            );
        }
        splice.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_splice_ends_when_backpressure_stops_all_progress() {
        let idle = std::time::Duration::from_secs(10);
        let (mut left_peer, left_splice) = tokio::io::duplex(1);
        let (right_splice, _right_peer) = tokio::io::duplex(1);
        let splice = tokio::spawn(splice_bidirectional_bounded(
            left_splice,
            right_splice,
            1024,
            idle,
        ));

        left_peer.write_all(b"a").await.unwrap();
        tokio::task::yield_now().await;
        left_peer.write_all(b"b").await.unwrap();
        tokio::task::yield_now().await;
        assert!(!splice.is_finished());

        tokio::time::advance(idle + std::time::Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            splice.is_finished(),
            "a blocked write and quiet reverse direction must share one idle deadline"
        );
        splice.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_splice_ends_on_first_eof() {
        let (left_peer, left_splice) = tokio::io::duplex(16);
        let (right_splice, _right_peer) = tokio::io::duplex(16);
        let splice = tokio::spawn(splice_bidirectional_bounded(
            left_splice,
            right_splice,
            1024,
            std::time::Duration::from_secs(60),
        ));
        drop(left_peer);
        tokio::time::timeout(std::time::Duration::from_secs(1), splice)
            .await
            .expect("EOF must end the whole splice")
            .unwrap();
    }

    #[tokio::test]
    async fn bounded_splice_enforces_each_direction_byte_cap() {
        let (mut left_peer, left_splice) = tokio::io::duplex(16);
        let (right_splice, mut right_peer) = tokio::io::duplex(16);
        let splice = tokio::spawn(splice_bidirectional_bounded(
            left_splice,
            right_splice,
            2,
            std::time::Duration::from_secs(60),
        ));

        left_peer.write_all(b"ab").await.unwrap();
        let mut accepted = [0u8; 2];
        right_peer.read_exact(&mut accepted).await.unwrap();
        assert_eq!(&accepted, b"ab");
        left_peer.write_all(b"c").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), splice)
            .await
            .expect("crossing the direction cap must end the splice")
            .unwrap();
    }

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
