//! Two threads of PID 1 carry packets between tun and the net wire.
//! Each destination has one writer; either direction ending stops the session.

use std::convert::Infallible;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread;

use guest_abi::{link, packet};

use crate::reap;

/// Called after signals are blocked; both threads retain the inherited mask.
pub(crate) fn start(tun: OwnedFd, net: OwnedFd) -> Result<(), String> {
    let (tun, net) = (File::from(tun), File::from(net));
    let (tun_for_net, net_for_net) = (
        tun.try_clone()
            .map_err(|err| format!("could not duplicate the tun descriptor: {err}"))?,
        net.try_clone()
            .map_err(|err| format!("could not duplicate the net-wire descriptor: {err}"))?,
    );
    start_direction("ral-net-out", "tun→net", move || {
        tun_to_net(tun_for_net, net_for_net)
    })?;
    start_direction("ral-net-in", "net→tun", move || net_to_tun(net, tun))
}

fn start_direction(
    name: &str,
    direction: &'static str,
    run: impl FnOnce() -> Result<Infallible, String> + Send + 'static,
) -> Result<(), String> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let Err(why) =
                catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|_| Err("panicked".to_string()));
            reap::fail(epitaph(&format!("in the {direction} direction {why}")));
        })
        .map(|_| ())
        .map_err(|err| format!("could not start the net pump's {direction} thread ({name}): {err}"))
}

fn tun_to_net(mut tun: File, mut net: File) -> Result<Infallible, String> {
    let mut buf = [0u8; link::MTU];
    loop {
        let n = tun
            .read(&mut buf)
            .map_err(|err| format!("could not read the tun device: {err}"))?;
        if n == 0 {
            return Err("saw the tun device close".to_string());
        }
        packet::write_frame(&mut net, &buf[..n])
            .map_err(|err| format!("could not write the net wire: {err}"))?;
    }
}

/// Raw `read_exact` only, no `BufReader`; each frame consumes exactly its bytes.
fn net_to_tun(mut net: File, mut tun: File) -> Result<Infallible, String> {
    loop {
        let frame = packet::read_frame(&mut net)
            .map_err(|err| format!("could not read the net wire: {err}"))?;
        tun.write_all(&frame)
            .map_err(|err| format!("could not write the tun device: {err}"))?;
    }
}

/// The host accepts the net wire once, so a failed pump cannot restart.
fn epitaph(why: &str) -> String {
    format!(
        "the network pump {why}; the session cannot be resumed in place, powering the machine off"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epitaph_names_the_failure_and_the_shutdown() {
        let text = epitaph("in the tun→net direction saw the tun device close");
        assert!(
            text.contains("tun→net direction saw the tun device close"),
            "{text}"
        );
        assert!(text.contains("cannot be resumed"), "{text}");
    }
}
