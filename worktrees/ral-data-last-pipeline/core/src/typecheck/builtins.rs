//! Hardcoded type schemes for ral builtins.
//!
//! `builtin_scheme` allocates fresh unifier slots directly, so the returned
//! scheme can be stored in the env or used at a call site without any
//! post-processing renaming step.

use super::fmt::fmt_scheme;
use super::scheme::{CachedFreeVars, Scheme};
use super::ty::{CompTy, ModeVar, PipeMode, PipeSpec, Row, RowVar, Ty, TyVar};
use super::unify::Unifier;

pub fn thunk(cty: CompTy) -> Ty {
    Ty::Thunk(Box::new(cty))
}
pub fn fun(param: Ty, body: CompTy) -> CompTy {
    CompTy::Fun(Box::new(param), Box::new(body))
}
pub fn pure(ty: Ty) -> CompTy {
    CompTy::pure(ty)
}

/// Build a closed record type from a list of (label, type) pairs.
pub fn closed_record(fields: &[(&str, Ty)]) -> Ty {
    let mut row = Row::Empty;
    for (l, t) in fields.iter().rev() {
        row = Row::Extend(l.to_string(), Box::new(t.clone()), Box::new(row));
    }
    Ty::Record(row)
}

/// The `{ok, value, status, cmd, message, stdout, line, col}` record
/// returned by `_try` / `try`.  `message` carries the failure text —
/// runtime errors' own message, or the failing external command's
/// stderr decoded as UTF-8.  `stdout` is the body's fd 1 capture as
/// `Bytes`; a body that prints before failing leaves those bytes here
/// rather than on the terminal.  Use `await`'s `stderr: Bytes` field
/// if you need the raw fd 2 of a captured task.
fn try_error_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("ok", Ty::Bool),
        ("value", value_ty),
        ("status", Ty::Int),
        ("cmd", Ty::String),
        ("message", Ty::String),
        ("stdout", Ty::Bytes),
        ("line", Ty::Int),
        ("col", Ty::Int),
    ])
}

/// The `{ value, stdout, stderr, status }` record returned by `await`/`race`.
/// The block's return type α flows into `value`; stdout and stderr are the
/// raw byte buffers; status is the POSIX exit code (0 on success).  Failure
/// is signalled by raising, not by an `ok` flag — wrap in `try` to recover.
fn await_record(value_ty: Ty) -> Ty {
    closed_record(&[
        ("value", value_ty),
        ("stdout", Ty::Bytes),
        ("stderr", Ty::Bytes),
        ("status", Ty::Int),
    ])
}

/// The record type returned by `_fs list` for each directory entry.
pub fn fs_list_entry_ty() -> Ty {
    closed_record(&[
        ("name", Ty::String),
        ("type", Ty::String),
        ("size", Ty::Int),
        ("mtime", Ty::Int),
    ])
}

