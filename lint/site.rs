// Typed sites: the one way a banned call is exempted from
// `clippy::disallowed_methods`.
//
// A site wraps exactly one call, in one of two shapes:
//
//     site!(Silent::Variant, callee::path(args))
//     site!(Silent::Variant, receiver => method(args))
//
// The first argument is a value of a type implementing `Site`; the crate's
// enums name its sites, and a variant's doc says what the place is and why it
// is exempt.  The receiver and every argument are evaluated outside the
// exemption, so a banned call nested in an argument still fires.  The
// exemption is `#[expect]`, so a site that no longer wraps a banned call is
// reported as `unfulfilled_lint_expectations`.
//
// Each crate includes this file from a module named `site` at its crate root
// (the macro addresses `$crate::site::…`).  The macro is crate-local, with no
// `#[macro_export]`: an `#[expect]` placed by a macro from another crate is
// never reported as unfulfilled.

pub(crate) trait Site: Copy {}

macro_rules! site {
    // The `@` rules are internal and come first, so that an `@…` invocation
    // never reaches an `expr` fragment of the public rules below.

    // Split the call tokens into callee path tokens and the final `( args )`
    // group.
    (@path [$($f:tt)+] ( $($a:tt)* )) => {
        $crate::site::site!(@bind path [] [$($f)+] ; $($a)*)
    };
    (@path [$($f:tt)*] $next:ident $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* $next] $($rest)*)
    };
    (@path [$($f:tt)*] :: $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* ::] $($rest)*)
    };
    (@path [$($f:tt)*] < $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* <] $($rest)*)
    };
    (@path [$($f:tt)*] > $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* >] $($rest)*)
    };
    (@path [$($f:tt)*] >> $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* >>] $($rest)*)
    };
    (@path [$($f:tt)*] & $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* &] $($rest)*)
    };
    (@path [$($f:tt)*] , $($rest:tt)*) => {
        $crate::site::site!(@path [$($f)* ,] $($rest)*)
    };
    (@path [$($f:tt)*] $($rest:tt)*) => {
        compile_error!(
            "site!: expected `callee::path(args)` or `receiver => method(args)` and nothing else; \
             split method chains and nested calls into separate statements"
        )
    };

    // Bind receiver/arguments one by one, left to right, each in a nested
    // `match`, then emit the single attributed call.  The attribute sits on
    // the block's tail expression, not on a `let`, so a diverging callee
    // (`std::process::exit`) leaves no unreachable binding behind.
    (@bind path [$($b:ident),*] [$($f:tt)+] ;) => {{
        #[expect(
            clippy::disallowed_methods,
            reason = "a typed site: its variant names the place and says why"
        )]
        $($f)+($($b),*)
    }};
    (@bind method [$r:ident $(, $b:ident)*] [$m:ident] ;) => {{
        #[expect(
            clippy::disallowed_methods,
            reason = "a typed site: its variant names the place and says why"
        )]
        $r.$m($($b),*)
    }};
    (@bind $kind:ident [$($b:ident),*] [$($info:tt)+] ; $head:expr $(, $rest:expr)* $(,)?) => {
        match $head {
            tmp => $crate::site::site!(@bind $kind [$($b,)* tmp] [$($info)+] ; $($rest),*)
        }
    };

    ($site:expr, $recv:expr => $m:ident ( $($a:tt)* )) => {{
        fn check<S: $crate::site::Site>(_: S) {}
        check($site);
        $crate::site::site!(@bind method [] [$m] ; $recv, $($a)*)
    }};
    ($site:expr, $($call:tt)+) => {{
        fn check<S: $crate::site::Site>(_: S) {}
        check($site);
        $crate::site::site!(@path [] $($call)+)
    }};
}
pub(crate) use site;
