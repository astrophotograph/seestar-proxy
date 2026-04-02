//! Transparent proxy support via iptables REDIRECT + SO_ORIGINAL_DST.
//!
//! When the proxy runs behind an iptables REDIRECT rule, incoming connections
//! are redirected to the proxy's listening port. The original destination
//! (the Seestar's real IP and port) can be recovered from the socket using
//! `getsockopt(SOL_IP, SO_ORIGINAL_DST)`.
//!
//! This is Linux-only. On other platforms, `get_original_dst` returns `None`.

use std::net::SocketAddr;

/// Linux `SO_ORIGINAL_DST` constant from netfilter (not in the libc crate).
#[cfg(target_os = "linux")]
const SO_ORIGINAL_DST: libc::c_int = 80;

/// Extract the original destination address from a redirected TCP connection.
///
/// Returns `None` on non-Linux platforms or if the socket wasn't redirected
/// (e.g. a direct connection to the proxy).
#[cfg(target_os = "linux")]
pub fn get_original_dst(fd: std::os::unix::io::RawFd) -> Option<SocketAddr> {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret == 0 {
        let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
        let port = u16::from_be(addr.sin_port);
        Some(SocketAddr::new(ip.into(), port))
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub fn get_original_dst(_fd: std::os::unix::io::RawFd) -> Option<SocketAddr> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[tokio::test]
    async fn non_redirected_socket_returns_some_local_addr() {
        // On Linux, SO_ORIGINAL_DST on a non-redirected socket succeeds and
        // returns the local (destination) address. On non-Linux it returns None.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        let result = get_original_dst(stream.as_raw_fd());
        #[cfg(target_os = "linux")]
        {
            // Returns the socket's local address (since no iptables redirect).
            let orig = result.expect("SO_ORIGINAL_DST should succeed on Linux");
            assert_eq!(orig.ip(), addr.ip());
            assert_eq!(orig.port(), addr.port());
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(result.is_none());
        }
    }
}