/// Return a polymorphic type scheme for a builtin executable by name.
///
/// Fresh type/mode/row variables are allocated directly from `u`, so the
/// returned scheme can be stored in the environment or used at a call site
/// without any post-processing renaming step.
pub fn builtin_scheme(name: &str, u: &mut Unifier) -> Option<Scheme> {
    // Allocate a pool of fresh template vars.  Most schemes need at most two
    // type vars (α, β) and two mode vars (μ₀, μ₁); all four are allocated
    // unconditionally so every match arm can freely use any subset.
    let (av, bv) = (u.fresh_tyvar(), u.fresh_tyvar());
    let (m0, m1) = (u.fresh_modevar(), u.fresh_modevar());
    let a = || Ty::Var(av);
    let b = || Ty::Var(bv);

    let mk = |ty_vars: &[TyVar], mode_vars: &[ModeVar], row_vars: &[RowVar], ty: Ty| Scheme {
        ty_vars: ty_vars.to_vec(),
        mode_vars: mode_vars.to_vec(),
        row_vars: row_vars.to_vec(),
        ty,
        cached_fv: Some(CachedFreeVars::default()),
    };
    // F[μ₀,μ₁] A  — used for branch/lastthunk returns with fresh modes
    let fm = |ty: Ty| {
        CompTy::Return(
            PipeSpec {
                input: PipeMode::Var(m0),
                output: PipeMode::Var(m1),
            },
            Box::new(ty),
        )
    };

    Some(match name {
        // ── List operations ──────────────────────────────────────────────────
        "length" =>
        // ∀α. α → F Int  (accepts list, string, or any collection)
        {
            mk(&[av], &[], &[], thunk(fun(a(), pure(Ty::Int))))
        }
        "keys" =>
        // ∀α. [Str:α] → F [Str]
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    Ty::Map(Box::new(a())),
                    pure(Ty::List(Box::new(Ty::String))),
                )),
            )
        }
        "values" =>
        // ∀α. [Str:α] → F [α]
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(Ty::Map(Box::new(a())), pure(Ty::List(Box::new(a()))))),
            )
        }
        "has" | "equal" | "lt" | "gt" =>
        // ∀α β. α → β → F Bool
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(a(), fun(b(), pure(Ty::Bool)))),
            )
        }

        // ── Collection builtins (called via _map, _filter, _fold, _each) ────
        "map" =>
        // ∀α β. U(α → F β) → [α] → F [β]
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), pure(b()))),
                    fun(Ty::List(Box::new(a())), pure(Ty::List(Box::new(b())))),
                )),
            )
        }
        "filter" =>
        // ∀α. U(α → F Bool) → [α] → F [α]
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), pure(Ty::Bool))),
                    fun(Ty::List(Box::new(a())), pure(Ty::List(Box::new(a())))),
                )),
            )
        }
        "_each" =>
        // ∀α β. [α] → U(α → F β) → F Unit
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    Ty::List(Box::new(a())),
                    fun(thunk(fun(a(), pure(b()))), pure(Ty::Unit)),
                )),
            )
        }
        "_fold" =>
        // ∀α β. [α] → β → U(β → α → F β) → F β
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    Ty::List(Box::new(a())),
                    fun(b(), fun(thunk(fun(b(), fun(a(), pure(b())))), pure(b()))),
                )),
            )
        }
        "sort-list" =>
        // ∀α. [α] → F [α]
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(Ty::List(Box::new(a())), pure(Ty::List(Box::new(a()))))),
            )
        }
        "sort-list-by" =>
        // ∀α β. U(α → F β) → [α] → F [α]
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), pure(b()))),
                    fun(Ty::List(Box::new(a())), pure(Ty::List(Box::new(a())))),
                )),
            )
        }

        // ── Error handling ───────────────────────────────────────────────────
        "_try" =>
        // ∀α. U(F α) → F {ok:Bool, value:α, status:Int, cmd:Str, stderr:Bytes, line:Int, col:Int}
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(thunk(pure(a())), pure(try_error_record(a())))),
            )
        }

        "_try-apply" =>
        // ∀α β. U(α → F β) → α → F {ok:Bool, value:β}
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), pure(b()))),
                    fun(
                        a(),
                        pure(closed_record(&[("ok", Ty::Bool), ("value", b())])),
                    ),
                )),
            )
        }

        "try" =>
        // ∀α. U(F α) → U(error(α) → F α) → F α
        // Both body and handler must return the same type: on success
        // the body's value is returned, on failure the handler's is.
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    thunk(pure(a())),
                    fun(thunk(fun(try_error_record(a()), pure(a()))), pure(a())),
                )),
            )
        }
        "audit" =>
        // ∀α β. U(F α) → F {kind:Str, cmd:Str, args:[Str], status:Int, script:Str, line:Int,
        //                    col:Int, stdout:Bytes, stderr:Bytes, value:α,
        //                    children:[[Str:β]], start:Int, end:Int, principal:Str}
        // stdout/stderr are stored as raw bytes; `ral --audit` renders them
        // as lossy UTF-8 in its JSON output.
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    thunk(pure(a())),
                    pure(closed_record(&[
                        ("kind", Ty::String),
                        ("cmd", Ty::String),
                        ("args", Ty::List(Box::new(Ty::String))),
                        ("status", Ty::Int),
                        ("script", Ty::String),
                        ("line", Ty::Int),
                        ("col", Ty::Int),
                        ("stdout", Ty::Bytes),
                        ("stderr", Ty::Bytes),
                        ("value", a()),
                        ("children", Ty::List(Box::new(Ty::Map(Box::new(b()))))),
                        ("start", Ty::Int),
                        ("end", Ty::Int),
                        ("principal", Ty::String),
                    ])),
                )),
            )
        }
        "guard" =>
        // ∀α β. U(F α) → U(F β) → F α
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(thunk(pure(a())), fun(thunk(pure(b())), pure(a())))),
            )
        }

        // ── String / conversion ──────────────────────────────────────────────
        "_convert" =>
        // ∀α β. Str → α → F β  (op name first, then value)
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(Ty::String, fun(a(), pure(b())))),
            )
        }
        "upper" | "lower" | "dedent" | "shell-quote" =>
        // Str → F Str
        {
            mk(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::String))))
        }
        "shell-split" =>
        // Str → F [Str]
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(Ty::String, pure(Ty::List(Box::new(Ty::String))))),
            )
        }
        "match" =>
        // Str → Str → F Bool
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(Ty::String, fun(Ty::String, pure(Ty::Bool)))),
            )
        }
        "find-match" =>
        // Str → Str → F Str
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(Ty::String, fun(Ty::String, pure(Ty::String)))),
            )
        }
        "split" | "find-matches" =>
        // Str → Str → F [Str]
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(Ty::String, pure(Ty::List(Box::new(Ty::String)))),
                )),
            )
        }
        "replace" | "replace-all" =>
        // Str → Str → Str → F Str
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(Ty::String, fun(Ty::String, pure(Ty::String))),
                )),
            )
        }
        "slice" =>
        // Str → Int → Int → F Str
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(Ty::Int, fun(Ty::Int, pure(Ty::String))),
                )),
            )
        }
        "intercalate" =>
        // ∀α. Str → [α] → F Str
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(Ty::List(Box::new(a())), pure(Ty::String)),
                )),
            )
        }

        // ── File system ──────────────────────────────────────────────────────
        "glob" =>
        // Str → F [Str]
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(Ty::String, pure(Ty::List(Box::new(Ty::String))))),
            )
        }
        "exists" | "is-file" | "is-dir" | "is-link" | "is-readable" | "is-writable" =>
        // Str → F Bool
        {
            mk(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::Bool))))
        }
        "is-empty" =>
        // ∀α. α → F Bool  (accepts list, map, string, or path)
        {
            mk(&[av], &[], &[], thunk(fun(a(), pure(Ty::Bool))))
        }
        "which" =>
        // Str → F Str
        {
            mk(&[], &[], &[], thunk(fun(Ty::String, pure(Ty::String))))
        }

        // ── Line-by-line streaming ────────────────────────────────────────────
        "fold-lines" =>
        // ∀α. U(α → Str → F α) → α → F[Bytes,∅] α
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), fun(Ty::String, pure(a())))),
                    fun(a(), CompTy::Return(PipeSpec::decode(), Box::new(a()))),
                )),
            )
        }

        // ── Concurrency ──────────────────────────────────────────────────────
        "spawn" =>
        // ∀α μ₀ μ₁. U(F[μ₀,μ₁] α) → F (Handle α)
        // Body may use any pipeline modes — bytes, values, or pure.
        {
            mk(
                &[av],
                &[m0, m1],
                &[],
                thunk(fun(
                    thunk(fm(a())),
                    pure(Ty::Handle(Box::new(a()))),
                )),
            )
        }
        "watch" =>
        // ∀α μ₀ μ₁. String → U(F[μ₀,μ₁] α) → F (Handle α)
        {
            mk(
                &[av],
                &[m0, m1],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(thunk(fm(a())), pure(Ty::Handle(Box::new(a())))),
                )),
            )
        }
        "await" =>
        // ∀α. Handle α → F { value: α, stdout: Bytes, stderr: Bytes, status: Int }
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(Ty::Handle(Box::new(a())), pure(await_record(a())))),
            )
        }
        "race" =>
        // ∀α. [Handle α] → F { value: α, stdout: Bytes, stderr: Bytes, status: Int }
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(
                    Ty::List(Box::new(Ty::Handle(Box::new(a())))),
                    pure(await_record(a())),
                )),
            )
        }
        "cancel" | "disown" =>
        // ∀α. Handle α → F Unit
        {
            mk(
                &[av],
                &[],
                &[],
                thunk(fun(Ty::Handle(Box::new(a())), pure(Ty::Unit))),
            )
        }
        "par" =>
        // ∀α β. U(α → F β) → [α] → Int → F [β]
        {
            mk(
                &[av, bv],
                &[],
                &[],
                thunk(fun(
                    thunk(fun(a(), pure(b()))),
                    fun(
                        Ty::List(Box::new(a())),
                        fun(Ty::Int, pure(Ty::List(Box::new(b())))),
                    ),
                )),
            )
        }

        // ── Arithmetic/exit ──────────────────────────────────────────────────
        "exit" | "quit" => mk(&[], &[], &[], thunk(fun(Ty::Int, pure(Ty::Unit)))),
        "fail" =>
        // ∀α ρ. [status: Int | ρ] → F α
        // Always diverges; result type unconstrained.  Argument is an open
        // record requiring at least `status: Int`; other fields (`message`,
        // …) are accepted by the row-tail variable so a caught error record
        // can be re-raised verbatim.  Literal `fail [status: 0]` is rejected
        // at runtime; `fail 0` (Int) is now a static type error.
        {
            let rho = u.fresh_row_var();
            let arg = Ty::Record(Row::Extend(
                "status".into(),
                Box::new(Ty::Int),
                Box::new(Row::Var(rho)),
            ));
            mk(&[av], &[], &[rho], thunk(fun(arg, pure(a()))))
        }

        // ── Content search ───────────────────────────────────────────────────
        "grep-files" =>
        // Str → [Str] → F [[file:Str, line:Int, text:Str]]
        {
            mk(
                &[],
                &[],
                &[],
                thunk(fun(
                    Ty::String,
                    fun(
                        Ty::List(Box::new(Ty::String)),
                        pure(Ty::List(Box::new(closed_record(&[
                            ("file", Ty::String),
                            ("line", Ty::Int),
                            ("text", Ty::String),
                        ])))),
                    ),
                )),
            )
        }

        // ── Scoping primitives ───────────────────────────────────────────────
        _ => return None,
    })
}

