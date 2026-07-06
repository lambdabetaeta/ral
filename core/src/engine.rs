//! Engine process: the session-lived child that holds the Shell
//! and executes turns sent over a framed socket by the front-end.

use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;

use crate::serial::FOValue;
use crate::transport::{Control, DispatchId, Event, Frame, Turn};
use crate::types::{DeferredSink, Shell, SurfaceSink};
use crate::wire::WireChannel;

use std::sync::{Arc, Mutex, mpsc};

/// A surface sink that writes Event::Surface frames live to the wire
/// as values are produced, rather than buffering.
struct ChannelSurfaceSink {
    id: crate::transport::DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::EventSink for ChannelSurfaceSink {
    fn emit(&self, ev: &FOValue) {
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(self.id, Event::Surface(ev.clone())));
    }
}

/// A deferred sink that writes Event::DeferredSurface frames to the wire.
struct ChannelDeferredSink {
    id: DispatchId,
    writer: Arc<Mutex<WireChannel>>,
}

impl crate::types::DeferredSink for ChannelDeferredSink {
    fn deliver(&self, batch: Vec<FOValue>) {
        let _ = self
            .writer
            .lock()
            .unwrap()
            .write_frame(&Frame::Event(self.id, Event::DeferredSurface(batch)));
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
            // The live handles this turn runs under, joined with the protocol
            // `Turn` in the request the engine door runs.  Phase A installs
            // no desk on the wire engine.
            let req = crate::driver::TurnRequest {
                turn,
                surface: Some(Arc::new(ChannelSurfaceSink {
                    id,
                    writer: worker_writer.clone(),
                }) as SurfaceSink),
                deferred: Some(Arc::new(ChannelDeferredSink {
                    id,
                    writer: worker_writer.clone(),
                }) as Arc<dyn DeferredSink>),
                desk: None,
                lifecycle: Box::new(()),
            };
            let report = shell.run_turn(req).into_report();

            // Send the terminal Report frame.
            let _ = worker_writer
                .lock()
                .unwrap()
                .write_frame(&Frame::Event(id, Event::Report(report)));
        }
    });

    // ── Reader loop (this thread) ──────────────────────────────────
    let mut reader_ch = reader_ch;
    loop {
        match reader_ch.read_frame() {
            Ok(Some(Frame::Dispatch(id, turn))) => {
                match turn_tx.try_send((id, *turn)) {
                    Ok(()) => { /* worker accepted the turn */ }
                    Err(mpsc::TrySendError::Full(_)) => {
                        // Worker is busy — reply with a static diagnostic.
                        let report = crate::transport::Report::Static {
                            diagnostics: crate::transport::Diagnostics::Host(
                                "engine busy".into(),
                            ),
                        };
                        let _ = writer
                            .lock()
                            .unwrap()
                            .write_frame(&Frame::Event(id, Event::Report(report)));
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
