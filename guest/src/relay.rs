//! The app's ports over vsock: a listener per port in FLYYY_PORTS (the worker's backhaul connects to that
//! vsock port of the VM), each connection relayed to 127.0.0.1:<port> on threads of its own, one per
//! direction. The client's half-close is passed on to the app (it stops sending, the app's read side
//! ends, and it can still answer). The app's end closes both connections fully, as socat did, so an
//! answer that ends by its connection closing (no Content-Length) ends at the edge too. Firecracker's
//! vsock needed that (it doesn't pass a guest's half-close to the host); Cloud Hypervisor's passes it on
//! as an EOF, but a full close is still what the app's end means. Nothing here may panic: this is
//! PID 1, built with panic = "abort", and its end is the VM's.

use std::{
    fmt::Display,
    io::{self, Read, Write},
    net::{Shutdown, TcpStream},
    os::fd::AsRawFd,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use nix::sys::socket::{bind, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use vsock::{VsockListener, VsockStream};

/// The listen backlog: under load the default (socat's was 5) overflows and the vsock handshake closes,
/// which the edge answers with a 502.
const BACKLOG: libc::c_int = 1024;
/// Relay threads only copy: a small stack each, in a VM of 128 MiB.
const STACK: usize = 256 * 1024;

/// FLYYY_PORTS: the app's ports, comma-separated, each once.
pub fn ports(spec: &str) -> Result<Vec<u16>, String> {
    let mut out: Vec<u16> = Vec::new();
    for p in spec.split(',') {
        let port: u16 = p
            .parse()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| format!("FLYYY_PORTS: bad port {p:?}"))?;
        if out.contains(&port) {
            return Err(format!("FLYYY_PORTS: port {port} twice"));
        }
        out.push(port);
    }
    Ok(out)
}

/// A vsock listener on `port`, any CID. Made with nix and listen(2) directly: nix's Backlog (and the vsock
/// crate's bind, which uses it) refuses more than SOMAXCONN, which musl has as 128.
pub fn listen(port: u16) -> Result<VsockListener, String> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|e| format!("vsock socket: {e}"))?;
    bind(
        fd.as_raw_fd(),
        &VsockAddr::new(libc::VMADDR_CID_ANY, port.into()),
    )
    .map_err(|e| format!("vsock bind :{port}: {e}"))?;
    // SAFETY: a valid socket descriptor; plain syscall.
    if unsafe { libc::listen(fd.as_raw_fd(), BACKLOG) } != 0 {
        return Err(format!(
            "vsock listen :{port}: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(VsockListener::from(fd))
}

/// At most one console message a second about dropped connections; the ones in between are counted and
/// reported with the next. The console is a slow serial line shared with the kernel log, and a flood of
/// messages would stall the very loop that's failing.
pub struct RateLimit {
    /// The second (since the first call, plus 1) of the last message; 0: none yet.
    last: AtomicU64,
    suppressed: AtomicU64,
}

impl RateLimit {
    pub const fn new() -> Self {
        RateLimit {
            last: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Some(how many were suppressed since the last message) if this one should be logged, at `now`
    /// seconds; None if it's counted instead.
    pub fn hit(&self, now: u64) -> Option<u64> {
        let now = now + 1;
        let last = self.last.load(Relaxed);
        if last >= now
            || self
                .last
                .compare_exchange(last, now, Relaxed, Relaxed)
                .is_err()
        {
            self.suppressed.fetch_add(1, Relaxed);
            return None;
        }
        Some(self.suppressed.swap(0, Relaxed))
    }
}

static DROPS: RateLimit = RateLimit::new();

/// A connection on `port` dropped by the guest itself (not by the app): logged, rate-limited. writeln!
/// on a locked stderr, its error ignored: eprintln! panics if the write fails, and this is PID 1.
fn dropped(port: u16, what: &str, e: &dyn Display) {
    static START: OnceLock<Instant> = OnceLock::new();
    let now = START.get_or_init(Instant::now).elapsed().as_secs();
    if let Some(more) = DROPS.hit(now) {
        let mut err = io::stderr().lock();
        let _ = match more {
            0 => writeln!(err, "flyyy-init: vsock :{port}: {what}: {e}"),
            n => writeln!(err, "flyyy-init: vsock :{port}: {what}: {e} (and {n} more dropped since the last message)"),
        };
    }
}

/// Accept forever, a relay per connection. A connection that can't get a thread is dropped (closed) and
/// logged; so is a failed accept.
pub fn serve(listener: VsockListener, port: u16) {
    loop {
        match listener.accept() {
            Ok((v, _)) => {
                let spawned = thread::Builder::new().stack_size(STACK).spawn(move || {
                    // Nothing listening there yet (the app is still starting): close, as socat did. Not
                    // logged: every boot has it until the app listens, and the worker's health check
                    // reports an app that never does.
                    if let Ok(tcp) = TcpStream::connect(("127.0.0.1", port)) {
                        if let Err(e) = relay(v, tcp) {
                            dropped(port, "connection closed, no relay", &e);
                        }
                    }
                });
                if let Err(e) = spawned {
                    dropped(port, "connection closed, no thread", &e);
                    thread::sleep(Duration::from_millis(10));
                }
            }
            // Out of descriptors, say: let some close, then go on.
            Err(e) => {
                dropped(port, "accept", &e);
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A stream that can be shut down one way or both.
pub trait Duplex: Read + Write + Send + Sized + 'static {
    fn split(&self) -> io::Result<Self>;
    fn shut(&self, how: Shutdown);
}

impl Duplex for VsockStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shut(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

impl Duplex for TcpStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn shut(&self, how: Shutdown) {
        let _ = self.shutdown(how);
    }
}

/// Bytes both ways, `client` (the vsock connection) to and from `app` (its TCP connection), until both
/// directions are done. The client's EOF shuts the app's write side (its half-close, passed on). The
/// app's EOF (or an error either way) shuts both connections down fully: the client sees the end even
/// though it never closed its side, and the other direction's blocked read returns, so the relay ends.
/// An error means the relay couldn't start (no descriptor or no thread for its second half), and both
/// connections are closed; a copy's error is how a connection ends, not an error here.
pub fn relay<C: Duplex, A: Duplex>(client: C, app: A) -> io::Result<()> {
    let client2 = client.split()?;
    let app2 = app.split()?;
    let back = thread::Builder::new().stack_size(STACK).spawn(move || {
        let (mut from, mut to) = (app2, client2);
        let _ = io::copy(&mut from, &mut to);
        to.shut(Shutdown::Both);
        from.shut(Shutdown::Both);
    });
    let back = back?;
    let (mut from, mut to) = (client, app);
    match io::copy(&mut from, &mut to) {
        Ok(_) => to.shut(Shutdown::Write),
        Err(_) => {
            to.shut(Shutdown::Both);
            from.shut(Shutdown::Both);
        }
    }
    let _ = back.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn ports_are_checked() {
        assert_eq!(ports("8080"), Ok(vec![8080]));
        assert_eq!(ports("80,5432"), Ok(vec![80, 5432]));
        for bad in ["", "0", "x", "80,", "70000", "80,80", " 80"] {
            assert!(ports(bad).is_err(), "{bad:?}");
        }
    }

    /// The app answers and closes while the client keeps its sending side open (it waits for the end of
    /// an answer without Content-Length): the client must see the end, and the relay must finish.
    #[test]
    fn the_apps_end_closes_both_sides() {
        let (mut client, a) = pair();
        let (b, mut server) = pair();
        let (done_tx, done) = std::sync::mpsc::channel();
        thread::spawn(move || {
            relay(a, b).unwrap();
            let _ = done_tx.send(());
        });
        client.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        let mut req = [0u8; 18];
        server.read_exact(&mut req).unwrap();
        server
            .write_all(b"HTTP/1.0 200 OK\r\n\r\nthe body, to EOF")
            .unwrap();
        drop(server);
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answer = Vec::new();
        client
            .read_to_end(&mut answer)
            .expect("the end, not a timeout");
        assert!(answer.ends_with(b"the body, to EOF"));
        done.recv_timeout(Duration::from_secs(5))
            .expect("the relay finished");
    }

    /// One message a second at most, with the count of the ones in between.
    #[test]
    fn drops_are_logged_once_a_second_with_a_count() {
        let r = RateLimit::new();
        assert_eq!(r.hit(0), Some(0), "the first is logged");
        assert_eq!(r.hit(0), None);
        assert_eq!(r.hit(0), None);
        assert_eq!(r.hit(1), Some(2), "the next second, with the two counted");
        assert_eq!(r.hit(1), None);
        assert_eq!(r.hit(5), Some(1));
        assert_eq!(r.hit(4), None, "a late caller's older second counts too");
    }

    /// A pair of connected TCP streams.
    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        (c, l.accept().unwrap().0)
    }

    #[test]
    fn relays_both_ways_and_passes_half_closes_on() {
        // client <-> (a | relay | b) <-> server, TCP standing in for vsock.
        let (mut client, a) = pair();
        let (b, mut server) = pair();
        let t = thread::spawn(move || relay(a, b).unwrap());
        client.write_all(b"request").unwrap();
        // The client is done sending: the server sees EOF after the request, and can still answer.
        client.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        server.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"request");
        server.write_all(b"answer").unwrap();
        server.shutdown(Shutdown::Write).unwrap();
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"answer");
        t.join().unwrap();
    }
}
