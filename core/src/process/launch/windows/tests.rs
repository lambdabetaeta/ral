use super::*;
use crate::process::Placement;

mod inheritance;

/// Only `CreateProcessW`'s own failure is the program's: a batch image and a
/// NUL in an argument are ral's refusals.
#[test]
fn only_create_process_s_failure_is_the_program_s() {
    let refused = |launch: Launch| {
        matches!(
            launch.spawn(
                Placement::Inherit,
                &crate::process::DurableRoot::default().child()
            ),
            Err(Unstarted::Refused(_))
        )
    };
    assert!(refused(Launch::new(r"C:\ral-no-such-dir\tool.bat")));
    let mut nul = Launch::new(r"C:\Windows\System32\cmd.exe");
    nul.arg("a\0b");
    assert!(refused(nul));
    assert!(matches!(
        Launch::new(r"C:\ral-no-such-dir\tool.exe").spawn(
            Placement::Inherit,
            &crate::process::DurableRoot::default().child()
        ),
        Err(Unstarted::Program(_))
    ));
}

#[test]
fn environment_nul_facts_name_the_key_not_the_secret_value() {
    for (key, value, subject) in [
        (
            "BAD\0KEY",
            "value",
            PrepareSubject::EnvironmentKey(crate::text::quote_native(OsStr::new("BAD\0KEY"))),
        ),
        (
            "RAL_NUL_TEST",
            "secret\0value",
            PrepareSubject::EnvironmentValue(crate::text::quote_native(OsStr::new("RAL_NUL_TEST"))),
        ),
    ] {
        let edits = [(
            super::super::env_key(OsStr::new(key)),
            EnvEdit::Set(value.into()),
        )]
        .into();
        let error = environment_block(&edits).unwrap_err();
        assert_eq!(error.step, PrepareStep::Environment);
        assert_eq!(error.subject, subject);
        assert_eq!(error.cause, PrepareCause::Invalid(PrepareInvalid::Nul));
    }
}
