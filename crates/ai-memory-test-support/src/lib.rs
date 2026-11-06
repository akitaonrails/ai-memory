//! Helpers shared by this workspace's tests.
//!
//! Dev-dependency only. Nothing here ships, and nothing here may depend on
//! another workspace crate: a dev-dependency cycle makes cargo compile the
//! depended-on crate twice and the two copies' types do not unify.
//!
//! No tests of its own either (`[lib] test = false`): every test binary in the
//! workspace costs a link and, on macOS and Windows, a first-run scan, and the
//! helpers here are exercised by every test that calls them.

use std::net::{SocketAddr, TcpListener};

/// A loopback port that refuses every connection and stays reserved for as
/// long as this value lives.
///
/// A port found by binding a listener and dropping it is free again at once,
/// so under `cargo test`, where a binary's tests run as threads in one process,
/// another test's server can take it. The "dead" endpoint then answers and
/// counts requests that were never its own (#2030). Here the socket stays
/// bound but never listens: the kernel refuses connections to it, and nothing
/// else can bind the port. Unlike [`dead_http_endpoint`], a connection to it
/// fails before it is established, which is what tests of connect-level
/// failures need.
pub struct RefusingPort {
    socket: socket2::Socket,
    addr: SocketAddr,
}

impl RefusingPort {
    /// The reserved loopback address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Start listening on the reserved port, for a test whose server comes
    /// back at the same address. The listener is non-blocking, ready for
    /// `tokio::net::TcpListener::from_std`.
    ///
    /// # Panics
    /// Panics if the socket cannot listen.
    #[must_use]
    pub fn into_listener(self) -> TcpListener {
        self.socket
            .listen(128)
            .expect("listen on the reserved port");
        let listener: TcpListener = self.socket.into();
        listener
            .set_nonblocking(true)
            .expect("set the listener non-blocking");
        listener
    }
}

/// Reserve a loopback port that refuses connections; see [`RefusingPort`].
///
/// # Panics
/// Panics if a loopback TCP socket cannot be created or bound.
#[must_use]
pub fn refusing_port() -> RefusingPort {
    use socket2::{Domain, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).expect("create a TCP socket");
    let any: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
    socket.bind(&any.into()).expect("bind a loopback port");
    let addr = socket
        .local_addr()
        .expect("read the bound address")
        .as_socket()
        .expect("an IP socket address");
    RefusingPort { socket, addr }
}

/// A loopback HTTP endpoint that accepts every connection and closes it at
/// once, so a request to it fails immediately with a connection error.
///
/// Tests that need "the server is down" used to point at a closed port.
/// Linux and macOS refuse that connect in microseconds; Windows retransmits
/// the SYN and takes about two seconds, which turned every hook test that
/// posts to a dead server into a multi-second test. The listener thread lives
/// for the rest of the process, which for a test binary is short.
///
/// Where binding is denied (a sandbox without network), this falls back to
/// the closed reserved port, so such tests keep their old behaviour there
/// instead of failing on the helper.
pub fn dead_http_endpoint() -> String {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return "http://127.0.0.1:1".to_string();
    };
    let Ok(addr) = listener.local_addr() else {
        return "http://127.0.0.1:1".to_string();
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

/// The first PowerShell on this machine that starts and exits cleanly,
/// preferring Windows PowerShell and falling back to PowerShell 7 (`pwsh`).
///
/// Resolved once per process: probing spawns a shell, and the tests that need
/// this spawn several themselves.
///
/// # Panics
/// Panics if neither executable runs. The tests that call this cannot pass
/// without one, so failing loudly beats a confusing spawn error later.
#[cfg(windows)]
pub fn powershell_exe() -> &'static str {
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    static RESOLVED: OnceLock<&'static str> = OnceLock::new();
    RESOLVED.get_or_init(|| {
        ["powershell.exe", "pwsh.exe"]
            .into_iter()
            .find(|exe| {
                Command::new(exe)
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "exit 0",
                    ])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            })
            .expect("PowerShell should be available")
    })
}
