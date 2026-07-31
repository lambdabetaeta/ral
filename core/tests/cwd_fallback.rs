#![allow(clippy::disallowed_methods)]

//! Regression: an unseeded [`Shell::cwd`] reads the process cwd, and a
//! failed `getcwd(3)` answers `"."`, which fails closed downstream.

use ral_core::types::Shell;
use std::sync::Mutex;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn unseeded_shell_falls_back_to_the_process_cwd() {
    let _guard = CWD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let shell = Shell::default();
    assert_eq!(shell.cwd(), std::env::current_dir().unwrap());
}

#[cfg(unix)]
#[test]
fn deleted_process_cwd_falls_back_to_dot() {
    let _guard = CWD_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let orig = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    drop(dir);

    let shell = Shell::default();
    let cwd = shell.cwd();

    std::env::set_current_dir(&orig).unwrap();
    assert_eq!(cwd, std::path::PathBuf::from("."));
}
