//! The demo's own `micromux proxy`, shared by every scenario.

use crate::Error;
use std::path::Path;
use std::time::{Duration, Instant};

/// The port of the demo's proxy, `proxy.port` in `examples/demo/micromux.yaml`.
const PROXY_PORT: u16 = 8480;

/// The demo's proxy process, killed when dropped.
pub(crate) struct Proxy {
    child: std::process::Child,
}

impl Proxy {
    /// Start `micromux proxy` on [`PROXY_PORT`] with `state_dir` and wait until it listens.
    pub(crate) fn start(micromux: &Path, state_dir: &Path) -> Result<Self, Error> {
        const MAX_WAIT: Duration = Duration::from_secs(10);

        // Connecting below would also succeed against a stranger on the port.
        if std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, PROXY_PORT)).is_err() {
            return Err(Error::ProxyPortTaken { port: PROXY_PORT });
        }
        let child = std::process::Command::new(micromux)
            .arg("proxy")
            .arg("--port")
            .arg(PROXY_PORT.to_string())
            .env("MICROMUX_STATE_DIR", state_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()?;
        let mut proxy = Self { child };
        let started = Instant::now();
        while started.elapsed() < MAX_WAIT {
            if proxy.child.try_wait()?.is_some() {
                break;
            }
            if std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, PROXY_PORT)).is_ok() {
                return Ok(proxy);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(Error::ProxyNotReady { port: PROXY_PORT })
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Err(err) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            eprintln!("failed to stop the demo proxy: {err}");
        }
    }
}
