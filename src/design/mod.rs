//! Design contracts (Wave E items 39–49): a machine-readable description of
//! what the TUI is *supposed* to be, checked against the running app.
//!
//! * [`schema`] — the contract document model (YAML/JSON, strict fields);
//! * [`oracle`] — the declarative assertion language shared by contracts,
//!   scenarios, and audits;
//! * [`loader`] — real YAML/JSON parsing + load-time validation;
//! * [`rules`] — the query surface other subsystems read through;
//! * [`conformance`] — the PASS/FAIL/WARN checker that drives the app.

pub mod conformance;
pub mod loader;
pub mod oracle;
pub mod rules;
pub mod scaffold;
pub mod schema;

pub use conformance::{
    check_contract, check_contract_policy, check_contract_with_mode, validate_document,
    CheckResult, ContractReport, ExecPolicy, ObservedBehavior, Verdict,
};
pub use loader::{default_contract, load_design_contract, parse_json, parse_yaml, to_yaml};
pub use oracle::{
    eval_active, eval_static, is_static, parse as parse_oracle, ActiveArgs, Oracle, OracleArg,
    OracleError, OracleOutcome, KNOWN_PREDICATES,
};
pub use rules::ContractRules;
pub use scaffold::{
    gather_states, scaffold_multi_state, GatheredStates, ScaffoldBudget, ScaffoldState,
};
pub use schema::{
    ComponentContract, ContractMode, ContractSchema, InteractionContract, Keybinding,
    LaunchContract, LayoutConstraint, OracleDecl, ProjectContract, ViewportReq,
};
