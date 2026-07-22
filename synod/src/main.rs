//! The `synod` binary: a thin shell over [`synod::run`].

fn main() {
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
    if let Err(e) = synod::run() {
        eprintln!("synod: {e}");
        std::process::exit(1);
    }
}
