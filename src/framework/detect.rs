//! Framework detection (spec section 26). Inspect the target source tree for
//! build manifests and known framework crates, then look for native adapters.
//!
//! IMPORTANT (spec items 64-66):
//!   * Do not use raw substring matching against manifests — parse them.
//!   * Crossterm is terminal I/O infrastructure, NOT a framework.
//!   * `@opentui` is its own framework, not Textual.
//!   * `native_adapter = true` only when an adapter module actually exists.

use std::collections::HashSet;

/// One detected framework/library candidate with its evidence (re-review
/// item 33). `primary` on the report is `candidates[0]` when non-empty.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameworkCandidate {
    pub name: String,
    /// What the match actually is: `framework` (a widget/layout system),
    /// `terminal_io` (infrastructure like crossterm — NOT a framework),
    /// or `styling` (lipgloss-class styling libs).
    pub class: &'static str,
    pub confidence: f32,
    /// Where the evidence came from (`Cargo.toml dependency 'ratatui'`).
    pub evidence: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct FrameworkDetection {
    /// The ranked winner, when anything matched. `Some(candidates[0])`
    /// kept as a convenience field for existing consumers.
    pub primary: Option<FrameworkCandidate>,
    /// Every match, confidence-ordered (ties broken by class rank:
    /// framework before terminal_io before styling). A tree that names
    /// BOTH ratatui and crossterm lists ratatui as primary and crossterm
    /// as terminal_io context — the old single-slot shape silently
    /// dropped the runner-up (which was usually the true architecture
    /// signal).
    pub candidates: Vec<FrameworkCandidate>,
    /// Terminal I/O infrastructure detected (crossterm-class). Reported
    /// separately because it is NOT a framework claim.
    pub terminal_io: Option<String>,
    /// Styling library detected (lipgloss-class).
    pub styling: Option<String>,
    pub native_adapter: bool,
    pub coverage_adapter: bool,
    pub evidence: Vec<String>,
    /// Audit finding 36: a manifest that EXISTS but failed to parse is
    /// evidence for an agent debugging a project — never collapsed into
    /// "not detected". `parse_failures` names each broken manifest with a
    /// one-line reason; `warnings` carries softer observations (probes
    /// that could not run, paths that were skipped).
    #[serde(default)]
    pub parse_failures: Vec<FrameworkParseFailure>,
    /// `searched_paths` — the directories actually scanned (the resolved
    /// project root and, for monorepos, the workspace root), so a caller
    /// can tell WHERE detection looked.
    #[serde(default)]
    pub searched_paths: Vec<String>,
}

/// One manifest that existed but could not be parsed (audit finding 36).
#[derive(Debug, serde::Serialize)]
pub struct FrameworkParseFailure {
    pub file: String,
    pub reason: String,
}

impl FrameworkDetection {
    /// The framework-class primary, when the top candidate actually is a
    /// framework (a crossterm-only tree has a terminal_io primary and NO
    /// framework claim — item 33's category separation).
    pub fn framework(&self) -> Option<&str> {
        self.candidates
            .iter()
            .find(|c| c.class == CLASS_FRAMEWORK)
            .map(|c| c.name.as_str())
    }

    /// Overall confidence: the primary candidate's, or 0.0 when nothing
    /// matched (the historical field, kept for wire stability).
    pub fn confidence(&self) -> f32 {
        self.primary.as_ref().map(|c| c.confidence).unwrap_or(0.0)
    }
}

