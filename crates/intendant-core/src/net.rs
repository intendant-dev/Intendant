//! Local network-interface enumeration: small OS probes with no
//! dependency on any daemon subsystem (unix walks `getifaddrs(3)` via
//! libc; Windows goes through the `if-addrs` crate).

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
