//! The control-operator keywords: what the parser reads for each, declared
//! once beside the vocabulary [`crate::syntax::is_keyword`] answers from.

use super::ast::{Ast, HandlerArm, Options, ScopeAst};

/// How a control operator reads the operand at one position.
///
/// Not every operand is an expression: a form's option bracket is the form's
/// own syntax, where `[]` is the empty option set rather than a list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operand {
    Atom,
    /// `[]` or `[l: v, …]`, the labels written out.  `arms` marks the form
    /// whose options include the `handlers:` arm list; `example` is an entry
    /// the refusal of a bound bundle shows.
    Options {
        arms: bool,
        example: &'static str,
    },
}

/// What a control operator's operands parsed to, by kind.
pub(crate) struct Operands {
    pub atoms: Vec<Ast>,
    pub options: Options,
    pub handlers: Option<Vec<HandlerArm>>,
}

/// Everything the parser needs for one control-operator keyword.
///
/// The surface name, how each operand position is read, a description of the
/// operands for the arity-mismatch message, and a constructor from the
/// validated operands.
pub(crate) struct ScopeKeyword {
    pub name: &'static str,
    pub(crate) operands: &'static [Operand],
    pub(crate) operand_desc: &'static str,
    pub(crate) build: fn(Operands) -> ScopeAst,
}

impl ScopeKeyword {
    pub(crate) fn arity(&self) -> usize {
        self.operands.len()
    }
}

/// Every control-operator keyword. [`crate::syntax::is_keyword`] reads this
/// list, so the parser's ban on these names in binding positions and
/// exarch's syntax highlighter cannot drift apart.
pub(crate) const KEYWORDS: &[ScopeKeyword] = &[
    ScopeKeyword {
        name: "try",
        operands: &[Operand::Atom, Operand::Atom],
        operand_desc: "body, handler",
        build: |ops| {
            let [body, handler]: [Ast; 2] = ops.atoms.try_into().expect("arity validated");
            ScopeAst::Try {
                body: Box::new(body),
                handler: Box::new(handler),
            }
        },
    },
    ScopeKeyword {
        name: "guard",
        operands: &[Operand::Atom, Operand::Atom],
        operand_desc: "body, cleanup",
        build: |ops| {
            let [body, cleanup]: [Ast; 2] = ops.atoms.try_into().expect("arity validated");
            ScopeAst::Guard {
                body: Box::new(body),
                cleanup: Box::new(cleanup),
            }
        },
    },
    ScopeKeyword {
        name: "within",
        operands: &[
            Operand::Options {
                arms: true,
                example: "dir: $d",
            },
            Operand::Atom,
        ],
        operand_desc: "options, body",
        build: |ops| {
            let [body]: [Ast; 1] = ops.atoms.try_into().expect("arity validated");
            ScopeAst::Within {
                opts: ops.options,
                handlers: ops.handlers,
                body: Box::new(body),
            }
        },
    },
    ScopeKeyword {
        name: "grant",
        operands: &[
            Operand::Options {
                arms: false,
                example: "net: $n",
            },
            Operand::Atom,
        ],
        operand_desc: "capabilities, body",
        build: |ops| {
            let [body]: [Ast; 1] = ops.atoms.try_into().expect("arity validated");
            ScopeAst::Grant {
                caps: ops.options,
                body: Box::new(body),
            }
        },
    },
    ScopeKeyword {
        name: "audit",
        operands: &[Operand::Atom],
        operand_desc: "body",
        build: |ops| {
            let [body]: [Ast; 1] = ops.atoms.try_into().expect("arity validated");
            ScopeAst::Audit {
                body: Box::new(body),
            }
        },
    },
];

/// Look up a control-operator keyword by surface name.
pub(crate) fn lookup(name: &str) -> Option<&'static ScopeKeyword> {
    KEYWORDS.iter().find(|kw| kw.name == name)
}
