use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::cli::StatsArgs;
use crate::paths::ManagerPaths;

use super::db;
use super::rollout;
use super::CalibrationReport;
use super::TokenMix;
use super::TokenTotals;
use super::CALIBRATION_FILE;
use super::CALIBRATION_SCHEMA_VERSION;
use super::FALLBACK_TOKEN_MIX;
use super::STATE_DB;
use super::STATS_JSON_SCHEMA_VERSION;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct MixCalibration {
    #[serde(rename = "schemaVersion")]
    pub(super) schema_version: u64,
    #[serde(rename = "calibratedAt")]
    calibrated_at: i64,
    pub(super) samples: u64,
    #[serde(rename = "sourceRollouts")]
    source_rollouts: u64,
    #[serde(rename = "totalTokens")]
    total_tokens: u64,
    #[serde(rename = "tokenMix")]
    pub(super) token_mix: TokenMix,
}

impl MixCalibration {
    fn new(source_rollouts: u64, totals: &TokenTotals, token_mix: TokenMix) -> Self {
        Self {
            schema_version: CALIBRATION_SCHEMA_VERSION,
            calibrated_at: super::unix_now(),
            samples: totals.samples,
            source_rollouts,
            total_tokens: totals.total_tokens,
            token_mix,
        }
    }
}

pub fn calibrate_mix(paths: &ManagerPaths, args: StatsArgs) -> Result<CalibrationReport> {
    let slot_filters = args.slots.iter().cloned().collect::<BTreeSet<_>>();
    let db_paths = db::state_db_paths(paths, &slot_filters)?;
    if db_paths.is_empty() {
        anyhow::bail!("no Codex {STATE_DB} database found");
    }

    let mut rollout_paths = BTreeMap::new();
    for db_path in &db_paths {
        for rollout_path in db::read_rollout_paths(db_path, paths, &slot_filters)? {
            if !rollout_path.exists() {
                continue;
            }
            let canonical = fs::canonicalize(&rollout_path)
                .with_context(|| format!("resolve {}", rollout_path.display()))?;
            rollout_paths.entry(canonical).or_insert(rollout_path);
        }
    }

    let mut totals = TokenTotals::default();
    for rollout_path in rollout_paths.values() {
        if let Some(usage) = rollout::read_final_token_usage(rollout_path)? {
            totals.add(usage);
        }
    }
    if totals.samples == 0 {
        anyhow::bail!("no rollout token_count samples found");
    }

    let token_mix = totals.token_mix();
    let calibration = MixCalibration::new(rollout_paths.len() as u64, &totals, token_mix);
    let saved_to = write_mix_calibration(paths, &calibration)?;

    Ok(CalibrationReport {
        schema_version: STATS_JSON_SCHEMA_VERSION,
        json: args.json,
        saved_to: saved_to.display().to_string(),
        source_databases: db_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        source_rollouts: rollout_paths.len() as u64,
        samples: totals.samples,
        total_tokens: totals.total_tokens,
        uncached_input_tokens: totals.uncached_input_tokens,
        cached_input_tokens: totals.cached_input_tokens,
        output_tokens: totals.output_tokens,
        token_mix,
    })
}

pub(super) fn load_token_mix(paths: &ManagerPaths) -> (TokenMix, String) {
    let path = paths.manager_dir.join(CALIBRATION_FILE);
    let Some(calibration) = read_mix_calibration(&path) else {
        return (FALLBACK_TOKEN_MIX, "built-in fallback".to_string());
    };
    if calibration.samples == 0 || !calibration.token_mix.valid() {
        return (FALLBACK_TOKEN_MIX, "built-in fallback".to_string());
    }
    (
        calibration.token_mix,
        format!(
            "calibration: {} ({} samples)",
            path.display(),
            calibration.samples
        ),
    )
}

pub(super) fn read_mix_calibration(path: &Path) -> Option<MixCalibration> {
    let content = fs::read_to_string(path).ok()?;
    parse_mix_calibration(&content)
}

fn write_mix_calibration(paths: &ManagerPaths, calibration: &MixCalibration) -> Result<PathBuf> {
    fs::create_dir_all(&paths.manager_dir)
        .with_context(|| format!("create {}", paths.manager_dir.display()))?;
    let path = paths.manager_dir.join(CALIBRATION_FILE);
    write_mix_calibration_path(&path, calibration)?;
    Ok(path)
}

fn write_mix_calibration_path(path: &Path, calibration: &MixCalibration) -> Result<()> {
    let content = serde_json::to_string_pretty(calibration)?;
    fs::write(path, content).with_context(|| format!("write {}", path.display()))
}

pub(super) fn parse_mix_calibration(content: &str) -> Option<MixCalibration> {
    let calibration = serde_json::from_str::<MixCalibration>(content).ok()?;
    (calibration.schema_version == CALIBRATION_SCHEMA_VERSION).then_some(calibration)
}
