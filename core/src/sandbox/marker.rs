//! Authentication of the confinement marker.
//!
//! `RAL_SANDBOX_ACTIVE` records that this process already runs inside an
//! OS sandbox, so a restrictive `grant` body must dispatch *locally*
//! rather than re-enter confinement: re-sandboxing is redundant under
//! bwrap and impossible under Seatbelt (one-shot per process).  The
//! marker therefore suppresses re-sandboxing only — never the in-process
//! capability checks, which keep folding the grant stack regardless.
//!
//! The hazard is that the marker is an inherited, public-name string: any
//! ancestor process can export it.  Trusting its mere *presence* lets an
//! arbitrary parent switch off the OS layer — the sole enforcer of `net`
//! and of bundled-coreutils filesystem access (review findings S1, S2).
//!
//! The marker is therefore authenticated by a **capability token**: a
//! fresh random secret minted by ral's own sandbox re-exec, per re-exec.
//! The token reaches the confined child only over the IPC request channel
//! ([`ChildEvalRequest`](crate::child_eval::ChildEvalRequest)) — a
//! socketpair / named pipe the parent ral owns, which an external wrapper
//! cannot write to.  The child [`adopt`]s the token: it records the
//! secret process-globally and stamps the same value into the env marker.
//! [`authenticated`] then trusts the marker only when its env value
//! equals the recorded token.  A forged or bare `RAL_SANDBOX_ACTIVE` has
//! no matching record, so it does not authenticate and confinement is
//! still attempted.
//!
//! This is the *cheaper middle* of the two designs in recommendation A8.
//! The stronger alternative passes the marker as an inherited file
//! descriptor whose pipe-peer relationship a wrapper cannot fabricate;
//! the token raises the bar from "any ambient environment" to "can read
//! ral's private IPC request channel", which is the same trust boundary
//! the rest of the confined eval already rests on.

use std::sync::OnceLock;

/// The token recorded from the authenticated IPC request, set once per
/// confined child by [`adopt`].  Empty in any process that never served
/// a wire request — including a top-level ral whose env merely inherited
/// the marker from an arbitrary parent.
static RECORDED_TOKEN: OnceLock<String> = OnceLock::new();

/// Mint a fresh capability token for one sandbox re-exec.  32 bytes of
/// OS entropy, hex-encoded so it survives an env-var round trip.
pub(super) fn mint() -> String {
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Adopt the token delivered over the authenticated IPC request: record
/// it process-globally and stamp it into the env marker so
/// [`authenticated`] (and the env-presence reads that signal "confined
/// child") observe the token value.  Idempotent — a child serves exactly
/// one request, but a second call is harmless.
pub(crate) fn adopt(token: &str) {
    let _ = RECORDED_TOKEN.set(token.to_string());
    // Safety: the confined child is single-threaded at this point — the
    // serve loop has read its one request and not yet entered eval, so no
    // other thread reads the environment concurrently.
    unsafe {
        std::env::set_var(super::SANDBOX_ACTIVE_ENV, token);
    }
}

/// Whether this process is a genuinely-confined ral child: the env marker
/// is present *and* its value equals the token recorded from the IPC
/// request.  The comparison runs in constant time so a near-miss forgery
/// cannot be tuned byte-by-byte through a timing side channel.
///
/// This is the enforcement-relevant predicate.  A bare or forged
/// `RAL_SANDBOX_ACTIVE` fails it (no recorded token, or a mismatch), so
/// callers fall through to attempting confinement rather than trusting
/// the ambient string.
// Reads the confinement marker to authenticate against the recorded token;
// not a basedir.
#[allow(clippy::disallowed_methods)]
pub(crate) fn authenticated() -> bool {
    let Some(recorded) = RECORDED_TOKEN.get() else {
        return false;
    };
    match std::env::var_os(super::SANDBOX_ACTIVE_ENV) {
        Some(value) => constant_time_eq(value.as_encoded_bytes(), recorded.as_bytes()),
        None => false,
    }
}

/// Length-aware constant-time byte comparison.  Returns early only on a
/// length mismatch (which the token's fixed width makes uninformative);
/// otherwise every byte is folded so the running time does not depend on
/// the position of the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Fill `buf` with cryptographically-strong OS entropy.
///
/// Unix reads `/dev/urandom`; Windows calls `ProcessPrng`, the modern
/// user-space CSPRNG.  Both are the primitives `getrandom` itself selects
/// on these platforms, reached directly to avoid a new dependency for one
/// 32-byte draw.  A failure here is unrecoverable for a security token,
/// so it panics rather than minting a guessable value.
#[cfg(unix)]
fn fill_random(buf: &mut [u8]) {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").expect("sandbox token: open /dev/urandom");
    f.read_exact(buf).expect("sandbox token: read /dev/urandom");
}

#[cfg(windows)]
fn fill_random(buf: &mut [u8]) {
    use windows_sys::Win32::Security::Cryptography::ProcessPrng;
    // ProcessPrng never fails for a valid buffer (it returns nonzero);
    // treat a zero return as fatal rather than minting weak entropy.
    let ok = unsafe { ProcessPrng(buf.as_mut_ptr(), buf.len()) };
    assert!(ok != 0, "sandbox token: ProcessPrng failed");
}

#[cfg(not(any(unix, windows)))]
fn fill_random(_buf: &mut [u8]) {
    panic!("sandbox token: no OS entropy source on this platform");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_distinct_and_hex() {
        let a = mint();
        let b = mint();
        assert_ne!(a, b, "two mints must not collide");
        assert_eq!(a.len(), 64, "32 bytes hex-encode to 64 chars");
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
