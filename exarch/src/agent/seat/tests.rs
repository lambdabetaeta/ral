/// The sentence is the product here, so it is the thing under test: that it
/// stays one sentence, that it tells a start apart from a death, and that the
/// two things a debugger needs are both in it.  Platform-independent, unlike
/// the wire fixtures, because nothing in it spawns an engine.
mod lost;

// These drive a real `--engine` child, never an in-process
// `engine_session` thread: that faces its process's signals, so a
// same-process engine would race the ambient cancel cells against whatever
// sibling test in this lib binary is mid-run, and core's lock over those
// cells is unreachable from here.
#[cfg(unix)]
mod wire;
