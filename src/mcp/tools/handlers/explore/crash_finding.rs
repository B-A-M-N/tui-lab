//! Crash-minimization finding pipeline (god-object round 2, G5c):
//! moved OUT of the `TuiLabServer` impl — building a crash Finding is
//! exploration-domain glue (Wave D item 38), not server plumbing. The
//! server borrow is a parameter now.

use crate::audit::{Category, Severity};
use rmcp::serde_json::json;

/// Wave D item 38: when an exploration report recorded a crash exit,
/// run the full minimization pipeline — clean restart, delta-debug
/// replay, saved Scenario — and emit a Finding with
/// `reproduction = scenario_id` into the run. Returns the pipeline
/// record for the tool response (`null` when nothing crashed).
pub(crate) fn minimize_crash_finding(
    server: &crate::mcp::tools::TuiLabServer,
    sess: &mut crate::session::state::Session,
    seed: u64,
    report: &crate::exploration::random::ExploreReport,
) -> serde_json::Value {
    use crate::exploration::repro::FailureKind;

    // Only a failure-classified exit is a reproduction candidate; clean
    // exits and signal-free deaths get no pipeline (and no fabricated
    // finding).
    let crash_exit = report.exits.iter().find(|e| {
        matches!(
            e.classification,
            crate::exploration::random::ExitClassification::Error
                | crate::exploration::random::ExitClassification::Signal
        )
    });
    let Some(exit) = crash_exit else {
        return serde_json::Value::Null;
    };
    let expected = match exit.classification {
        crate::exploration::random::ExitClassification::Signal => FailureKind::Crash,
        _ => FailureKind::Crash,
    };

    // Steps up to and including the crashing action.
    let upto = exit.action_index as usize;
    let trace: Vec<crate::exploration::random::ExplorationStep> = report
        .steps
        .iter()
        .filter(|s| s.seq as usize <= upto)
        .cloned()
        .collect();

    let name = format!("seed{seed}-act{}", exit.action_index);
    let pipeline = crate::exploration::repro::minimize_crash(sess, &trace, expected, &name);

    if !pipeline.reproduced {
        // Honest outcome: the trace did not reproduce on a clean
        // restart. Report the attempt; emit no reproduction finding.
        let mut run = server.run.lock().unwrap();
        let _ = run.extend_findings(vec![crate::audit::Finding {
            id: format!("EXPLORE-CRASH-{}", exit.action_index),
            rule_id: None,
            severity: Severity::Warn,
            category: Category::Other("exploration".into()),
            summary: format!(
                "Exploration crash at action {} ({}): original trace ({} steps) did not reproduce on a clean restart — not minimized, no scenario fabricated",
                exit.action_index,
                exit.action_name,
                pipeline.original_len,
            ),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                format!("crash_at_{}", exit.action_index),
                "process died during seeded exploration",
            )
            .with_detail(json!({
                "seed": seed,
                "action_index": exit.action_index,
                "action_name": exit.action_name,
                "exit_code": exit.exit_code,
                "exit_signal": exit.exit_signal,
                "attempts": pipeline.attempts,
            }))],
            confidence: 0.9,
            reproduction: None,
            source_refs: Vec::new(),
            occurrence_id: None,
        }]);
        return json!({
            "reproduced": false,
            "attempts": pipeline.attempts,
            "note": "trace did not reproduce on a clean restart; no scenario fabricated",
        });
    }

    // Save the minimized scenario into the run and attach its ID to the
    // finding (item 38, the last mile). The pipeline hands back the
    // fully-built Scenario — no lossy rebuild.
    let scenario = match pipeline.scenario.clone() {
        Some(s) => s,
        None => return json!({ "reproduced": false, "attempts": pipeline.attempts }),
    };
    let scenario_id = scenario.id.clone();
    let saved = {
        let mut run = server.run.lock().unwrap();
        run.save_scenario(scenario);
        let _ = run.extend_findings(vec![crate::audit::Finding {
            id: format!("EXPLORE-CRASH-{}", exit.action_index),
            rule_id: None,
            severity: Severity::Error,
            category: Category::Other("exploration".into()),
            summary: format!(
                "Exploration crash at action {} ({}): minimized to {} step(s), saved as scenario {}",
                exit.action_index,
                exit.action_name,
                pipeline.minimized_len,
                scenario_id,
            ),
            evidence: vec![crate::audit::EvidenceRef::point(
                crate::audit::EvidenceKind::Other,
                format!("repro_{}", scenario_id),
                "minimized reproduction saved as a replayable scenario",
            )
            .with_detail(json!({
                "seed": seed,
                "action_index": exit.action_index,
                "action_name": exit.action_name,
                "original_len": pipeline.original_len,
                "minimized_len": pipeline.minimized_len,
                "minimized_steps": pipeline.steps,
                "attempts": pipeline.attempts,
            }))],
            confidence: 1.0,
            reproduction: Some(scenario_id.clone()),
            source_refs: Vec::new(),
            occurrence_id: None,
        }]);
        scenario_id.clone()
    };
    json!({
        "reproduced": true,
        "scenario_id": saved,
        "original_len": pipeline.original_len,
        "minimized_len": pipeline.minimized_len,
        "minimized_steps": pipeline.steps,
        "attempts": pipeline.attempts,
    })
}
