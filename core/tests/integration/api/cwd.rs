//! A shell is constructed at a working directory, and the process cwd is one
//! only while the OS can name it: a removed directory gives none, so a front
//! end fails to boot rather than read relative paths against nothing.  The
//! test that changes the process cwd is isolated, as the cwd is shared by
//! every test in the binary.

#[test]
fn a_shell_rooted_at_the_process_cwd_stands_there() {
    let shell = ral_core::test_helper::core_shell();
    assert_eq!(shell.cwd(), std::env::current_dir().unwrap());
}

#[cfg(unix)]
#[test]
fn a_removed_process_cwd_is_no_working_directory() {
    ral_core::isolated!(a_removed_process_cwd_is_no_working_directory, || {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        drop(dir);
        assert_eq!(ral_core::host::cwd(), None);
    });
}
