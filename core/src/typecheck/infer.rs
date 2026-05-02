//! Type inference: infer_val, infer_comp, and supporting helpers.
//!
//! `infer_val` synthesizes a value type (Ty) for a Val node.
//! `infer_comp` synthesizes a computation type (CompTy) for a Comp node.
//! Both are mutually recursive: thunk bodies are inferred as computations,
//! and return values are inferred as values.

use super::builtins::{
    FieldSchema, builtin_scheme, plugin_entry_field_ty, plugin_op_arg_spec, scoping_schema,
};
use super::env::{InferCtx, TyEnv};
use super::fmt::fmt_ty;
use super::generalize::{generalize, instantiate};
use super::scheme::Scheme;
use super::ty::{CompTy, PipeMode, PipeSpec, Row, Ty};
use crate::step::{DONE_TAG, HEAD_FIELD, MORE_TAG, TAIL_FIELD};
use crate::ast::{ExprOp, Pattern};
use crate::classify::{HeadKind, head_kind};
use crate::ir::{Comp, CompKind, ExecName, Val, ValListElem, ValMapEntry};

/// Walk a row spine and collect (label, payload_ty) pairs in source order,
/// stopping at the first non-Extend node (Empty or unresolved variable).
/// Caller is expected to have applied substitutions; later duplicates of
/// the same label are skipped per scoped-label semantics.
fn collect_extends(row: &Row) -> Vec<(String, Ty)> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = row;
    loop {
        match cur {
            Row::Extend(l, ty, rest) => {
                if seen.insert(l.clone()) {
                    out.push((l.clone(), (**ty).clone()));
                }
                cur = rest;
            }
            _ => return out,
        }
    }
}

fn literal_ty(s: &str) -> Ty {
    match s {
        "true" | "false" => Ty::Bool,
        "unit" => Ty::Unit,
        _ if s.parse::<i64>().is_ok() => Ty::Int,
        _ if s.parse::<f64>().is_ok() => Ty::Float,
        _ => Ty::String,
    }
}

pub fn infer_comp(ctx: &mut InferCtx, env: &mut TyEnv, comp: &Comp) -> CompTy {
    Inferencer { ctx, env }.infer_comp(comp)
}

struct Inferencer<'a> {
    ctx: &'a mut InferCtx,
    env: &'a mut TyEnv,
}