/// Return the formatted type string for a builtin, or `None` if unknown.
pub fn builtin_type_hint(name: &str) -> Option<String> {
    let mut u = Unifier::new();
    let scheme = builtin_scheme(name, &mut u)?;
    Some(fmt_scheme(&scheme))
}

/// Number of value arguments the builtin's scheme declares (count of nested
/// `Fun` layers under the outer `Thunk`).  Used to η-expand first-class
/// builtin references (`$upper`) into curried lambda thunks.  `None` for
/// builtins without a scheme — typically variadic ones like `echo`.
pub fn builtin_arity(name: &str) -> Option<usize> {
    let mut u = Unifier::new();
    let scheme = builtin_scheme(name, &mut u)?;
    fn count(ct: &CompTy) -> usize {
        match ct {
            CompTy::Fun(_, body) => 1 + count(body),
            _ => 0,
        }
    }
    match &scheme.ty {
        Ty::Thunk(inner) => Some(count(inner)),
        _ => Some(0),
    }
}

/// A per-key type schema — `fn(key, unifier) -> Option<Ty>`.
///
/// Drives [`super::infer::Inferencer::check_map_entry_fields`].  Returning
/// `None` for a key leaves that entry runtime-dispatched (still inferred
/// for side-effects, but not unified against anything).
pub type FieldSchema = fn(&str, &mut Unifier) -> Option<Ty>;

