//! Audit finding 50: a real scenario-owned persona matrix. The same target,
//! scenario, parameters, dimensions and build are launched independently
//! under each persona; the persona's declared environment is the only changing
//! dimension. Each row retains launch provenance and a scenario verdict.

use std::sync::{Arc, Mutex};

use tui_lab::scenario::model::{FailurePolicy, ParameterValue, Scenario, ScenarioLaunch};
use tui_lab::scenario::runner::{RunStatus, ScenarioRunReport};
use tui_lab::session::state::LaunchSpec;
use tui_lab::session::SessionPool;
use tui_lab::terminal::{PersonaMatrixExecution, PersonaMatrixOutcome};

type RowOutcome = Result<ScenarioRunReport, String>;
#[derive(Debug)]
struct MatrixError(anyhow::Error);

impl std::fmt::Display for MatrixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for MatrixError {}

impl From<String> for MatrixError {
    fn from(value: String) -> Self {
        Self(anyhow::anyhow!(value))
    }
}

impl From<anyhow::Error> for MatrixError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}

type Matrix = PersonaMatrixExecution<RowOutcome, MatrixError>;

#[derive(Clone)]
struct MatrixFixture {
    scenario: Scenario,
    values: Vec<ParameterValue>,
    run: Option<Arc<Mutex<tui_lab::run::RunContext>>>,
    policy: Option<FailurePolicy>,
}

fn owned_scenario() -> Scenario {
    let scenario = Scenario {
        inherit_session: false,
        launch: Some(ScenarioLaunch {
            command: "python3".into(),
            args: vec![
                "-c".into(),
                "print('PERSONA-READY'); import sys,time; sys.stdin.read(1)".into(),
            ],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
            backend: "auto".into(),
            isolation: "local".into(),
        }),
        ..Scenario::new("persona-matrix")
    };
    scenario
        .act(serde_json::json!({ "action": "type", "text": "matrix" }))
        .assert(serde_json::json!({ "assertion": "text", "text": "matrix" }))
}

fn fixture() -> MatrixFixture {
    MatrixFixture {
        scenario: owned_scenario(),
        values: Vec::new(),
        run: None,
        policy: None,
    }
}

fn expected_terms() -> Vec<String> {
    vec![
        "dumb".into(),
        "xterm".into(),
        "xterm-256color".into(),
        "xterm-256color".into(),
    ]
}

