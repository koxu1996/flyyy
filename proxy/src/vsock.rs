//! Reaching a VM: Cloud Hypervisor's vsock Unix socket per machine (its hybrid vsock, the protocol
//! Firecracker's has too), under the worker's runtime directory.

use std::time::Duration;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::agent;

/// A machine's vsock Unix socket in the worker's runtime directory (with the VMs' launch records,
/// shared by the agent and the backhaul).
pub(crate) fn socket(run_dir: &str, machine: &str) -> String {
    format!("{run_dir}/{machine}.vsock")
}

/// `vsock:<machine id>:<port>` (a fact's addr). The id becomes a path under the run dir, so it must be
/// a plain machine id (`[a-z0-9-]{1,32}`: no `/`, no `.`).
pub(crate) fn parse_vsock(addr: &str) -> Result<(String, u16), String> {
    let rest = addr.strip_prefix("vsock:").ok_or_else(|| format!("addr {addr:?}: want vsock:<id>:<port>"))?;
    let (machine, port) = rest.rsplit_once(':').ok_or_else(|| format!("addr {addr:?}: want vsock:<id>:<port>"))?;
    if !agent::spec::name_ok(machine) {
        return Err(format!("addr {addr:?}: bad machine id"));
    }
    match port.parse::<u16>() {
        Ok(p) if p > 0 => Ok((machine.to_owned(), p)),
        _ => Err(format!("addr {addr:?}: bad port")),
    }
}

/// Cloud Hypervisor's reply to `CONNECT <port>\n` is `OK <host-side port>\n`.
pub(crate) fn handshake_ok(line: &[u8]) -> bool {
    line.starts_with(b"OK ") && line.ends_with(b"\n") && line[3..line.len() - 1].iter().all(u8::is_ascii_digit) && line.len() > 4
}

/// Connect to a VM's vsock port through Cloud Hypervisor's Unix socket: `CONNECT <port>\n`, then one reply
/// line, read a byte at a time (bounded to 64 bytes) so nothing of the stream after it is consumed.
/// The whole handshake has a 1 s timeout, like the old TCP dial.
pub(crate) async fn dial_vsock(path: &str, port: u16) -> Result<UnixStream, String> {
    let handshake = async {
        let mut s = UnixStream::connect(&path).await.map_err(|e| format!("connect {path}: {e}"))?;
        s.write_all(format!("CONNECT {port}\n").as_bytes()).await.map_err(|e| format!("write {path}: {e}"))?;
        let mut line = Vec::with_capacity(16);
        while line.last() != Some(&b'\n') {
            if line.len() >= 64 {
                return Err(format!("{path}: handshake reply too long"));
            }
            let mut b = [0u8];
            match s.read(&mut b).await {
                Ok(0) => return Err(format!("{path}: closed during handshake (nothing on port {port}?)")),
                Ok(_) => line.push(b[0]),
                Err(e) => return Err(format!("read {path}: {e}")),
            }
        }
        if handshake_ok(&line) {
            Ok(s)
        } else {
            Err(format!("{path}: bad handshake reply {:?}", String::from_utf8_lossy(&line)))
        }
    };
    tokio::time::timeout(Duration::from_secs(1), handshake).await.map_err(|_| format!("{path}: handshake timeout"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vsock_addr_and_handshake_parsing() {
        assert_eq!(parse_vsock("vsock:whoami-ab12cd:80"), Ok(("whoami-ab12cd".into(), 80)));
        for bad in ["vsock:../x:80", "vsock:a/b:80", "vsock:x:", "vsock:x:65536", "vsock:x:0", "vsock::80", "10.0.0.1:80", "vsock:X:80"] {
            assert!(parse_vsock(bad).is_err(), "{bad}");
        }
        assert!(handshake_ok(b"OK 1073741824\n"));
        for bad in [&b"OK \n"[..], b"OK 12", b"NO 12\n", b"OK 1x\n", b""] {
            assert!(!handshake_ok(bad), "{:?}", String::from_utf8_lossy(bad));
        }
    }
}
