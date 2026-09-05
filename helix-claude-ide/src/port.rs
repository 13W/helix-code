//! Port selection, see `claude-code-ide-protocol-spec.md` §2.1: a random port
//! in `10000..=65535`, at most 50 attempts, each verified by a real bind on
//! `127.0.0.1`.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;

pub const PORT_MIN: u16 = 10000;
pub const PORT_RANGE: u32 = 55536; // 10000..=65535
pub const MAX_ATTEMPTS: usize = 50;

/// Bind a listener on `127.0.0.1`, either on the given port or on a random
/// one from the protocol range.
pub async fn bind(fixed_port: Option<u16>) -> io::Result<TcpListener> {
    if let Some(port) = fixed_port {
        return TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await;
    }
    let mut rng = XorShift::seeded();
    let mut last_err = None;
    for _ in 0..MAX_ATTEMPTS {
        let port = random_port(&mut rng);
        match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await {
            Ok(listener) => return Ok(listener),
            Err(e) => last_err = Some(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "Failed to find an available port after multiple attempts: {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        ),
    ))
}

pub fn random_port(rng: &mut XorShift) -> u16 {
    PORT_MIN + (rng.next_u32() % PORT_RANGE) as u16
}

/// Tiny PRNG; the port only needs to be unpredictable enough to avoid
/// collisions between concurrently starting editors, not cryptographically.
pub struct XorShift(u64);

impl XorShift {
    pub fn seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let pid = std::process::id() as u64;
        let addr = &nanos as *const u64 as u64; // ASLR adds a little entropy
        let seed = nanos ^ pid.rotate_left(32) ^ addr.rotate_left(17);
        XorShift(if seed == 0 {
            0x2545_F491_4F6C_DD1D
        } else {
            seed
        })
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_stay_in_range() {
        let mut rng = XorShift::seeded();
        for _ in 0..10_000 {
            let p = random_port(&mut rng);
            assert!((PORT_MIN..=u16::MAX).contains(&p));
        }
    }

    #[tokio::test]
    async fn binds_random_port_on_loopback() {
        let listener = bind(None).await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
        assert!(addr.port() >= PORT_MIN);
    }

    #[tokio::test]
    async fn fixed_port_is_honoured() {
        // Grab a free port first so the test does not depend on a hard-coded number.
        let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let listener = bind(Some(port)).await.unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }
}