fn persona_matrix_relaunches_same_scenario_independently() -> std::process::ExitCode {
    let f = fixture();
    let scenario = f.scenario;
    let values = f.values;
    let run = f.run;
    let policy = f.policy;
    let pool = SessionPool::new();
    let selected = vec![
        "dumb".to_string(),
        "xterm".to_string(),
        "xterm-256color".to_string(),
        "truecolor".to_string(),
    ];
    let scenario_ref = &scenario;
    let pool_ref = &pool;
    let run_ref = run.as_ref();

    let report: Matrix = tui_lab::scenario::runner::run_persona_matrix_selected(
        &scenario,
        &selected,
        &values,
        run_ref,
        policy,
        move |_scenario, persona, spec, values, run, policy| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("persona runtime");
            let values = values.to_vec();
            let run = run.map(std::sync::Arc::clone);
            rt.block_on(async move {
                let launch: LaunchSpec = spec.clone();
                assert!(
                    launch
                        .env
                        .iter()
                        .any(|(k, v)| k == "TERM" && v == &persona.term),
                    "persona launch must carry its TERM declaration"
                );
                let sid = pool_ref.start_with_spec(launch).await?;
                let generation = pool_ref
                    .with_session(Some(&sid), |sess| sess.generation)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let scenario = scenario_ref.clone();
                let outcome = pool_ref
                    .with_session(Some(&sid), move |sess| {
                        tui_lab::scenario::runner::ScenarioRunner::run_in_run_with_policy(
                            &scenario,
                            sess,
                            &values,
                            run.as_ref(),
                            policy,
                        )
                    })
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let row = tui_lab::terminal::PersonaMatrixRow {
                    persona: persona.id.clone(),
                    term: persona.term.clone(),
                    colorterm: persona.colorterm.clone(),
                    outcome: Ok(outcome),
                };
                let provenance = PersonaMatrixOutcome {
                    row,
                    scenario_id: scenario_ref.id.clone(),
                    scenario_schema: scenario_ref.schema.clone(),
                    launch_spec: spec.clone(),
                    generation,
                };
                Ok::<_, anyhow::Error>((provenance, Some(sid)))
            })
            .map_err(MatrixError)
        },
    );

    for o in &report.outcomes {
        if let Err(e) = o {
            println!("matrix launch error: {e}");
        }
    }
    assert!(report.launches_passed, "all persona launches must succeed");
    assert_eq!(report.outcomes.len(), 4);
    let mut terms = Vec::new();
    for outcome in report.outcomes.iter() {
        let outcome = outcome.as_ref().expect("persona execution");
        assert!(outcome.generation >= 1);
        assert_eq!(outcome.scenario_id, scenario.id);
        assert_eq!(outcome.launch_spec.cols, 80);
        assert_eq!(outcome.launch_spec.rows, 24);
        let run = outcome.row.outcome.as_ref().expect("scenario report");
        if run.steps_failed > 0 || run.steps_skipped > 0 {
            panic!(
                "persona {} failed: status={:?} results={:?}",
                outcome.row.persona, run.status, run.step_results
            );
        }
        assert_eq!(run.steps_total, 2);
        assert_eq!(run.steps_failed, 0);
        assert_eq!(run.steps_skipped, 0);
        assert_eq!(run.status, RunStatus::Completed);
        terms.push(outcome.row.term.clone());
    }
    assert_eq!(terms, expected_terms());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("cleanup runtime");
    rt.block_on(async {
        for sid in report.cleanup_ids {
            pool.stop(&sid).await.expect("persona session cleanup");
        }
    });
    std::process::ExitCode::SUCCESS
}

fn persona_matrix_refuses_inherited_or_unknown_persona_without_launch() -> std::process::ExitCode {
    let inherited =
        Scenario::new("bad-matrix").act(serde_json::json!({ "action": "type", "text": "never" }));
    let report: PersonaMatrixExecution<RowOutcome, MatrixError> =
        tui_lab::scenario::runner::run_persona_matrix_selected(
            &inherited,
            &["xterm".to_string()],
            &[],
            None,
            None,
            |_s, _p, _spec, _v, _r, _policy| panic!("must refuse before launch"),
        );
    assert!(!report.launches_passed);
    assert_eq!(report.outcomes.len(), 1);
    assert!(report.outcomes[0]
        .as_ref()
        .unwrap_err()
        .0
        .to_string()
        .contains("scenario-owned launch"));

    let owned = owned_scenario();
    let unknown: PersonaMatrixExecution<RowOutcome, MatrixError> =
        tui_lab::scenario::runner::run_persona_matrix_selected(
            &owned,
            &["not-a-persona".to_string()],
            &[],
            None,
            None,
            |_s, _p, _spec, _v, _r, _policy| panic!("unknown persona must refuse before launch"),
        );
    assert!(!unknown.launches_passed);
    assert_eq!(unknown.outcomes.len(), 1);
    assert!(unknown.outcomes[0]
        .as_ref()
        .unwrap_err()
        .0
        .to_string()
        .contains("unknown terminal persona"));

    std::process::ExitCode::SUCCESS
}

fn main() -> std::process::ExitCode {
    let a = persona_matrix_relaunches_same_scenario_independently();
    let b = persona_matrix_refuses_inherited_or_unknown_persona_without_launch();
    if a == std::process::ExitCode::SUCCESS && b == std::process::ExitCode::SUCCESS {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
