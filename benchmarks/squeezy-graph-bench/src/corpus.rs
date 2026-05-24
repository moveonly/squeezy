use std::{fs, path::PathBuf, process::Command};

use serde::Deserialize;
use squeezy_core::{Result, SqueezyError};

use crate::report::CorpusCaseReport;

#[derive(Debug, Deserialize)]
pub(crate) struct CorpusManifest {
    pub(crate) version: u32,
    pub(crate) cases: Vec<CorpusCase>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CorpusCase {
    pub(crate) name: String,
    pub(crate) family: String,
    pub(crate) language: String,
    pub(crate) tier: String,
    pub(crate) fixture: String,
    pub(crate) spec: String,
    pub(crate) report: String,
    #[serde(default)]
    pub(crate) mixed_repo: Option<String>,
    #[serde(default)]
    pub(crate) mixed_iterations: Option<usize>,
    #[serde(default)]
    pub(crate) ra_lsp_probes: Option<usize>,
    #[serde(default)]
    pub(crate) oracle_files: Option<usize>,
    #[serde(default)]
    pub(crate) no_speed_gate: bool,
    #[serde(default)]
    pub(crate) repo: Option<CorpusRepo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CorpusRepo {
    pub(crate) url: String,
    pub(crate) rev: String,
    pub(crate) checkout: String,
}

impl CorpusManifest {
    pub(crate) fn load(path: &std::path::Path) -> Result<Self> {
        let text = fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&text)
            .map_err(|err| SqueezyError::Graph(format!("invalid corpus manifest: {err}")))?;
        if manifest.version != 1 {
            return Err(SqueezyError::Graph(format!(
                "unsupported corpus manifest version {}",
                manifest.version
            )));
        }
        Ok(manifest)
    }
}

impl CorpusCase {
    pub(crate) fn matches(&self, family: &str, tier: &str) -> bool {
        let family_matches = family == "all" || self.family == family || self.language == family;
        let tier_matches = match tier {
            "smoke" => self.tier == "smoke",
            "full" => matches!(self.tier.as_str(), "smoke" | "full"),
            _ => false,
        };
        family_matches && tier_matches
    }

    pub(crate) fn report_case(&self) -> CorpusCaseReport {
        CorpusCaseReport {
            name: self.name.clone(),
            family: self.family.clone(),
            tier: self.tier.clone(),
            source_url: self.repo.as_ref().map(|repo| repo.url.clone()),
            source_ref: self.repo.as_ref().map(|repo| repo.rev.clone()),
        }
    }
}

pub(crate) fn ensure_repo(case: &CorpusCase) -> Result<()> {
    let Some(repo) = &case.repo else {
        return Ok(());
    };
    let checkout = PathBuf::from(&repo.checkout);
    if !checkout.exists() {
        if let Some(parent) = checkout.parent() {
            fs::create_dir_all(parent)?;
        }
        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--filter=blob:none",
                "--no-checkout",
                &repo.url,
            ])
            .arg(&checkout)
            .status()
            .map_err(|err| SqueezyError::Graph(format!("failed to spawn git clone: {err}")))?;
        if !status.success() {
            return Err(SqueezyError::Graph(format!(
                "git clone {} failed with {status}",
                repo.url
            )));
        }
    }

    let fetch = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["fetch", "--depth", "1", "origin", &repo.rev])
        .status()
        .map_err(|err| SqueezyError::Graph(format!("failed to spawn git fetch: {err}")))?;
    if !fetch.success() {
        return Err(SqueezyError::Graph(format!(
            "git fetch {} {} failed with {fetch}",
            repo.url, repo.rev
        )));
    }

    let checkout_status = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["checkout", "--detach", &repo.rev])
        .status()
        .map_err(|err| SqueezyError::Graph(format!("failed to spawn git checkout: {err}")))?;
    if !checkout_status.success() {
        return Err(SqueezyError::Graph(format!(
            "git checkout {} failed with {checkout_status}",
            repo.rev
        )));
    }
    Ok(())
}