enum StepProbe {
    NotStep,
    Step(Ty),
    Invalid(&'static str),
}

impl Inferencer<'_> {
    fn with_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved_pos = self.ctx.pos;
        self.env.push();
        let out = f(self);
        self.env.pop();
        self.ctx.pos = saved_pos;
        out
    }

    fn bind_pattern(&mut self, pat: &Pattern, ty: &Ty) {
        match pat {
            Pattern::Wildcard => {}
            Pattern::Name(name) => {
                self.env.bind(name.clone(), Scheme::mono(ty.clone()));
            }
            Pattern::List { elems, rest } => {
                let elem = self.ctx.unifier.fresh_ty();
                self.ctx.unify_ty(ty, &Ty::List(Box::new(elem.clone())));
                for elem_pat in elems {
                    self.bind_pattern(elem_pat, &elem);
                }
                if let Some(rest_name) = rest {
                    self.env
                        .bind(rest_name.clone(), Scheme::mono(Ty::List(Box::new(elem))));
                }
            }
            Pattern::Map(entries) => {
                let tail = self.ctx.unifier.fresh_row_var();
                let mut row = Row::Var(tail);
                let mut field_tys = Vec::with_capacity(entries.len());
                for (key, _, _) in entries.iter().rev() {
                    let field_ty = self.ctx.unifier.fresh_ty();
                    field_tys.push(field_ty.clone());
                    row = Row::Extend(key.clone(), Box::new(field_ty), Box::new(row));
                }
                field_tys.reverse();
                self.ctx.unify_ty(ty, &Ty::Record(row));
                for ((_, subpattern, _), field_ty) in entries.iter().zip(field_tys.iter()) {
                    self.bind_pattern(subpattern, field_ty);
                }
            }
        }
    }

    fn extract_return(&mut self, cty: &CompTy) -> (Ty, PipeMode, PipeMode) {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(spec, ty) => (*ty, spec.input, spec.output),
            _ => {
                let ty = self.ctx.unifier.fresh_ty();
                let input = self.ctx.unifier.fresh_mode();
                let output = self.ctx.unifier.fresh_mode();
                let expected = CompTy::Return(
                    PipeSpec {
                        input: input.clone(),
                        output: output.clone(),
                    },
                    Box::new(ty.clone()),
                );
                self.ctx.unify_comp_ty(cty, &expected);
                (ty, input, output)
            }
        }
    }

    /// Project the I/O end (input or output) of a computation type, peering
    /// past `Fun` arrows.  An unresolved comp var yields a fresh mode.
    fn comp_end_mode(&mut self, cty: &CompTy, pick: fn(PipeSpec) -> PipeMode) -> PipeMode {
        match self.ctx.unifier.resolve_comp_ty(cty) {
            CompTy::Return(spec, _) => pick(spec),
            CompTy::Fun(_, body) => self.comp_end_mode(&body, pick),
            CompTy::Var(_) => self.ctx.unifier.fresh_mode(),
        }
    }

    fn comp_input_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.input)
    }

    fn comp_output_mode(&mut self, cty: &CompTy) -> PipeMode {
        self.comp_end_mode(cty, |s| s.output)
    }

    fn autoderef_thunk_return(&mut self, mut cty: CompTy) -> CompTy {
        loop {
            match self.ctx.unifier.resolve_comp_ty(&cty) {
                CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                    Ty::Thunk(inner) => cty = *inner,
                    // Free type variable in head position: this matches the
                    // runtime behavior where eval_app's Thunk arm trampoline-
                    // forces a Thunk value before applying args.  At type
                    // level we constrain the head to be a Thunk and continue
                    // unfolding.  Without this, a parameter `$f` whose type
                    // is yet unknown would fail to unify when args are
                    // applied (the elaborator no longer emits a `Force`
                    // wrapper that used to plant this constraint).
                    Ty::Var(_) => {
                        let inner = self.ctx.unifier.fresh_comp_ty();
                        self.ctx
                            .unify_ty(&ty, &Ty::Thunk(Box::new(inner.clone())));
                        cty = inner;
                    }
                    _ => return cty,
                },
                _ => return cty,
            }
        }
    }

    fn apply_args(&mut self, mut cty: CompTy, args: &[Val]) -> CompTy {
        for arg in args {
            cty = self.autoderef_thunk_return(cty);
            let arg_ty = self.infer_val(arg);
            let result = self.ctx.unifier.fresh_comp_ty();
            let expected = CompTy::Fun(Box::new(arg_ty.clone()), Box::new(result.clone()));
            self.ctx.unify_comp_ty_hint(
                &cty,
                &expected,
                "this argument's type does not match the function's parameter",
            );
            cty = result;
        }
        cty
    }

    /// If `head_ty` resolves to a `Return(_, ty)` where `ty` is concretely
    /// non-callable — i.e. not a `Thunk` and not a free type variable that
    /// could later become one — return that `ty`.  Otherwise return `None`.
    ///
    /// Used by `CompKind::App` to detect `'foo' bar baz` and friends and
    /// raise a surface-level diagnostic before the general unifier
    /// mismatch fires.
    fn head_non_callable_ty(&mut self, head_ty: &CompTy) -> Option<Ty> {
        match self.ctx.unifier.resolve_comp_ty(head_ty) {
            CompTy::Return(_, ty) => match self.ctx.unifier.resolve_ty(&ty) {
                Ty::Thunk(_) | Ty::Var(_) => None,
                concrete => Some(concrete),
            },
            CompTy::Fun(_, _) | CompTy::Var(_) => None,
        }
    }

    fn apply_piped_value(&mut self, cty: CompTy, piped_ty: Ty) -> CompTy {
        // Step-shaped output flowing into a function consumer is iterated
        // element-by-element by the runtime (see `evaluator/invoke.rs`).
        // The typechecker mirrors that: when the producer's value type
        // unifies with `[.more: {head: τ, tail: Thunk(_)} | .done: _ | ρ]`,
        // we propagate `τ` rather than the whole variant, so the consumer
        // is checked against the element type.
        let piped_ty = match self.step_probe(&piped_ty) {
            StepProbe::Step(elem) => elem,
            StepProbe::NotStep => piped_ty,
            StepProbe::Invalid(msg) => {
                self.ctx.error_hint(
                    "invalid Step value in pipeline".to_string(),
                    &format!("{msg}; expected .more {{head, tail: Block}} or .done"),
                );
                piped_ty
            }
        };
        let cty = self.autoderef_thunk_return(cty);
        let result = self.ctx.unifier.fresh_comp_ty();
        let expected = CompTy::Fun(Box::new(piped_ty), Box::new(result.clone()));
        // Failure here is a false positive in mixed-mode pipelines where a
        // byte-channel stage precedes a value-passing stage; suppress it and
        // let the pipeline mode unification in infer_pipeline catch the real error.
        let _ = self.ctx.unifier.unify_comp_ty(&cty, &expected);
        result
    }

    /// Probe whether `ty` matches the runtime Step protocol at a pipeline
    /// boundary.
    fn step_probe(&mut self, ty: &Ty) -> StepProbe {
        let resolved = self.ctx.unifier.apply_ty(ty);
        let row = match resolved.clone() {
            Ty::Variant(r) => r,
            _ => return StepProbe::NotStep,
        };
        let labels = collect_extends(&row);
        let more_payload = labels
            .iter()
            .find(|(l, _)| l == MORE_TAG)
            .map(|(_, t)| t.clone());
        if more_payload.is_none() {
            if labels.iter().any(|(l, _)| l == DONE_TAG) {
                return StepProbe::Step(self.ctx.unifier.fresh_ty());
            }
            return StepProbe::NotStep;
        }

        let payload = self.ctx.unifier.apply_ty(&more_payload.expect("checked is_some"));
        let payload_row = match payload {
            Ty::Record(r) => r,
            _ => return StepProbe::Invalid(".more payload must be a record"),
        };
        let payload_labels = collect_extends(&payload_row);
        let head_ty = payload_labels
            .iter()
            .find(|(l, _)| l == HEAD_FIELD)
            .map(|(_, t)| t.clone());
        let Some(head_ty) = head_ty else {
            return StepProbe::Invalid(".more payload missing head");
        };
        let tail_ty = payload_labels
            .iter()
            .find(|(l, _)| l == TAIL_FIELD)
            .map(|(_, t)| t.clone());
        let Some(tail_ty) = tail_ty else {
            return StepProbe::Invalid(".more payload missing tail");
        };
        let tail_cty = match self.ctx.unifier.apply_ty(&tail_ty) {
            Ty::Thunk(cty) => *cty,
            _ => return StepProbe::Invalid(".more tail must be a Block"),
        };
        let (tail_ret, _, _) = self.extract_return(&tail_cty);
        let tail_stepish = match self.ctx.unifier.apply_ty(&tail_ret) {
            Ty::Variant(r) => {
                let labels = collect_extends(&r);
                labels
                    .iter()
                    .any(|(l, _)| l == MORE_TAG || l == DONE_TAG)
            }
            _ => false,
        };
        if !tail_stepish {
            return StepProbe::Invalid(".more tail does not return a Step");
        }
        // Keep the recogniser honest: a Step node's tail must itself
        // produce a Step node of the same shape.
        if self.ctx.unifier.unify_ty(&tail_ret, &resolved).is_err() {
            return StepProbe::Invalid(".more tail Step shape does not match current node");
        }
        StepProbe::Step(head_ty)
    }

    /// The value shape returned by `from-lines`: a recursive Step stream
    /// of Strings, i.e. `.more {head: String, tail: Thunk(F Step)}` or
    /// `.done`.  The recursion closes through a comp-var root, not a TyVar.
    fn from_lines_step_ty(&mut self) -> Ty {
        let tail_comp = self.ctx.unifier.fresh_comp_ty();
        let payload = Ty::Record(Row::Extend(
            HEAD_FIELD.into(),
            Box::new(Ty::String),
            Box::new(Row::Extend(
                TAIL_FIELD.into(),
                Box::new(Ty::Thunk(Box::new(tail_comp.clone()))),
                Box::new(Row::Empty),
            )),
        ));
        let step = Ty::Variant(Row::Extend(
            MORE_TAG.into(),
            Box::new(payload),
            Box::new(Row::Extend(
                DONE_TAG.into(),
                Box::new(Ty::Unit),
                Box::new(Row::Empty),
            )),
        ));
        self.ctx
            .unify_comp_ty(&tail_comp, &CompTy::pure(step.clone()));
        step
    }

    fn infer_branches(&mut self, args: &[Val]) -> CompTy {
        let mut branch_tys = Vec::new();
        for arg in args {
            match arg {
                Val::Thunk(comp) => {
                    let ty = self.with_scope(|this| this.infer_comp(comp));
                    branch_tys.push(ty);
                }
                // Conditions and other non-thunk args still type-check for
                // side effects, but don't constrain the branches' return type.
                other => {
                    let _ = self.infer_val(other);
                }
            }
        }

        if branch_tys.is_empty() {
            return CompTy::pure(self.ctx.unifier.fresh_ty());
        }

        let first = branch_tys.remove(0);
        for ty in &branch_tys {
            self.ctx
                .unify_comp_ty_hint(&first, ty, "all branches must produce the same type");
        }
        first
    }

    /// Validate a map literal's entries against a per-key `schema`.
    ///
    /// For each entry, the value is inferred (so side-effects and inner
    /// type errors surface); additionally, if the key is a literal the
    /// `schema` knows, the value's type is unified against the expected
    /// one.  Unknown keys, spreads, and dynamic keys stay runtime-
    /// dispatched.  Shared by `within`, `grant`, and rc plugin entries —
    /// three shapes of the same "optional-args map" idiom.
    fn check_map_entry_fields(&mut self, entries: &[ValMapEntry], ctx: &str, schema: FieldSchema) {
        for entry in entries {
            let (key, val) = match entry {
                ValMapEntry::Entry(Val::Literal(k) | Val::String(k), v) => (Some(k.as_str()), v),
                ValMapEntry::Entry(_, v) | ValMapEntry::Spread(v) => (None, v),
            };
            let expected = key.and_then(|k| schema(k, &mut self.ctx.unifier));
            let actual = self.infer_val(val);
            if let (Some(key), Some(expected)) = (key, expected) {
                self.ctx.unify_ty_hint(
                    &actual,
                    &expected,
                    &format!("{ctx} {key}: wrong value type"),
                );
            }
        }
    }

    /// Infer an rc `plugins:` list: validate each literal-map entry against
    /// the plugin-entry schema, with no cross-entry unification so entries
    /// with mixed shapes coexist.  The list's element type is a fresh var.
    fn infer_plugins_list(&mut self, elems: &[ValListElem]) -> Ty {
        for elem in elems {
            match elem {
                ValListElem::Single(Val::Map(entries)) => {
                    self.check_map_entry_fields(entries, "plugin entry", plugin_entry_field_ty);
                }
                ValListElem::Single(v) => {
                    let _ = self.infer_val(v);
                }
                ValListElem::Spread(v) => {
                    let spread_ty = self.infer_val(v);
                    let inner = self.ctx.unifier.fresh_ty();
                    self.ctx.unify_ty(&spread_ty, &Ty::List(Box::new(inner)));
                }
            }
        }
        Ty::List(Box::new(self.ctx.unifier.fresh_ty()))
    }

    /// Unify each positional `arg` with the corresponding `(expected, hint)`
    /// from `spec`; infer any extras for side-effects.  Used by op-dispatched
    /// builtins where each op has a fixed arg shape.
    fn check_positional_args(&mut self, args: &[Val], spec: &[(Ty, &str)]) {
        for (arg, (expected, hint)) in args.iter().zip(spec) {
            let actual = self.infer_val(arg);
            self.ctx.unify_ty_hint(&actual, expected, hint);
        }
        for extra in args.iter().skip(spec.len()) {
            let _ = self.infer_val(extra);
        }
    }

    fn infer_last_thunk(&mut self, args: &[Val]) -> CompTy {
        for arg in args.iter().rev() {
            if let Val::Thunk(comp) = arg {
                return self.with_scope(|this| this.infer_comp(comp));
            }
        }
        CompTy::pure(self.ctx.unifier.fresh_ty())
    }

    fn infer_primop(&mut self, op: ExprOp, args: &[Val]) -> Ty {
        match op {
            ExprOp::Not => {
                debug_assert_eq!(args.len(), 1, "Not is unary");
                let ty = self.infer_val(&args[0]);
                self.ctx.unify_ty(&ty, &Ty::Bool);
                Ty::Bool
            }
            _ => {
                debug_assert_eq!(args.len(), 2, "binary op");
                let lhs = self.infer_val(&args[0]);
                let rhs = self.infer_val(&args[1]);
                self.ctx.unify_ty(&lhs, &rhs);
                match op {
                    ExprOp::Eq | ExprOp::Ne | ExprOp::Lt | ExprOp::Gt | ExprOp::Le | ExprOp::Ge => {
                        Ty::Bool
                    }
                    ExprOp::Not => unreachable!(),
                    _ => lhs,
                }
            }
        }
    }

    fn exec_comp_ty(&mut self, name: &str, args: &[Val], external_only: bool) -> CompTy {
        if matches!(name, "exit" | "quit") && args.is_empty() {
            return CompTy::pure(Ty::Unit);
        }

        if name == "fail" {
            // Detect literal `fail [status: 0]` (or any literal record whose
            // status field is the literal 0).  Bare `fail 0` is now a type
            // error caught by unification (Int vs Record).
            let zero_status = matches!(
                args.first(),
                Some(Val::Map(entries)) if entries.iter().any(|e| matches!(
                    e,
                    crate::ir::ValMapEntry::Entry(Val::Literal(k) | Val::String(k), Val::Int(0))
                        if k == "status"
                )),
            );
            if zero_status {
                self.ctx.error_hint(
                    "`fail [status: 0]` is not allowed — fail requires a nonzero status".into(),
                    "use `return` for a clean exit",
                );
            }
        }

        if name == "_fs"
            && let Some(Val::Literal(op) | Val::String(op)) = args.first()
            && op == "list"
        {
            if let Some(arg) = args.get(1) {
                let _ = self.infer_val(arg);
            }
            return CompTy::pure(Ty::List(Box::new(super::builtins::fs_list_entry_ty())));
        }

        if name == "_plugin"
            && let Some(Val::Literal(op) | Val::String(op)) = args.first()
            && let Some(spec) = plugin_op_arg_spec(op, &mut self.ctx.unifier)
        {
            self.check_positional_args(&args[1..], &spec);
            return CompTy::pure(Ty::Unit);
        }

        if name == "_type" {
            let ty = args
                .first()
                .map(|arg| self.infer_val(arg))
                .unwrap_or_else(|| self.ctx.unifier.fresh_ty());
            let resolved = self.ctx.unifier.apply_ty(&ty);
            let pos = self
                .ctx
                .pos
                .map(|sp| format!("@{}..{}: ", sp.start, sp.end))
                .unwrap_or_default();
            eprintln!("_type: {}{}", pos, fmt_ty(&resolved));
            return CompTy::pure(ty);
        }

        if !external_only && name == "from-lines" {
            self.infer_args(args);
            return CompTy::Return(PipeSpec::decode(), Box::new(self.from_lines_step_ty()));
        }

        if !external_only && let Some(scheme) = builtin_scheme(name, &mut self.ctx.unifier) {
            let head_cty = match scheme.ty {
                Ty::Thunk(body) => *body,
                _ => self.ctx.unifier.fresh_comp_ty(),
            };
            return self.apply_args(head_cty, args);
        }

        let kind = if external_only {
            HeadKind::External
        } else {
            head_kind(name)
        };
        match kind {
            HeadKind::Branches => self.infer_branches(args),
            HeadKind::LastThunk => {
                // For `within`/`grant`: validate the options map (the first
                // non-thunk arg) against the per-name schema, then fall
                // through to inferring the thunk's return type.
                if let Some(schema) = scoping_schema(name)
                    && let Some(Val::Map(entries)) =
                        args.iter().find(|a| !matches!(a, Val::Thunk(_)))
                {
                    self.check_map_entry_fields(entries, name, schema);
                }
                self.infer_last_thunk(args)
            }
            HeadKind::Bytes | HeadKind::External => self.external_exec_comp_ty(args),
            HeadKind::StreamingReducer => {
                self.infer_args(args);
                CompTy::Return(
                    PipeSpec {
                        input: PipeMode::Bytes,
                        output: PipeMode::Bytes,
                    },
                    Box::new(Ty::Unit),
                )
            }
            HeadKind::Value => {
                self.infer_args(args);
                CompTy::pure(self.ctx.unifier.fresh_ty())
            }
            HeadKind::DecodeToValue => {
                self.infer_args(args);
                CompTy::Return(PipeSpec::decode(), Box::new(self.ctx.unifier.fresh_ty()))
            }
            HeadKind::EncodeToBytes => {
                self.infer_args(args);
                CompTy::Return(PipeSpec::encode(), Box::new(Ty::Bytes))
            }
            HeadKind::Never => {
                self.infer_args(args);
                CompTy::Return(
                    PipeSpec {
                        input: self.ctx.unifier.fresh_mode(),
                        output: self.ctx.unifier.fresh_mode(),
                    },
                    Box::new(self.ctx.unifier.fresh_ty()),
                )
            }
        }
    }

    fn external_exec_comp_ty(&mut self, args: &[Val]) -> CompTy {
        self.infer_args(args);
        let input = self.ctx.unifier.fresh_mode();
        CompTy::Return(
            PipeSpec {
                input,
                output: PipeMode::Bytes,
            },
            Box::new(Ty::String),
        )
    }

    fn infer_args(&mut self, args: &[Val]) {
        for arg in args {
            let _ = self.infer_val(arg);
        }
    }

    /// Infer each computation in `parts` in order, returning the last type.
    /// If `parts` is empty, returns `CompTy::pure(empty)`.
    fn infer_sequence(&mut self, parts: &[Comp], empty: Ty) -> CompTy {
        let mut last = CompTy::pure(empty);
        for part in parts {
            last = self.infer_comp(part);
        }
        last
    }

    fn infer_map_val(&mut self, entries: &[ValMapEntry]) -> Ty {
        let all_literal_keys = entries.iter().all(|entry| match entry {
            ValMapEntry::Entry(Val::Literal(_) | Val::String(_), _) => true,
            ValMapEntry::Entry(_, _) => false,
            ValMapEntry::Spread(_) => true,
        });

        if all_literal_keys && !entries.is_empty() {
            let mut spread_rows = Vec::new();
            let mut field_entries = Vec::new();
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(Val::Literal(key) | Val::String(key), value)
                        if key == "plugins" && matches!(value, Val::List(_)) =>
                    {
                        let Val::List(elems) = value else {
                            unreachable!()
                        };
                        let ty = self.infer_plugins_list(elems);
                        field_entries.push((key.clone(), ty));
                    }
                    ValMapEntry::Entry(Val::Literal(key) | Val::String(key), value) => {
                        field_entries.push((key.clone(), self.infer_val(value)));
                    }
                    ValMapEntry::Spread(value) => {
                        let spread_ty = self.infer_val(value);
                        let row_var = self.ctx.unifier.fresh_row_var();
                        self.ctx
                            .unify_ty(&spread_ty, &Ty::Record(Row::Var(row_var)));
                        spread_rows.push(row_var);
                    }
                    ValMapEntry::Entry(_, value) => {
                        let _ = self.infer_val(value);
                    }
                }
            }

            let mut row = match spread_rows.len() {
                0 => Row::Empty,
                1 => Row::Var(spread_rows[0]),
                _ => Row::Var(self.ctx.unifier.fresh_row_var()),
            };
            for (key, value_ty) in field_entries.into_iter().rev() {
                row = Row::Extend(key, Box::new(value_ty), Box::new(row));
            }
            Ty::Record(row)
        } else {
            let value_ty = self.ctx.unifier.fresh_ty();
            for entry in entries {
                match entry {
                    ValMapEntry::Entry(_, value) => {
                        let _ = self.infer_val(value);
                    }
                    ValMapEntry::Spread(value) => {
                        let spread_ty = self.infer_val(value);
                        let inner = self.ctx.unifier.fresh_ty();
                        self.ctx
                            .unify_ty(&spread_ty, &Ty::Map(Box::new(inner.clone())));
                    }
                }
            }
            Ty::Map(Box::new(value_ty))
        }
    }

    fn infer_val(&mut self, val: &Val) -> Ty {
        match val {
            Val::Unit => Ty::Unit,
            Val::TildePath(_) => Ty::String,
            Val::Literal(s) => literal_ty(s),
            Val::String(_) => Ty::String,
            Val::Int(_) => Ty::Int,
            Val::Float(_) => Ty::Float,
            Val::Bool(_) => Ty::Bool,
            Val::Variable(name) => match self.env.lookup(name).cloned() {
                Some(scheme) => instantiate(&mut self.ctx.unifier, &scheme),
                None => self.ctx.unifier.fresh_ty(),
            },
            Val::Thunk(comp) => Ty::Thunk(Box::new(self.with_scope(|this| this.infer_comp(comp)))),
            Val::List(elems) => {
                let elem = self.ctx.unifier.fresh_ty();
                for entry in elems {
                    let entry_ty = match entry {
                        ValListElem::Single(value) => self.infer_val(value),
                        ValListElem::Spread(value) => {
                            let spread_ty = self.infer_val(value);
                            let inner = self.ctx.unifier.fresh_ty();
                            self.ctx
                                .unify_ty(&spread_ty, &Ty::List(Box::new(inner.clone())));
                            inner
                        }
                    };
                    self.ctx.unify_ty(&entry_ty, &elem);
                }
                Ty::List(Box::new(elem))
            }
            Val::Map(entries) => self.infer_map_val(entries),
            Val::Variant { label, payload } => {
                // Variant construction is open: `.ok 5` infers
                // `[.ok: Int | ρ]` where ρ is a fresh row variable.  The
                // label is stored *with* its leading dot in the row so that
                // alphabet checks at unify time treat it as a tag.
                let payload_ty = match payload {
                    Some(p) => self.infer_val(p),
                    None => Ty::Unit,
                };
                let rest = self.ctx.unifier.fresh_row();
                Ty::Variant(Row::Extend(
                    format!(".{label}"),
                    Box::new(payload_ty),
                    Box::new(rest),
                ))
            }
        }
    }

    fn infer_pipeline(&mut self, stages: &[Comp]) -> CompTy {
        if stages.is_empty() {
            return CompTy::bytes_in_out(Ty::Unit);
        }

        let mut stage_tys: Vec<CompTy> =
            stages.iter().map(|stage| self.infer_comp(stage)).collect();
        for i in 0..stage_tys.len() - 1 {
            let out = self.comp_output_mode(&stage_tys[i]);
            let out_resolved = self.ctx.unifier.resolve_mode(&out);

            if out_resolved == PipeMode::None {
                let (piped_ty, _, _) = self.extract_return(&stage_tys[i]);
                let next = stage_tys[i + 1].clone();
                stage_tys[i + 1] = self.apply_piped_value(next, piped_ty);
                continue;
            }

            let inp = self.comp_input_mode(&stage_tys[i + 1]);
            self.ctx.unify_mode(&out, &inp);
        }

        let input = self.comp_input_mode(&stage_tys[0]);
        let last = &stage_tys[stage_tys.len() - 1];
        let output = self.comp_output_mode(last);
        let ret_ty = match self.ctx.unifier.resolve_comp_ty(last) {
            CompTy::Return(_, ty) => *ty,
            _ => self.ctx.unifier.fresh_ty(),
        };
        CompTy::Return(PipeSpec { input, output }, Box::new(ret_ty))
    }

    fn infer_index(&mut self, target: &Comp, keys: &[Comp]) -> CompTy {
        let target_cty = self.infer_comp(target);
        let (target_ty, _, _) = self.extract_return(&target_cty);

        let mut current_ty = target_ty;
        for key in keys {
            let resolved = self.ctx.unifier.apply_ty(&current_ty);
            current_ty = match resolved {
                Ty::List(elem) | Ty::Map(elem) => {
                    let _ = self.infer_comp(key);
                    *elem
                }
                Ty::Thunk(_) => {
                    self.ctx.error_hint(
                        "cannot index a Block".to_string(),
                        "force it first with '!'",
                    );
                    let _ = self.infer_comp(key);
                    self.ctx.unifier.fresh_ty()
                }
                _ => {
                    let record_label = match &key.kind {
                        CompKind::Return(Val::Literal(label)) if label.parse::<i64>().is_err() => {
                            Some(label.clone())
                        }
                        CompKind::Return(Val::String(label)) => Some(label.clone()),
                        _ => None,
                    };
                    if let Some(label) = record_label {
                        let field_ty = self.ctx.unifier.fresh_ty();
                        let tail_row = self.ctx.unifier.fresh_row();
                        let record_ty = Ty::Record(Row::Extend(
                            label,
                            Box::new(field_ty.clone()),
                            Box::new(tail_row),
                        ));
                        self.ctx.unify_ty(&current_ty, &record_ty);
                        field_ty
                    } else {
                        let _ = self.infer_comp(key);
                        self.ctx.unifier.fresh_ty()
                    }
                }
            };
        }

        CompTy::pure(current_ty)
    }

    fn infer_case(&mut self, scrutinee: &Comp, table: &Comp) -> CompTy {
        // Scrutinee value type, table value type, and the shared result.
        // Both scrutinee and table are constrained to return values at
        // this point: `case` inspects the scrutinee value and indexes
        // the handler table value.
        let scrutinee_cty = self.infer_comp(scrutinee);
        let (scrut_ty, _, _) = self.extract_return(&scrutinee_cty);
        let table_cty = self.infer_comp(table);
        let (table_ty, _, _) = self.extract_return(&table_cty);
        let result_cty = self.ctx.unifier.fresh_comp_ty();

        // Shape constraints.
        let scrut_row_var = self.ctx.unifier.fresh_row_var();
        self.ctx
            .unify_ty(&scrut_ty, &Ty::Variant(Row::Var(scrut_row_var)));
        let handler_row_var = self.ctx.unifier.fresh_row_var();
        self.ctx
            .unify_ty(&table_ty, &Ty::Record(Row::Var(handler_row_var)));

        // Resolve the handler row.  Record literals always close to Empty,
        // so this returns a clean label list under normal use.
        let handler_resolved = self
            .ctx
            .unifier
            .apply_row(&Row::Var(handler_row_var));
        let handler_labels = collect_extends(&handler_resolved);

        // Per-label connection: each handler at `.l` must be a thunk of a
        // function `payload_l → result_cty`.  Build the closed scrutinee row
        // from these payload types as we go.
        let mut closed_scrut = Row::Empty;
        for (label, handler_ty) in handler_labels.iter().rev() {
            let payload_ty = self.ctx.unifier.fresh_ty();
            let expected = Ty::Thunk(Box::new(CompTy::Fun(
                Box::new(payload_ty.clone()),
                Box::new(result_cty.clone()),
            )));
            if self.ctx.unifier.unify_ty(handler_ty, &expected).is_err() {
                let expected_resolved = self.ctx.unifier.apply_ty(&expected);
                let found_resolved = self.ctx.unifier.apply_ty(handler_ty);
                self.ctx.emit_kind(
                    crate::typecheck::scheme::TypeErrorKind::CaseLabelTypeMismatch {
                        label: label.clone(),
                        expected: expected_resolved,
                        found: found_resolved,
                    },
                    None,
                );
            }
            closed_scrut = Row::Extend(
                label.clone(),
                Box::new(payload_ty),
                Box::new(closed_scrut),
            );
        }

        // Force scrutinee row to exactly the handler label set.  Row mismatch
        // becomes CaseNotExhaustive: an extra label on the handler side means
        // the handler covers a constructor the scrutinee can never produce;
        // a missing label means the scrutinee has a constructor with no arm.
        if let Err(kind) = self
            .ctx
            .unifier
            .unify_row(&Row::Var(scrut_row_var), &closed_scrut)
        {
            use crate::typecheck::scheme::TypeErrorKind;
            let translated = match kind {
                TypeErrorKind::RowExtraField { label } => TypeErrorKind::CaseNotExhaustive {
                    missing: vec![],
                    extra: vec![label],
                },
                TypeErrorKind::RowMissingField { label } => TypeErrorKind::CaseNotExhaustive {
                    missing: vec![label],
                    extra: vec![],
                },
                other => other,
            };
            self.ctx.emit_kind(translated, None);
        }

        result_cty
    }

    fn infer_letrec(&mut self, bindings: &[(String, Comp)]) -> CompTy {
        let betas: Vec<CompTy> = bindings
            .iter()
            .map(|_| self.ctx.unifier.fresh_comp_ty())
            .collect();

        for ((name, _), beta) in bindings.iter().zip(betas.iter()) {
            self.env.bind(
                name.clone(),
                Scheme::mono(Ty::Thunk(Box::new(beta.clone()))),
            );
        }
        for ((_, lam_comp), beta) in bindings.iter().zip(betas.iter()) {
            let lam_ty = self.infer_comp(lam_comp);
            self.ctx.unify_comp_ty(&lam_ty, beta);
        }
        // Drop the mono self-bindings before generalising.  If they
        // stayed in env, `env_free_vars` would see their (post-body)
        // free comp/ty/row vars as residuals and `generalize` would
        // refuse to quantify them — which silently un-poly's every
        // recursive scheme and lets one call site bind a polymorphic
        // var that all other call sites then share.  Re-bind below
        // with the polymorphic schemes once each is built.
        for (name, _) in bindings {
            self.env.unbind(name);
        }
        let mut schemes: Vec<(String, Scheme)> = Vec::with_capacity(bindings.len());
        for ((name, _), beta) in bindings.iter().zip(betas.iter()) {
            let thunk_ty = Ty::Thunk(Box::new(beta.clone()));
            let scheme = generalize(&mut self.ctx.unifier, self.env, &thunk_ty);
            schemes.push((name.clone(), scheme));
        }
        for (name, scheme) in schemes {
            self.env.bind(name, scheme);
        }

        CompTy::pure(Ty::Unit)
    }

    fn infer_comp(&mut self, comp: &Comp) -> CompTy {
        // Update position from the node's span.
        if let Some(span) = comp.span {
            self.ctx.pos = Some(span);
        }

        match &comp.kind {
            CompKind::Return(value) => CompTy::pure(self.infer_val(value)),
            CompKind::Lam { param, body } => {
                let param_ty = self.ctx.unifier.fresh_ty();
                let body_ty = self.with_scope(|this| {
                    this.bind_pattern(param, &param_ty);
                    this.infer_comp(body)
                });
                CompTy::Fun(Box::new(param_ty), Box::new(body_ty))
            }
            CompKind::Rec { name, body } => {
                let beta = self.ctx.unifier.fresh_comp_ty();
                let body_ty = self.with_scope(|this| {
                    this.env.bind(
                        name.clone(),
                        Scheme::mono(Ty::Thunk(Box::new(beta.clone()))),
                    );
                    this.infer_comp(body)
                });
                self.ctx.unify_comp_ty(&body_ty, &beta);
                body_ty
            }
            CompKind::Force(value) => {
                let val_ty = self.infer_val(value);
                let cty = self.ctx.unifier.fresh_comp_ty();
                self.ctx
                    .unify_ty(&val_ty, &Ty::Thunk(Box::new(cty.clone())));
                cty
            }
            CompKind::Bind {
                comp: inner,
                pattern,
                rest,
            } => {
                let inner_ty = self.infer_comp(inner);
                let bound_ty = match self.ctx.unifier.resolve_comp_ty(&inner_ty) {
                    CompTy::Fun(..) => Ty::Thunk(Box::new(inner_ty)),
                    _ => {
                        let (ty, _, _) = self.extract_return(&inner_ty);
                        ty
                    }
                };

                match pattern {
                    Pattern::Name(name) => {
                        let scheme = generalize(&mut self.ctx.unifier, self.env, &bound_ty);
                        self.env.bind(name.clone(), scheme);
                    }
                    other => {
                        let concrete = self.ctx.unifier.apply_ty(&bound_ty);
                        self.bind_pattern(other, &concrete);
                    }
                }
                self.infer_comp(rest)
            }
            CompKind::App { head, args, .. } => {
                let head_ty = self.infer_comp(head);
                // Surface the common surface error — a literal value
                // (`'foo'`, `42`, ...) used as a command head with args —
                // before falling into the general `Cmd a vs a → b`
                // mismatch path, which prints implementation jargon.
                if !args.is_empty()
                    && let Some(ty) = self.head_non_callable_ty(&head_ty)
                {
                    self.ctx.emit_kind(
                        crate::typecheck::scheme::TypeErrorKind::HeadNotCallable { ty },
                        Some(
                            "a command head must be a function or a thunk; \
                             a value here is data, not a callable — pass it as \
                             an argument or wrap a callable instead",
                        ),
                    );
                    // Still type-check the args for cascading errors, then
                    // return a fresh result so the outer pipeline / chain
                    // type-checks against something coherent.
                    for arg in args {
                        let _ = self.infer_val(arg);
                    }
                    return self.ctx.unifier.fresh_comp_ty();
                }
                self.apply_args(head_ty, args)
            }
            CompKind::Exec {
                name,
                args,
                external_only,
                ..
            } => match name {
                ExecName::Bare(name) => self.exec_comp_ty(name, args, *external_only),
                ExecName::Path(_) | ExecName::TildePath(_) => self.external_exec_comp_ty(args),
            },
            CompKind::Builtin { name, args } => self.exec_comp_ty(name, args, false),
            CompKind::Pipeline(stages) => self.infer_pipeline(stages),
            CompKind::Chain(parts) => {
                let empty = self.ctx.unifier.fresh_ty();
                self.infer_sequence(parts, empty)
            }
            CompKind::PrimOp(op, args) => CompTy::pure(self.infer_primop(*op, args)),
            CompKind::Interpolation(parts) => {
                for value in parts {
                    let _ = self.infer_val(value);
                }
                CompTy::pure(Ty::String)
            }
            CompKind::Background(inner) => {
                let inner_cty = self.infer_comp(inner);
                let payload = match self.ctx.unifier.resolve_comp_ty(&inner_cty) {
                    CompTy::Return(_, a) => *a,
                    _ => self.ctx.unifier.fresh_ty(),
                };
                CompTy::pure(Ty::Handle(Box::new(payload)))
            }
            CompKind::Index { target, keys } => self.infer_index(target, keys),
            CompKind::Seq(comps) => self.infer_sequence(comps, Ty::Unit),
            CompKind::LetRec {
                slot: None,
                bindings,
            } => self.infer_letrec(bindings),
            CompKind::LetRec { slot: Some(_), .. } => CompTy::pure(self.ctx.unifier.fresh_ty()),
            CompKind::If { cond, then, else_ } => {
                let cond_cty = self.infer_comp(cond);
                self.ctx.unify_comp_ty(&cond_cty, &CompTy::pure(Ty::Bool));
                let result = self.ctx.unifier.fresh_comp_ty();
                let then_ty = self.infer_val(then);
                let else_ty = self.infer_val(else_);
                let thunk_ty = Ty::Thunk(Box::new(result.clone()));
                self.ctx.unify_ty(&then_ty, &thunk_ty);
                self.ctx.unify_ty(&else_ty, &thunk_ty);
                result
            }
            CompKind::Case { scrutinee, table } => self.infer_case(scrutinee, table),
        }
    }
}
