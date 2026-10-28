use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::herdr::Client;

const PROGRAM: &str = "godview";

/// Focuses the running godview pane, or opens one as a tab. `herdr plugin pane` lists
/// nothing, so the pane is found by the program running in it, which also keeps a single
/// instance across workspaces.
pub fn open(client: &Client) -> Result<()> {
    for pane in client.pane_ids()? {
        let programs = client.foreground_programs(&pane).unwrap_or_default();
        if programs.iter().any(|p| p == PROGRAM) {
            return client.focus_pane(&pane);
        }
    }
    let herdr = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let out = Command::new(&herdr)
        .args(["plugin", "pane", "open", "--plugin", "godview"])
        .args(["--entrypoint", "view", "--placement", "tab", "--focus"])
        .output()
        .context("run herdr plugin pane open")?;
    if !out.status.success() {
        bail!(
            "herdr plugin pane open: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let answer: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if let Some(tab) = answer["result"]["plugin_pane"]["pane"]["tab_id"].as_str() {
        let renamed = Command::new(&herdr)
            .args(["tab", "rename", tab, PROGRAM])
            .output()
            .context("run herdr tab rename")?;
        if !renamed.status.success() {
            crate::log::warn(&format!(
                "tab rename: {}",
                String::from_utf8_lossy(&renamed.stderr).trim()
            ));
        }
    }
    Ok(())
}
