//! Self-update check against GitHub releases (cf. XDM updater).
//!
//! Read-only: reports the newest tag. Set `update_repo` in config to
//! `owner/name` to enable; downloads/installs stay manual (CI artifacts).

use serde::{Deserialize, Serialize};

use crate::{CcdmError, Result};

/// What the newest release looks like.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag: String,
    pub name: String,
    pub url: String,
}

/// Fetch the newest release of `owner/name` from the GitHub API.
pub async fn latest_release(
    repo: &str,
    client: &reqwest::Client,
) -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(CcdmError::from)?;
    if !resp.status().is_success() {
        return Err(CcdmError::Http(format!(
            "release check failed with HTTP {}",
            resp.status()
        )));
    }
    let text = resp.text().await.map_err(CcdmError::from)?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(CcdmError::from)?;
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if tag.is_empty() {
        return Err(CcdmError::Other("no releases found".to_string()));
    }
    Ok(ReleaseInfo {
        tag,
        name: value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        url: value
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Numeric dot-separated compare ignoring a leading `v`
/// (`is_newer("0.8.0", "v0.9")` is true).
pub fn is_newer(current: &str, latest: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (current, latest) = (parts(current), parts(latest));
    let width = current.len().max(latest.len()).max(1);
    for i in 0..width {
        let (a, b) = (
            *current.get(i).unwrap_or(&0),
            *latest.get(i).unwrap_or(&0),
        );
        if a != b {
            return b > a;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_logic() {
        assert!(is_newer("0.8.0", "v0.9"));
        assert!(is_newer("0.9.0", "0.9.1"));
        assert!(!is_newer("0.9.0", "0.9.0"));
        assert!(!is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("1.0", "0.99.99"));
    }
}
