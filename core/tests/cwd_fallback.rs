#![allow(clippy::disallowed_methods)]

//! A shell is constructed at a working directory, and the process cwd is one
//! only while the OS can name it: a removed directory gives none, so a front
//! end fails to boot rather than read relative paths against nothing.

use ral_core::sync::LockExt as _;
use std::sync::Mutex;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_shell_rooted_at_the_process_cwd_stands_there() {
    let _guard = CWD_LOCK.lock_ignore_poison();
    let shell = ral_core::test_helper::core_shell();
    assert_eq!(shell.cwd(), std::env::current_dir().unwrap());
}

#[cfg(unix)]
#[test]
fn a_removed_process_cwd_is_no_working_directory() {
    let _guard = CWD_LOCK.lock_ignore_poison();
    let orig = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    drop(dir);

    let cwd = ral_core::host::cwd();

    std::env::set_current_dir(&orig).unwrap();
    assert_eq!(cwd, None);
}
