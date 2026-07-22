//! The one line tauri asks of every host crate: read `tauri.conf.json`,
//! embed the static `ui/` directory, and generate the context the binary
//! links against.  Synod's frontend is hand-written HTML/CSS/JS with no
//! bundler, so there is nothing here to compile first — the directory is
//! the build product.

fn main() {
    tauri_build::build();
}
