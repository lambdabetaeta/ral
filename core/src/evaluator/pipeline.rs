//! Pipeline execution engine.
//!
//! Orchestrates multi-stage pipelines: type analysis, process spawning,
//! inter-stage channel wiring, pump threads, and OS signal management.
//! The evaluator delegates here for any pipeline with two or more stages.

mod group;
mod stages;

use crate::ir::Comp;
use crate::types::*;

use group::PipelineGroup;
use stages::{
    Channel, LaunchContext, analyze_stages, collect_handles, drain_trailing_bytes, launch_stage,
};

/// Conventional exit status for SIGPIPE (128 + 13).
/// A non-final pipeline stage that exits with this code was cut off
/// because its downstream consumer exited first; that is not a failure.
const SIGPIPE_STATUS: i32 = 141;

/// Execute a multi-stage pipeline.
///
/// Analyzes and type-checks all stages, then launches each in order,
/// wiring inter-stage channels (byte pipes or value channels) between
/// adjacent stages.  The pipeline's process group gets terminal foreground
/// while running; a SIGINT relay ensures Ctrl-C reaches all stages.
///
/// Returns the value produced by the final stage, or an error if any
/// non-final stage exits with a non-zero, non-SIGPIPE status.
pub(crate) fn run_pipeline(stages: &[Comp], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let specs = analyze_stages(stages, shell)?;
    let mut group = PipelineGroup::new(shell);
    let auditing = shell.audit.tree.is_some();

    let mut channel = Channel::None;
    let mut handles = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        let incoming = std::mem::replace(&mut channel, Channel::None);
        let ctx = LaunchContext {
            spec: &specs[i],
            i,
            specs: &specs,
            group: &mut group,
        };
        match launch_stage(stage, ctx, shell, incoming, auditing) {
            Ok((handle, outgoing)) => {
                handles.push(handle);
                channel = outgoing;
            }
            Err(err) => {
                group.restore_foreground();
                return Err(err);
            }
        }
        group.take_foreground();
    }

    let _relay = group.install_relay();

    let last_output = specs
        .last()
        .map(|s| s.comp_type.output)
        .unwrap_or(crate::ty::Mode::None);
    drain_trailing_bytes(&mut channel);

    let collector = collect_handles(handles, shell, stages.len());
    group.restore_foreground();
    collector.finish(shell, last_output)
}