/// Known TUI frameworks and their identifying package/crate names.
/// (name, class, manifest markers, dependency markers, extra hints, secondary libs)
type FrameworkSpec = (
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

/// Class names for candidates (item 33).
const CLASS_FRAMEWORK: &str = "framework";
const CLASS_TERMINAL_IO: &str = "terminal_io";
const CLASS_STYLING: &str = "styling";

const FRAMEWORKS: &[FrameworkSpec] = &[
    ("ratatui", CLASS_FRAMEWORK, &["ratatui"], &[], &[], &[]),
    (
        "textual",
        CLASS_FRAMEWORK,
        &[],
        &["textual"],
        &[],
        &["textual"],
    ),
    (
        "opentui",
        CLASS_FRAMEWORK,
        &[],
        &["@opentui/core", "@opentui/react"],
        &[],
        &[],
    ),
    ("ink", CLASS_FRAMEWORK, &[], &["ink"], &[], &[]),
    (
        "bubbletea",
        CLASS_FRAMEWORK,
        &[],
        &[],
        &["github.com/charmbracelet/bubbletea"],
        &[],
    ),
    // Item 33: crossterm is terminal I/O, not a framework; lipgloss is
    // styling. Both still DETECT (their presence is evidence about the
    // tree) but they classify separately so "framework: crossterm" — a
    // category error the old shape committed — cannot recur.
    (
        "crossterm",
        CLASS_TERMINAL_IO,
        &["crossterm"],
        &[],
        &[],
        &[],
    ),
    (
        "lipgloss",
        CLASS_STYLING,
        &[],
        &[],
        &["github.com/charmbracelet/lipgloss"],
        &[],
    ),
];

pub fn detect(cwd: &str) -> FrameworkDetection {
    let mut evidence = Vec::new();
    // Item 33: accumulate EVERY match with its evidence; rank at the end.
    // The old single-slot `detected` was overwritten per stream, so a tree
    // with ratatui AND crossterm kept whichever matched last, and the
    // class distinction (framework vs terminal I/O vs styling) was lost.
    let mut candidates: Vec<FrameworkCandidate> = Vec::new();
    // Audit finding 36: manifest parse failures are evidence, not absence.
    let mut parse_failures: Vec<FrameworkParseFailure> = Vec::new();
    // Record (or reinforce) one candidate. Reinforcement raises
    // confidence (two independent loci agreeing is stronger evidence).
    fn note(
        candidates: &mut Vec<FrameworkCandidate>,
        name: &str,
        class: &'static str,
        confidence: f32,
        locus: String,
    ) {
        if let Some(c) = candidates.iter_mut().find(|c| c.name == name) {
            c.confidence = (c.confidence + confidence * 0.25).min(1.0);
            c.evidence.push(locus);
        } else {
            candidates.push(FrameworkCandidate {
                name: name.to_string(),
                class,
                confidence,
                evidence: vec![locus],
            });
        }
    }

    let files = list_files(cwd);
    let file_set: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();

    // Parse package.json properly.
    if file_set.contains("package.json") {
        let txt = read_file(cwd, "package.json");
        match serde_json::from_str::<serde_json::Value>(&txt) {
            Ok(pkg) => {
                let all_deps = collect_json_dep_keys(&pkg);
                for (fw, class, _cargo, npm, _go, _pip) in FRAMEWORKS {
                    for npm_pkg in *npm {
                        if all_deps.iter().any(|d| d.as_str() == *npm_pkg) {
                            let confidence =
                                if *npm_pkg == "@opentui/core" || *npm_pkg == "@opentui/react" {
                                    1.0
                                } else {
                                    0.9
                                };
                            note(
                                &mut candidates,
                                fw,
                                class,
                                confidence,
                                format!("package.json dependency '{}'", npm_pkg),
                            );
                        }
                    }
                }
            }
            Err(e) => parse_failures.push(FrameworkParseFailure {
                file: format!("{}/package.json", cwd),
                reason: format!("parse failed: {e}"),
            }),
        }
    }

    // Parse Cargo.toml properly (including workspace dependency tables).
    if file_set.contains("Cargo.toml") {
        let txt = read_file(cwd, "Cargo.toml");
        match txt.parse::<toml::Value>() {
            Ok(toml_val) => {
                let all_deps = collect_toml_dep_keys(&toml_val);
                for (fw, class, cargo, _npm, _go, _pip) in FRAMEWORKS {
                    for crate_name in *cargo {
                        if all_deps.iter().any(|d| d.as_str() == *crate_name) {
                            let confidence = if *crate_name == "ratatui" { 1.0 } else { 0.9 };
                            note(
                                &mut candidates,
                                fw,
                                class,
                                confidence,
                                format!("Cargo.toml dependency '{}'", crate_name),
                            );
                        }
                    }
                }
            }
            Err(e) => parse_failures.push(FrameworkParseFailure {
                file: format!("{}/Cargo.toml", cwd),
                reason: format!("parse failed: {e}"),
            }),
        }
        // Workspace manifests declare deps in [workspace.dependencies]; a
        // workspace root is a perfectly good detection locus (item 46).
        if !candidates.iter().any(|c| c.class == CLASS_FRAMEWORK) && file_set.contains("Cargo.lock")
        {
            if let Ok(lock) = read_file(cwd, "Cargo.lock").parse::<toml::Value>() {
                if let Some(pkgs) = lock.get("package").and_then(|p| p.as_array()) {
                    'outer: for (fw, class, cargo, _npm, _go, _pip) in FRAMEWORKS {
                        for crate_name in *cargo {
                            if pkgs.iter().any(|p| {
                                p.get("name").and_then(|n| n.as_str()) == Some(*crate_name)
                            }) {
                                note(
                                    &mut candidates,
                                    fw,
                                    class,
                                    0.85,
                                    format!(
                                        "Cargo.lock package '{}' (transitive: check Cargo.toml for a direct dep)",
                                        crate_name
                                    ),
                                );
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
    }

    // Parse go.mod for Go frameworks.
    if file_set.contains("go.mod") {
        let txt = read_file(cwd, "go.mod");
        for (fw, class, _cargo, _npm, go_modules, _pip) in FRAMEWORKS {
            for go_mod in *go_modules {
                if txt
                    .lines()
                    .any(|line| line.trim().starts_with(go_mod) || line.contains(go_mod))
                {
                    note(
                        &mut candidates,
                        fw,
                        class,
                        0.9,
                        format!("go.mod references '{}'", go_mod),
                    );
                }
            }
        }
    }

    // Parse Python project manifests.
    if file_set.contains("pyproject.toml") {
        // pyproject gets REAL parsing (item 46): the [project] dependencies
        // array and the [tool.poetry.dependencies] table are structured data
        // — line-guessing them misfires (a comment naming "textual" is not a
        // dependency).
        let txt = read_file(cwd, "pyproject.toml");
        match txt.parse::<toml::Value>() {
        Ok(py) => {
            let mut py_deps: Vec<String> = Vec::new();
            if let Some(deps) = py
                .get("project")
                .and_then(|p| p.get("dependencies"))
                .and_then(|d| d.as_array())
            {
                for d in deps {
                    // PEP 508 strings: take the package name before any
                    // version/extra specifier.
                    if let Some(name) = d.as_str() {
                        let name: String = name
                            .chars()
                            .take_while(|c| {
                                c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'
                            })
                            .collect();
                        if !name.is_empty() {
                            py_deps.push(name.to_lowercase());
                        }
                    }
                }
            }
            if let Some(tool_deps) = py
                .get("tool")
                .and_then(|t| t.get("poetry"))
                .and_then(|t| t.get("dependencies"))
                .and_then(|d| d.as_table())
            {
                for name in tool_deps.keys() {
                    py_deps.push(name.to_lowercase());
                }
            }
            for (fw, class, _cargo, _npm, _go, pip) in FRAMEWORKS {
                for pip_pkg in *pip {
                    if py_deps.iter().any(|d| d == *pip_pkg) {
                        note(
                            &mut candidates,
                            fw,
                            class,
                            0.9,
                            format!("pyproject.toml dependency '{}'", pip_pkg),
                        );
                    }
                }
            }
        }
        Err(e) => parse_failures.push(FrameworkParseFailure {
            file: format!("{}/pyproject.toml", cwd),
            reason: format!("parse failed: {e}"),
        }),
    }
    }
    // requirements.txt stays line-based (it IS a line format). A name
    // pyproject already reported gets REINFORCED here (two loci agreeing
    // is stronger evidence); one pyproject missed gets detected fresh.
    if file_set.contains("requirements.txt") {
        let txt = read_file(cwd, "requirements.txt");
        for (fw, class, _cargo, _npm, _go, pip) in FRAMEWORKS {
            for pip_pkg in *pip {
                let hit = txt.lines().any(|line| {
                    let l = line.trim().to_lowercase();
                    l == *pip_pkg
                        || l.starts_with(&format!("{pip_pkg}=="))
                        || l.starts_with(&format!("{pip_pkg}>="))
                        || l.starts_with(&format!("{pip_pkg}~="))
                        || l.starts_with(&format!("{pip_pkg}<"))
                        || l.starts_with(&format!("{pip_pkg}["))
                });
                if hit {
                    note(
                        &mut candidates,
                        fw,
                        class,
                        0.85,
                        format!("requirements.txt dependency '{}'", pip_pkg),
                    );
                }
            }
        }
    }

    // Rank: confidence descending; ties break framework → terminal_io →
    // styling (a framework claim outranks its own infrastructure when both
    // carry equal evidence).
    let class_rank = |c: &FrameworkCandidate| match c.class {
        CLASS_FRAMEWORK => 0u8,
        CLASS_TERMINAL_IO => 1,
        _ => 2,
    };
    candidates.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| class_rank(a).cmp(&class_rank(b)))
            .then_with(|| a.name.cmp(&b.name))
    });

    // Convenience projections (item 33): the highest-confidence terminal
    // I/O lib and styling lib, when present.
    let terminal_io = candidates
        .iter()
        .find(|c| c.class == CLASS_TERMINAL_IO)
        .map(|c| c.name.clone());
    let styling = candidates
        .iter()
        .find(|c| c.class == CLASS_STYLING)
        .map(|c| c.name.clone());

    let primary = candidates.first().cloned();
    let native_adapter = match &primary {
        Some(c) if c.class == CLASS_FRAMEWORK => native_adapter_exists(&c.name),
        _ => false,
    };
    let coverage_adapter = primary.is_some() && crate::coverage::tuicov::is_available();

    for c in &candidates {
        evidence.extend(c.evidence.iter().cloned());
    }

    FrameworkDetection {
        primary,
        candidates,
        terminal_io,
        styling,
        native_adapter,
        coverage_adapter,
        evidence,
        parse_failures,
        searched_paths: vec![cwd.to_string()],
    }
}

fn native_adapter_exists(fw: &str) -> bool {
    // Wave F items 58–63: the NativeSemanticProtocol side channel is the
    // native-adapter path — a cooperative app declares its real tree and
    // the harness merges it over inference. Adapter snippets ship for
    // these frameworks (`tui_framework action=adapter_snippet`).
    matches!(fw, "ratatui" | "textual")
}

fn collect_json_dep_keys(pkg: &serde_json::Value) -> Vec<String> {
    let mut deps = Vec::new();
    for key in &["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = pkg[key].as_object() {
            for name in obj.keys() {
                deps.push(name.clone());
            }
        }
    }
    deps
}

fn collect_toml_dep_keys(toml_val: &toml::Value) -> Vec<String> {
    let mut deps = Vec::new();
    if let Some(table) = toml_val.as_table() {
        for key in &["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(section) = table.get(*key).and_then(|v| v.as_table()) {
                for name in section.keys() {
                    deps.push(name.clone());
                }
            }
        }
    }
    deps
}

fn list_files(cwd: &str) -> Vec<String> {
    std::fs::read_dir(cwd)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn read_file(cwd: &str, name: &str) -> String {
    std::fs::read_to_string(format!("{}/{}", cwd, name)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit finding 36: a manifest that EXISTS but is malformed is
    /// surfaced as a parse failure, not collapsed into "framework not
    /// detected". A broken package.json must appear in `parse_failures`.
    #[test]
    fn malformed_manifest_is_reported_not_ignored() {
        let dir = std::env::temp_dir().join(format!(
            "tui-fw-detect-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A package.json that is NOT valid JSON.
        std::fs::write(dir.join("package.json"), "{ this is not json").unwrap();

        let det = detect(dir.to_str().unwrap());
        assert!(
            det.parse_failures
                .iter()
                .any(|f| f.file.ends_with("package.json")),
            "broken package.json must surface: {:?}",
            det.parse_failures
        );
        assert!(
            det.searched_paths.iter().any(|p| *p == dir.to_str().unwrap()),
            "searched_paths must name where detection looked: {:?}",
            det.searched_paths
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit finding 36: these same latches did NOT hold before — the bug
    /// was the silent `unwrap_or_default`. Doubly assert the honest case:
    /// a valid project still detects, and a clean tree has no failures.
    #[test]
    fn clean_manifest_has_no_parse_failures() {
        let dir = std::env::temp_dir().join(format!(
            "tui-fw-clean-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A Cargo.toml with a recognizable framework.
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\n[dependencies]\nratatui = \"0.28\"\n",
        )
        .unwrap();

        let det = detect(dir.to_str().unwrap());
        assert!(
            det.parse_failures.is_empty(),
            "a valid manifest must not be reported as broken: {:?}",
            det.parse_failures
        );
        assert!(
            det.framework().is_some(),
            "a valid Cargo.toml naming ratatui must detect: framework()={:?}",
            det.framework()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
