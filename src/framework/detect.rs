//! Framework detection (spec section 26). Inspect the target source tree for
//! build manifests and known framework crates, then look for native adapters.
//!
//! IMPORTANT (spec items 64-66):
//!   * Do not use raw substring matching against manifests — parse them.
//!   * Crossterm is terminal I/O infrastructure, NOT a framework.
//!   * `@opentui` is its own framework, not Textual.
//!   * `native_adapter = true` only when an adapter module actually exists.

use std::collections::HashSet;

#[derive(Debug, serde::Serialize)]
pub struct FrameworkDetection {
    pub framework: Option<String>,
    pub confidence: f32,
    pub native_adapter: bool,
    pub coverage_adapter: bool,
    pub evidence: Vec<String>,
}

/// Known TUI frameworks and their identifying package/crate names.
/// (name, manifest markers, dependency markers, extra hints, secondary libs)
type FrameworkSpec = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
);

const FRAMEWORKS: &[FrameworkSpec] = &[
    ("ratatui", &["ratatui"], &[], &[], &[]),
    ("textual", &[], &["textual"], &[], &["textual"]),
    (
        "opentui",
        &[],
        &["@opentui/core", "@opentui/react"],
        &[],
        &[],
    ),
    ("ink", &[], &["ink"], &[], &[]),
    (
        "bubbletea",
        &[],
        &[],
        &["github.com/charmbracelet/bubbletea"],
        &[],
    ),
    ("crossterm", &["crossterm"], &[], &[], &[]),
    (
        "lipgloss",
        &[],
        &[],
        &["github.com/charmbracelet/lipgloss"],
        &[],
    ),
];

pub fn detect(cwd: &str) -> FrameworkDetection {
    let mut evidence = Vec::new();
    let mut detected: Option<(String, f32)> = None;

    let files = list_files(cwd);
    let file_set: HashSet<&str> = files.iter().map(|s| s.as_str()).collect();

    // Parse package.json properly.
    if file_set.contains("package.json") {
        let txt = read_file(cwd, "package.json");
        let pkg: serde_json::Value = serde_json::from_str(&txt).unwrap_or_default();
        let all_deps = collect_json_dep_keys(&pkg);
        for (fw, _cargo, npm, _go, _pip) in FRAMEWORKS {
            for npm_pkg in *npm {
                if all_deps.iter().any(|d| d.as_str() == *npm_pkg) {
                    let confidence = if *npm_pkg == "@opentui/core" || *npm_pkg == "@opentui/react"
                    {
                        1.0
                    } else {
                        0.9
                    };
                    detected = Some((fw.to_string(), confidence));
                    evidence.push(format!("package.json dependency '{}'", npm_pkg));
                    break;
                }
            }
        }
    }

    // Parse Cargo.toml properly.
    if file_set.contains("Cargo.toml") {
        let txt = read_file(cwd, "Cargo.toml");
        if let Ok(toml_val) = txt.parse::<toml::Value>() {
            let all_deps = collect_toml_dep_keys(&toml_val);
            for (fw, cargo, _npm, _go, _pip) in FRAMEWORKS {
                for crate_name in *cargo {
                    if all_deps.iter().any(|d| d.as_str() == *crate_name) {
                        if *crate_name == "crossterm" {
                            if detected.is_none() {
                                evidence.push(
                                    "Cargo.toml references crossterm (terminal I/O lib)"
                                        .to_string(),
                                );
                            }
                            continue;
                        }
                        let confidence = if *crate_name == "ratatui" { 1.0 } else { 0.9 };
                        detected = Some((fw.to_string(), confidence));
                        evidence.push(format!("Cargo.toml dependency '{}'", crate_name));
                        break;
                    }
                }
            }
        }
    }

    // Parse go.mod for Go frameworks.
    if file_set.contains("go.mod") {
        let txt = read_file(cwd, "go.mod");
        for (fw, _cargo, _npm, go_modules, _pip) in FRAMEWORKS {
            for go_mod in *go_modules {
                if txt
                    .lines()
                    .any(|line| line.trim().starts_with(go_mod) || line.contains(go_mod))
                {
                    detected = Some((fw.to_string(), 0.9));
                    evidence.push(format!("go.mod references '{}'", go_mod));
                    break;
                }
            }
        }
    }

    // Parse Python project manifests.
    if file_set.contains("pyproject.toml") || file_set.contains("requirements.txt") {
        let txt = read_file(cwd, "pyproject.toml") + &read_file(cwd, "requirements.txt");
        for (fw, _cargo, _npm, _go, pip) in FRAMEWORKS {
            for pip_pkg in *pip {
                // Match package name with various version specifiers, handling
                // both quoted (pyproject) and unquoted (requirements.txt) forms.
                let patterns = [
                    pip_pkg.to_string(),
                    format!("{}==", pip_pkg),
                    format!("{}>=", pip_pkg),
                    format!("{}>=", pip_pkg),
                    format!("{}\"", pip_pkg),
                    format!("{}',", pip_pkg),
                    format!("{}\",", pip_pkg),
                ];
                if txt.lines().any(|line| {
                    let l = line.trim();
                    patterns.iter().any(|p| l == *p || l.starts_with(p))
                }) {
                    detected = Some((fw.to_string(), 0.9));
                    evidence.push(format!("python manifest references '{}'", pip_pkg));
                    break;
                }
            }
        }
    }

    let native_adapter = match &detected {
        Some((fw, _)) => native_adapter_exists(fw),
        None => false,
    };

    let coverage_adapter = detected.is_some() && crate::coverage::tuicov::is_available();

    let (framework, confidence) = match detected {
        Some((fw, c)) => (Some(fw), c),
        None => (None, 0.0),
    };

    FrameworkDetection {
        framework,
        confidence,
        native_adapter,
        coverage_adapter,
        evidence,
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
