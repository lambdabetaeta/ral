//! Engine process: the session-lived child that holds the Shell
//! and executes turns sent over a framed socket by the front-end.

use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;

use crate::driver::TurnRequest;
use crate::serial::SerialValue;
use crate::transport::{Control, DispatchId, Event, Frame, Turn, report_to_mirror};
use crate::types::{Boundary, Shell, SurfaceSink, Value};
use crate::wire::WireChannel;

use std::sync::{Arc, Mutex, mpsc};

/// A surface sink that writes Event::Surface frames live to the wire
/// as values are produced, rather than buffering.
struct ChannelSurfaceSink {
    id: crate::transport::DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::EventSink for ChannelSurfaceSink {
    fn emit(&self, ev: &Value) {
        if let Ok(sv) = SerialValue::from_ground(ev) {
            let _ = self
                .writer
                .lock()
                .unwrap()
                .write_frame(&Frame::Event(self.id, Event::Surface(sv)));
        }
    }
}

/// A boundary sink that writes Event::BoundarySurface frames to the wire.
struct ChannelBoundarySink {
    id: DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::BoundarySink for ChannelBoundarySink {
    fn deliver(&self, batch: Vec<Value>, joined: std::sync::Arc<std::sync::Mutex<bool>>) {
        let already = {
            let mut guard = joined.lock().unwrap();
            let was = *guard;
            *guard = true;
            was
        };
        if already {
            return;
        }
        let sv_batch: Vec<SerialValue> = batch
            .into_iter()
            .filter_map(|v| SerialValue::from_ground(&v).ok())
            .collect();
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(self.id, Event::BoundarySurface(sv_batch)));
    }
}

/// Run the engine loop on an inherited socket (fd 3).
/// The front-end passes the socket as fd 3 before exec.
/// Never returns.
pub fn run_engine() -> ! {
    // SAFETY: fd 3 is the socket inherited from the front-end
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    let reader_ch = WireChannel::from_stream(stream);
    let writer_ch = reader_ch.try_clone().expect("try_clone engine channel");
    let writer = Arc::new(Mutex::new(writer_ch));

    let shell = Shell::new(Default::default());

    // Rendezvous channel: try_send succeeds only when the worker is
    // idle (blocked on recv).  While the worker runs a turn, try_send
    // fails with Full → reply "engine busy".
    let (turn_tx, turn_rx) = mpsc::sync_channel::<(crate::transport::DispatchId, Turn)>(0);

    // ── Worker thread: owns the Shell ──────────────────────────────
    let worker_writer = writer.clone();
    std::thread::spawn(move || {
        let mut shell = shell;
        while let Ok((id, turn)) = turn_rx.recv() {
            // Extract the request mirror
            let req = match &turn {
                Turn::Source { req, .. } | Turn::Hook { req, .. } => req,
            };

            // Build the TurnRequest
            let script_name = req.script_name.clone();
            let surface_sink: SurfaceSink = Arc::new(ChannelSurfaceSink {
                id,
                writer: worker_writer.clone(),
            });
            let boundary_sink: Option<Boundary> = Some(Arc::new(ChannelBoundarySink {
                id,
                writer: worker_writer.clone(),
            }));

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

            // Run the turn against the shell
            let report = match turn {
                Turn::Source { src, .. } => shell.run_source_turn(&src, turn_req),
                Turn::Hook { name, args, .. } => {
                    // Decode the ground arguments off the seam.
                    let live_args: Vec<Value> = args
                        .into_iter()
                        .filter_map(|sv| sv.into_ground().ok())
                        .collect();
                    shell.run_hook(&name, live_args, turn_req)
                }
            };

            // Convert and send the terminal Report frame.
            let report_mirror = report_to_mirror(report);
            let _ = worker_writer
                .lock()
                .unwrap()
                .write_frame(&Frame::Event(id, Event::Report(report_mirror)));
        }
    });

    // ── Reader loop (this thread) ──────────────────────────────────
    let mut reader_ch = reader_ch;
    loop {
        match reader_ch.read_frame() {
            Ok(Some(Frame::Dispatch(id, turn))) => {
                match turn_tx.try_send((id, turn)) {
                    Ok(()) => { /* worker accepted the turn */ }
                    Err(mpsc::TrySendError::Full(_)) => {
                        // Worker is busy — reply with a static diagnostic.
                        use crate::driver::TurnReport;
                        use crate::turn::StaticDiagnostics;
                        let mirror = report_to_mirror(TurnReport::Static {
                            diagnostics: StaticDiagnostics::Host(crate::types::Error::new(
                                "engine busy",
                                1,
                            )),
                        });
                        let _ = writer
                            .lock()
                            .unwrap()
                            .write_frame(&Frame::Event(id, Event::Report(mirror)));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        break; // worker died
                    }
                }
            }
            Ok(Some(Frame::Control(Control::Cancel(_)))) => {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
            }
            Ok(Some(Frame::Control(Control::Resize(_winsize)))) => {
                // no-op for now, TODO task 7
            }
            Ok(Some(Frame::Control(Control::Suspend))) => {}
            Ok(Some(Frame::Control(Control::Resume))) => {}
            Ok(Some(Frame::Attach {
                endpoint,
                cwd,
                home,
                rc_path,
                proto_version,
            })) => {
                // Check protocol version.
                use crate::transport::PROTOCOL_VERSION;
                if proto_version != PROTOCOL_VERSION {
                    eprintln!(
                        "engine: protocol version mismatch (front-end {}, engine {})",
                        proto_version, PROTOCOL_VERSION
                    );
                    std::process::exit(1);
                }
                // Restore the session environment.
                #[allow(
                    clippy::disallowed_methods,
                    reason = "engine cwd restore during Attach — sets engine process cwd, not Shell logical cwd"
                )]
                if let Err(e) = std::env::set_current_dir(&cwd) {
                    eprintln!("engine: failed to set cwd to {}: {e}", cwd.display());
                }
                // SAFETY: single-threaded engine startup, no other threads
                unsafe {
                    std::env::set_var("HOME", &home);
                }
                let _ = endpoint; // TODO Phase 2 Task 6: pass terminal fds via SCM_RIGHTS
                let _ = rc_path; // TODO: load rc_path with Shell::load_rc when available
            }
            Ok(Some(Frame::Detach)) | Ok(None) => {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Explicit);
                break;
            }
            Ok(Some(Frame::Event(..))) => {
                eprintln!("engine: unexpected Event frame");
            }
            Err(e) => {
                eprintln!("engine: read error: {e}");
                break;
            }
        }
    }
    std::process::exit(0);
}