/// Schema for the `within [env:, dir:]` options map.  `handlers`/`handler`
/// are thunk-typed, so they're handled by the `LastThunk` path instead.
pub fn within_field_ty(key: &str, u: &mut Unifier) -> Option<Ty> {
    match key {
        "env" => Some(Ty::Map(Box::new(u.fresh_ty()))),
        "dir" => Some(Ty::String),
        _ => None,
    }
}

/// Schema for the `grant [exec:, fs:, net:, editor:, env:, audit:]` map.
pub fn grant_field_ty(key: &str, _u: &mut Unifier) -> Option<Ty> {
    let bool_map = || Ty::Map(Box::new(Ty::Bool));
    match key {
        "exec" | "fs" => Some(Ty::Map(Box::new(Ty::List(Box::new(Ty::String))))),
        "net" | "audit" => Some(Ty::Bool),
        "editor" | "shell" => Some(bool_map()),
        _ => None,
    }
}

/// Schema for rc plugin entries `[plugin: Str, options: Map]`.
pub fn plugin_entry_field_ty(key: &str, u: &mut Unifier) -> Option<Ty> {
    match key {
        "plugin" => Some(Ty::String),
        "options" => Some(Ty::Map(Box::new(u.fresh_ty()))),
        _ => None,
    }
}

/// Which field-schema applies to a `HeadKind::LastThunk` call — `within` or
/// `grant`.  `None` for other names: no options map to validate.
pub fn scoping_schema(name: &str) -> Option<FieldSchema> {
    match name {
        "within" => Some(within_field_ty),
        "grant" => Some(grant_field_ty),
        _ => None,
    }
}

/// Positional-arg spec for `_plugin <op> <args...>`.  `None` for unknown ops:
/// left to runtime.
pub fn plugin_op_arg_spec(op: &str, u: &mut Unifier) -> Option<Vec<(Ty, &'static str)>> {
    match op {
        "load" => Some(vec![
            (Ty::String, "_plugin 'load' name: expected a String"),
            (
                Ty::Map(Box::new(u.fresh_ty())),
                "_plugin 'load' options: expected a Map",
            ),
        ]),
        "unload" => Some(vec![(
            Ty::String,
            "_plugin 'unload' name: expected a String",
        )]),
        _ => None,
    }
}
