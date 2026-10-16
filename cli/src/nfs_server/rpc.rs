//! The RPC-over-UDP client the keeper uses to read rpcbind's registrations
//! and probe a daemon with a NULL call (`nfs_server` is the server and the
//! keeper that drives this). Just enough of ONC RPC v2 (RFC 1057): an
//! AUTH_NULL CALL, sent twice in case a datagram is lost, and the accepted
//! reply parsed for its results.

use std::collections::HashSet;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

/// RPC programs: the portmapper, NFS and the mount protocol.
const PMAP_PROG: u32 = 100000;
pub(super) const NFS_PROG: u32 = 100003;
pub(super) const MOUNT_PROG: u32 = 100005;
pub(super) const IPPROTO_UDP: u32 = 17;

/// The programs rpcbind has registered (PMAPPROC_DUMP).
pub(super) fn registrations(portmapper: SocketAddr) -> io::Result<HashSet<u32>> {
    Ok(mappings(portmapper)?.into_iter().map(|m| m.program).collect())
}

/// One of rpcbind's registrations.
#[derive(Debug, PartialEq)]
pub(super) struct Mapping {
    pub(super) program: u32,
    pub(super) version: u32,
    pub(super) protocol: u32,
    pub(super) port: u32,
}

/// rpcbind's registrations: PMAPPROC_DUMP, version 2 of the portmapper.
pub(super) fn mappings(portmapper: SocketAddr) -> io::Result<Vec<Mapping>> {
    let reply = call(portmapper, PMAP_PROG, 2, 4, Duration::from_secs(2))?;
    let mut words = reply.chunks_exact(4).map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]));
    let mut out = Vec::new();
    loop {
        match words.next() {
            Some(1) => {}
            Some(0) => return Ok(out),
            _ => return Err(io::Error::other("rpcbind's list of registrations is cut short")),
        }
        let mut next = || words.next().ok_or_else(|| io::Error::other("rpcbind's registration is cut short"));
        out.push(Mapping { program: next()?, version: next()?, protocol: next()?, port: next()? });
    }
}

/// Call procedure `procedure` (with no arguments) of `program` version
/// `version` at `addr` over UDP, and return the results. Sent twice within
/// `timeout` in case the first datagram is lost.
pub(super) fn call(
    addr: SocketAddr,
    program: u32,
    version: u32,
    procedure: u32,
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.connect(addr)?;
    socket.set_read_timeout(Some(timeout / 2))?;
    let clock = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let xid = std::process::id().rotate_left(16) ^ clock.subsec_nanos();
    // CALL, RPC version 2, the procedure, and AUTH_NULL credentials and
    // verifier (flavour 0, no body).
    let message: Vec<u8> =
        [xid, 0, 2, program, version, procedure, 0, 0, 0, 0].iter().flat_map(|w| w.to_be_bytes()).collect();
    let mut buf = vec![0u8; 64 * 1024];
    for _ in 0..2 {
        socket.send(&message)?;
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        };
        if let Some(results) = accepted(&buf[..n], xid)? {
            return Ok(results.to_vec());
        }
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, format!("no answer from program {program} at {addr}")))
}

/// The results in an RPC reply to `xid`: None if it answers another call.
fn accepted(reply: &[u8], xid: u32) -> io::Result<Option<&[u8]>> {
    let word = |i: usize| reply.get(4 * i..4 * i + 4).map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]));
    let short = || io::Error::other("a short RPC reply");
    if word(0) != Some(xid) || word(1) != Some(1) {
        return Ok(None);
    }
    // MSG_ACCEPTED, then the verifier (flavour, length, body padded to 4).
    if word(2).ok_or_else(short)? != 0 {
        return Err(io::Error::other("the RPC call was denied"));
    }
    let verifier = (word(4).ok_or_else(short)? as usize).div_ceil(4);
    match word(5 + verifier).ok_or_else(short)? {
        0 => Ok(Some(&reply[4 * (6 + verifier)..])),
        stat => Err(io::Error::other(format!("the RPC call failed (accept_stat {stat})"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    /// A fake portmapper on a UDP port: answers every call with `results`
    /// after an accepted-reply header, or with a denial.
    fn fake_rpc(results: Vec<u8>, deny: bool) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = socket.recv_from(&mut buf) {
                assert!(n >= 40, "a whole call");
                let mut reply = buf[..4].to_vec(); // the xid
                let tail: &[u32] = if deny { &[1, 1, 0, 0] } else { &[1, 0, 0, 0, 0] };
                reply.extend(tail.iter().flat_map(|w| w.to_be_bytes()));
                reply.extend(&results);
                socket.send_to(&reply, from).unwrap();
            }
        });
        addr
    }

    fn words(ws: &[u32]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn reads_rpcbinds_registrations() {
        let dump = words(&[1, 100003, 2, 17, 2049, 1, 100005, 1, 17, 64970, 0]);
        let maps = mappings(fake_rpc(dump, false)).unwrap();
        assert_eq!(maps[1], Mapping { program: 100005, version: 1, protocol: 17, port: 64970 });
        assert_eq!(registrations(fake_rpc(words(&[0]), false)).unwrap(), HashSet::new());
        assert!(mappings(fake_rpc(words(&[1, 100003]), false)).is_err(), "cut short");
        assert!(mappings(fake_rpc(Vec::new(), true)).is_err(), "denied");
    }

    #[test]
    fn a_call_to_nothing_times_out() {
        let addr = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let started = Instant::now();
        assert!(call(addr, NFS_PROG, 2, 0, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
