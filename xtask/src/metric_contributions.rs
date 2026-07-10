// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Konstantin Vyatkin <tino@vtkn.io>

use std::process::Command;

/// Run the production analyzer through the real `mehen` binary, then print
/// only its contribution evidence. Keeping the analyzer graph behind the CLI
/// avoids making every lightweight xtask operation link all parser backends.
pub(crate) fn run(path: &str) -> Result<(), String> {
    let workspace = crate::tree_sitter::workspace_root().map_err(|err| err.to_string())?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(workspace)
        .args(["run", "--quiet", "-p", "mehen", "--", "metrics"])
        .arg(path)
        .args(["--profile", "default"])
        .output()
        .map_err(|err| format!("failed to run mehen: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "mehen metrics failed with {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let rendered = contributions_from_metrics_json(&output.stdout)?;
    println!("{rendered}");
    Ok(())
}

fn contributions_from_metrics_json(bytes: &[u8]) -> Result<String, String> {
    let report: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|err| format!("mehen returned invalid JSON: {err}"))?;
    let contributions = report
        .get("contributions")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    serde_json::to_string_pretty(&contributions)
        .map_err(|err| format!("failed to render contributions: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_contributions_or_an_empty_array() {
        let present = br#"{"contributions":[{"metric":"risk","amount":8}]}"#;
        let rendered = contributions_from_metrics_json(present).unwrap();
        assert!(rendered.contains("\"metric\": \"risk\""));

        let absent = br#"{"schema_version":"1.0"}"#;
        assert_eq!(contributions_from_metrics_json(absent).unwrap(), "[]");
    }
}
