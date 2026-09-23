//! Readings of a source text against the live session: its spine, and its
//! top-level `let`s' effects. Both compile, and neither runs.

use super::rows::{BindEffect, Spine, SpineError, SpineStage};
use crate::ir::{Comp, CompKind, Phrase, Toplevel};
use crate::syntax::ast::Pattern;
use crate::types::Shell;
use crate::{CompileError, compile_and_typecheck};

fn compile(shell: &Shell, src: &str) -> Result<Toplevel, CompileError> {
    compile_and_typecheck(
        src,
        shell.session_schemes(),
        crate::source::FileId::DUMMY,
        "",
        None,
    )
}

/// The first pipeline's stages and their types, or the first type error.
pub(super) fn spine(shell: &Shell, src: &str) -> Spine {
    if src.trim().is_empty() {
        return Spine::Empty;
    }
    match compile(shell, src) {
        Ok(top) => stages(&top, src).map_or(Spine::Empty, Spine::Stages),
        // Mid-typing, a parse error is an incomplete line, not an error.
        Err(CompileError::Parse(_)) => Spine::Empty,
        Err(CompileError::Types(errs)) => errs.first().map_or(Spine::Empty, |err| {
            let char_at = |byte: u32| crate::text::byte_to_char(src, byte as usize);
            Spine::TypeError(SpineError {
                span: err.pos.map(|sp| (char_at(sp.start), char_at(sp.end))),
                code: err.kind.code().to_string(),
                headline: err.kind.render_message(),
                label: err.kind.render_label(),
                hint: err.hint(),
            })
        }),
    }
}

fn stages(top: &Toplevel, src: &str) -> Option<Vec<SpineStage>> {
    fn pipeline(comp: &Comp) -> Option<&Comp> {
        match &comp.item {
            CompKind::Pipeline { .. } => Some(comp),
            CompKind::Bind {
                comp: bound, rest, ..
            } => pipeline(bound).or_else(|| pipeline(rest)),
            _ => None,
        }
    }
    let pipe = top.phrases.iter().find_map(|phrase| match &phrase.item {
        Phrase::Define { comp, .. } | Phrase::Run(comp) => pipeline(comp),
    })?;
    let CompKind::Pipeline {
        stages,
        stage_types,
        ..
    } = &pipe.item
    else {
        return None;
    };
    Some(
        stages
            .iter()
            .zip(stage_types)
            .map(|(stage, ty)| SpineStage {
                src: stage
                    .span
                    .and_then(|sp| src.get(sp.start as usize..sp.end as usize))
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                ty: crate::typecheck::fmt_ty(ty),
            })
            .collect(),
    )
}

/// Each top-level `let name = …`'s verdict off the checked IR: effectful when
/// its right side is an exec or an effect frame, or the checker coerced its
/// bytes to a value. Nothing, if `src` does not compile.
pub(super) fn bind_effects(shell: &Shell, src: &str) -> Vec<BindEffect> {
    let Ok(top) = compile(shell, src) else {
        return Vec::new();
    };
    top.phrases
        .iter()
        .filter_map(|phrase| {
            let Phrase::Define { pattern, comp, .. } = &phrase.item else {
                return None;
            };
            let Pattern::Name(name) = pattern.as_ref() else {
                return None;
            };
            let effectful = matches!(
                comp.item,
                CompKind::Exec(_)
                    | CompKind::Try { .. }
                    | CompKind::Guard { .. }
                    | CompKind::Audit { .. }
                    | CompKind::Within { .. }
                    | CompKind::Grant { .. }
                    | CompKind::Redirect { .. }
            ) || matches!(&comp.item, CompKind::Bind { rest, .. }
                if matches!(rest.item, CompKind::Decode(_)));
            Some(BindEffect {
                name: name.clone(),
                effectful,
            })
        })
        .collect()
}
