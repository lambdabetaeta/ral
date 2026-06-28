//! Engine process: the session-lived child that holds the Shell
//! and executes turns sent over a framed socket by the front-end.

use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;

use crate::driver::TurnRequest;
use crate::transport::{Control, Event, Frame, Turn, report_to_mirror};
use crate::types::{Boundary, Shell, SurfaceSink, Value};
use crate::wire::WireChannel;
use crate::serial::{InternCtx, SerialValue};

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

/// Run the engine loop on an inherited socket (fd 3).
/// The front-end passes the socket as fd 3 before exec.
/// Never returns.
pub fn run_engine() -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    let mut channel = WireChannel::from_stream(stream);

    // Boot a Shell with default configuration
    let shell = Shell::new(Default::default());

    // Run the engine loop
    let code = engine_loop(&mut channel, shell);
    std::process::exit(code);
}

/// A surface sink that buffers events in a Vec for flush after the turn.
struct BufferSurfaceSink {
    buf: Arc<Mutex<Vec<Value>>>,
}

impl crate::types::EventSink for BufferSurfaceSink {
    fn emit(&self, ev: &Value) {
        self.buf.lock().unwrap().push(ev.clone());
    }
}

/// The main engine loop: read frames, execute, write events back.
fn engine_loop(channel: &mut WireChannel, mut shell: Shell) -> i32 {
    let surface_buf: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let current_dispatch = Arc::new(AtomicU64::new(0));

    loop {
        let frame = match channel.read_frame() {
            Ok(Some(f)) => f,
            Ok(None) => {
                // EOF — front-end gone. Cancel in-flight, reap, exit.
                crate::process::request_foreground_cancel(
                    crate::process::CancelCause::Explicit
                );
                return 0;
            }
            Err(e) => {
                eprintln!("engine: read error: {e}");
                return 1;
            }
        };

        match frame {
            Frame::Attach(endpoint) => {
                // Store the terminal endpoint.  The lease (if any) is
                // conveyed via fd-passing (SCM_RIGHTS) rather than
                // serialised (`#[serde(skip)]`).  Phase 2 Task 6:
                //   - Receive fds with recvmsg + SCM_RIGHTS before the
                //     Attach frame arrives.
                //   - dup2 the received fds to 0,1,2.
                //   - Mint a TerminalLease from them.
                // For the initial implementation the endpoint is stored
                // as-is; its lease field is None until fd-passing lands.
                let _endpoint = endpoint;
                // Store the terminal endpoint (lease conveyed via fd-passing).
                // Full fd-passing happens in task 6.
            }
            Frame::Dispatch(id, turn) => {
                // Execute the turn synchronously.
                current_dispatch.store(id.0, Ordering::Relaxed);
                surface_buf.lock().unwrap().clear();

                // Extract the ReqMirror (same shape for both Source and Hook).
                let req = match &turn {
                    Turn::Source { req, .. } | Turn::Hook { req, .. } => req,
                };

                // Build the TurnRequest from the mirror.
                let script_name = req.script_name.clone();
                let surface_sink: SurfaceSink = Arc::new(BufferSurfaceSink {
                    buf: surface_buf.clone(),
                });
                // Boundary sink: for now, a no-op that discards detached-worker
                // surface batches.  Task 6 will wire this to the channel.
                let boundary_sink: Option<Boundary> = None;

                let turn_req = TurnRequest {
                    script_name: &script_name,
                    caps: req.caps.clone(),
                    turn_limit: req.turn_limit,
                    detached_limit: req.detached_limit,
                    io: req.io,
                    terminal: req.terminal,
                    stdin: req.stdin,
                    surface: Some(surface_sink),
                    boundary: boundary_sink,
                    lifecycle: Box::new(()),
                };

                // Run the turn against the shell.
                let report = match turn {
                    Turn::Source { src, .. } => {
                        shell.run_source_turn(&src, turn_req)
                    }
                    Turn::Hook { name, args, .. } => {
                        // Convert SerialValue args back to live Value.
                        // For the initial implementation, use empty scope arcs
                        // (handles only simple ground values — Int, String,
                        // Bool, List, Map — not closures).
                        let arcs: crate::serial::ScopeArcs = Vec::new();
                        let live_args: Vec<Value> = args
                            .into_iter()
                            .filter_map(|sv| sv.into_runtime(&arcs).ok())
                            .collect();
                        shell.run_hook(&name, live_args, turn_req)
                    }
                };

                // Flush buffered surface events as Event::Surface frames,
                // then send the final Report.
                {
                    let mut ctx = InternCtx::new();
                    let surface_values: Vec<Value> =
                        std::mem::take(&mut *surface_buf.lock().unwrap());
                    for v in &surface_values {
                        if let Ok(sv) = SerialValue::from_runtime(v, &mut ctx) {
                            let _ = channel.write_frame(
                                &Frame::Event(id, Event::Surface(sv))
                            );
                        }
                    }
                    // Convert TurnReport → ReportMirror and send as terminal frame.
                    let report_mirror = report_to_mirror(report);
                    let _ = channel.write_frame(
                        &Frame::Event(id, Event::Report(report_mirror))
                    );
                }
            }
            Frame::Control(Control::Cancel(_id)) => {
                crate::process::request_foreground_cancel(
                    crate::process::CancelCause::Explicit
                );
            }
            Frame::Control(Control::Suspend) => {
                // Phase 2 Task 7 (signal relocation): the front-end
                // installs OS signal handlers and translates SIGTSTP
                // into this Control frame.  The engine should suspend
                // its process group here.
                // no-op for now
            }
            Frame::Control(Control::Resume) => {
                // Phase 2 Task 7 (signal relocation): the front-end
                // translates SIGCONT into this Control frame.
                // no-op for now
            }
            Frame::Control(Control::Resize(winsize)) => {
                // Phase 2 Task 7 (signal relocation): the front-end
                // translates SIGWINCH into this Control frame.  The
                // engine should update its terminal state and forward
                // to the foreground process group.
                let _winsize = winsize;
                // no-op for now
            }
            Frame::Detach => {
                crate::process::request_foreground_cancel(
                    crate::process::CancelCause::Explicit
                );
                return 0;
            }
            Frame::Event(..) => {
                // Engine should never receive Event frames
                eprintln!("engine: unexpected Event frame");
            }
        }
    }
}

