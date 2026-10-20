//! The structured combinators `par` and `all`: each owns the workers it
//! starts, and none of them outlives the call.

use super::birth::{Birth, spawn_child};
use super::{await_handle, worker_body};
use crate::evaluator::machine;
use crate::types::{HandleInner, Mooring, Settled, Shell, Value, end_unclaimed, sig_hint};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::time::Duration;

/// The children a calling expression owns, in birth order.  A call takes
/// each with [`await_handle`] (exact, claiming); [`Self::end`] ends the rest.
#[derive(Default)]
pub(super) struct Litter {
    children: Vec<HandleInner>,
}

impl Litter {
    /// Run `call` over a fresh litter, then end whatever it left unclaimed:
    /// on every way out, a panic included, nothing the call started runs on.
    fn own(
        mooring: &Mooring,
        shell: &mut Shell,
        call: impl FnOnce(&mut Self, &mut Shell) -> Settled<Value>,
    ) -> Settled<Value> {
        let mut litter = Self::default();
        let ended = catch_unwind(AssertUnwindSafe(|| call(&mut litter, shell)));
        // A no-op once the call has taken every child.
        litter.end(shell, mooring.cleanup_grace);
        ended.unwrap_or_else(|panic| resume_unwind(panic))
    }

    /// Birth a child of the call, hung under its current scope.
    fn bear(
        &mut self,
        mooring: &Mooring,
        shell: &mut Shell,
        verb: &'static str,
        title: String,
        work: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value> + Send + 'static,
    ) -> Settled<HandleInner> {
        let under = shell.scope.clone();
        let child = spawn_child(mooring, shell, Birth::Call { under, title, verb }, work)?;
        self.children.push(child.clone());
        Ok(child)
    }

    /// The halt rule: [`end_unclaimed`] over this litter.
    fn end(&self, shell: &Shell, grace: Duration) {
        end_unclaimed(&self.children, shell, grace);
    }
}

/// `par <f> <items> <jobs>` -- map `f` over `items` on at most `jobs` workers
/// at once (`jobs <= 0`: one per item), owned by this call.  Each child is
/// taken in input order, so its output is replayed in that order too.
pub(crate) fn builtin_par(args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let items = args[1].as_list("par")?;
    let Value::Int(jobs) = args[2] else {
        return Err(sig_hint(
            format!("par: jobs must be an Int, got {}", args[2].type_name()),
            "e.g. par $f $items 4; 0 runs every item at once",
        ));
    };
    let limit = usize::try_from(jobs)
        .ok()
        .filter(|&j| j > 0)
        .unwrap_or(items.len());
    Litter::own(mooring, shell, |litter, shell| {
        let mut running = VecDeque::with_capacity(limit.min(items.len()));
        let mut values = Vec::with_capacity(items.len());
        for (i, x) in items.iter().enumerate() {
            if running.len() == limit
                && let Some(oldest) = running.pop_front()
            {
                values.push(await_handle(&oldest, shell)?);
            }
            let (f, x) = (args[0].clone(), x.into_owned());
            let work = move |mooring: &Mooring, child: &mut Shell| {
                machine::apply(f, vec![x], mooring, child)
            };
            running.push_back(litter.bear(mooring, shell, "par", format!("par #{i}"), work)?);
        }
        for child in running {
            values.push(await_handle(&child, shell)?);
        }
        Ok(Value::list(values))
    })
}

/// `all [k: BODY, …]` -- run every task at once, owned by this expression, and
/// return the record of their values, taken in the order written.
pub(crate) fn all_form(
    tasks: Vec<(String, Value)>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    Litter::own(mooring, shell, |litter, shell| {
        let mut born = Vec::with_capacity(tasks.len());
        for (label, body) in tasks {
            let closure = body.expect_thunk("all")?;
            let child = litter.bear(mooring, shell, "all", label.clone(), worker_body(closure))?;
            born.push((label, child));
        }
        let fields = born
            .into_iter()
            .map(|(label, child)| Ok((label, await_handle(&child, shell)?)))
            .collect::<Settled<Vec<_>>>()?;
        Ok(Value::map(fields))
    })
}
