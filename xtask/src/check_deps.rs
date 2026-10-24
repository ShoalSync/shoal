//! Enforces the crate boundaries documented in `CONVENTIONS.md`.
//!
//! Only normal and build dependencies count: dev-dependencies are not shipped.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

/// Every workspace crate's licence (ADR 0044).
const LICENSE: &str = "MIT OR Apache-2.0";

/// Crates that `shoal-engine` must never depend on, directly or transitively.
const ENGINE_FORBIDDEN: &[&str] = &["shoal-fs-fuse", "shoal-fs-cfapi", "shoal-web"];

/// The only workspace crates `shoal-ffi` may depend on directly.
const FFI_ALLOWED: &[&str] = &["shoal-sdk", "shoal-engine"];

struct Workspace {
    /// Package name to license expression, for workspace members.
    licenses: BTreeMap<String, String>,
    /// Package name to names of its shipped dependencies that are workspace members.
    deps: BTreeMap<String, BTreeSet<String>>,
}

pub fn run() -> Result<(), String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--locked"])
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let metadata: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("invalid cargo metadata: {e}"))?;
    let ws = Workspace::from_metadata(&metadata)?;

    let violations = check(&ws);
    if violations.is_empty() {
        println!("check-deps: {} crates OK", ws.licenses.len());
        Ok(())
    } else {
        Err(format!(
            "check-deps: {} violation(s)\n  {}",
            violations.len(),
            violations.join("\n  ")
        ))
    }
}

impl Workspace {
    fn from_metadata(metadata: &Value) -> Result<Self, String> {
        let members: BTreeSet<&str> = metadata["workspace_members"]
            .as_array()
            .ok_or("missing workspace_members")?
            .iter()
            .filter_map(Value::as_str)
            .collect();

        let mut id_to_name = BTreeMap::new();
        let mut licenses = BTreeMap::new();
        for pkg in metadata["packages"].as_array().ok_or("missing packages")? {
            let id = pkg["id"].as_str().ok_or("package without id")?;
            if members.contains(id) {
                let name = pkg["name"].as_str().ok_or("package without name")?;
                id_to_name.insert(id, name.to_owned());
                licenses.insert(
                    name.to_owned(),
                    pkg["license"].as_str().unwrap_or_default().to_owned(),
                );
            }
        }

        let mut deps = BTreeMap::new();
        for node in metadata["resolve"]["nodes"]
            .as_array()
            .ok_or("missing resolve graph")?
        {
            let Some(name) = node["id"].as_str().and_then(|id| id_to_name.get(id)) else {
                continue;
            };
            let shipped = node["deps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|dep| {
                    dep["dep_kinds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|k| k["kind"].as_str() != Some("dev"))
                })
                .filter_map(|dep| dep["pkg"].as_str().and_then(|id| id_to_name.get(id)))
                .cloned()
                .collect();
            deps.insert(name.clone(), shipped);
        }
        Ok(Self { licenses, deps })
    }

    fn closure(&self, root: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root.to_owned()];
        while let Some(name) = stack.pop() {
            for dep in self.deps.get(&name).into_iter().flatten() {
                if seen.insert(dep.clone()) {
                    stack.push(dep.clone());
                }
            }
        }
        seen
    }
}

fn check(ws: &Workspace) -> Vec<String> {
    let mut violations = Vec::new();

    for (name, license) in &ws.licenses {
        if license != LICENSE {
            violations.push(format!(
                "{name}: license is {license:?}, expected {LICENSE:?}"
            ));
        }
    }

    for dep in ws.closure("shoal-engine") {
        if ENGINE_FORBIDDEN.contains(&dep.as_str()) {
            violations.push(format!("shoal-engine: must not depend on {dep}"));
        }
    }

    for dep in ws.deps.get("shoal-ffi").into_iter().flatten() {
        if !FFI_ALLOWED.contains(&dep.as_str()) {
            violations.push(format!(
                "shoal-ffi: may depend only on {FFI_ALLOWED:?}, found {dep}"
            ));
        }
    }

    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(edges: &[(&str, &str)]) -> Workspace {
        let names = [
            "shoal-proto",
            "shoal-sdk",
            "shoal-store",
            "shoal-engine",
            "shoal-web",
            "shoal-fs-fuse",
            "shoal-ffi",
        ];
        let licenses = names
            .iter()
            .map(|n| ((*n).to_owned(), LICENSE.to_owned()))
            .collect();
        let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (from, to) in edges {
            deps.entry((*from).to_owned())
                .or_default()
                .insert((*to).to_owned());
        }
        Workspace { licenses, deps }
    }

    #[test]
    fn clean_graph_passes() {
        let ws = ws(&[
            ("shoal-sdk", "shoal-proto"),
            ("shoal-engine", "shoal-store"),
            ("shoal-ffi", "shoal-sdk"),
            ("shoal-ffi", "shoal-engine"),
        ]);
        assert_eq!(check(&ws), Vec::<String>::new());
    }

    #[test]
    fn engine_cannot_reach_web_transitively() {
        let ws = ws(&[
            ("shoal-engine", "shoal-store"),
            ("shoal-store", "shoal-web"),
        ]);
        assert_eq!(
            check(&ws),
            vec!["shoal-engine: must not depend on shoal-web"]
        );
    }

    #[test]
    fn ffi_limited_to_sdk_and_engine() {
        let ws = ws(&[("shoal-ffi", "shoal-store")]);
        assert_eq!(check(&ws).len(), 1);
    }

    #[test]
    fn wrong_license_is_reported() {
        let mut ws = ws(&[]);
        ws.licenses
            .insert("shoal-store".into(), "AGPL-3.0-only".into());
        assert_eq!(check(&ws).len(), 1);
    }
}
