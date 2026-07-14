use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sampler::SampleSummary;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfReport {
    pub run_id: String,
    pub git: GitInfo,
    pub platform: PlatformInfo,
    pub build: BuildInfo,
    pub scenarios: Vec<ScenarioResult>,
    pub regressions: Vec<Regression>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub head: String,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub id: String,
    pub duration_s: u64,
    pub samples: usize,
    /// macOS `phys_footprint` (MB) — the metric that matters, not `ps rss`.
    pub footprint_mb: MetricStats,
    /// Average wakeups/sec over the window (macOS energy metric vs Discord).
    pub wakeups_per_s: f32,
    /// Average CPU (% of one core) over the window.
    pub cpu_pct: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mello_stats_last: Option<mello_core::MelloStats>,
}

impl ScenarioResult {
    pub fn from_summary(id: String, duration_s: u64, s: &SampleSummary) -> Self {
        ScenarioResult {
            id,
            duration_s,
            samples: s.samples,
            footprint_mb: MetricStats {
                min: s.footprint_mb_min,
                p50: s.footprint_mb_p50,
                p95: s.footprint_mb_p95,
                max: s.footprint_mb_max,
            },
            wakeups_per_s: s.wakeups_per_s,
            cpu_pct: s.cpu_pct,
            mello_stats_last: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricStats {
    pub min: f32,
    pub p50: f32,
    pub p95: f32,
    pub max: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Regression {
    pub scenario: String,
    pub metric: String,
    pub baseline: f32,
    pub actual: f32,
    pub tolerance_pct: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineFile {
    pub platform: PlatformInfo,
    pub scenarios: BTreeMap<String, BaselineScenario>,
    pub tolerances: Tolerances,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineScenario {
    pub footprint_mb: BaselineMetric,
    pub wakeups_per_s: f32,
    pub cpu_pct: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineMetric {
    pub p50: f32,
    pub p95: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tolerances {
    pub footprint_mb_p95_pct: f32,
    pub wakeups_per_s_pct: f32,
    pub wakeups_per_s_abs: f32,
    pub cpu_pct_pct: f32,
    pub cpu_pct_abs: f32,
}

impl Default for Tolerances {
    fn default() -> Self {
        Self {
            footprint_mb_p95_pct: 10.0,
            wakeups_per_s_pct: 25.0,
            wakeups_per_s_abs: 3.0,
            cpu_pct_pct: 30.0,
            cpu_pct_abs: 1.5,
        }
    }
}

pub fn compare_report(report: &PerfReport, baseline: &BaselineFile) -> Vec<Regression> {
    let tol = &baseline.tolerances;
    let mut regressions = Vec::new();

    for scenario in &report.scenarios {
        let Some(base) = baseline.scenarios.get(&scenario.id) else {
            continue;
        };

        if base.footprint_mb.p95 > 0.0 {
            let limit = base.footprint_mb.p95 * (1.0 + tol.footprint_mb_p95_pct / 100.0);
            if scenario.footprint_mb.p95 > limit {
                regressions.push(Regression {
                    scenario: scenario.id.clone(),
                    metric: "footprint_mb_p95".to_string(),
                    baseline: base.footprint_mb.p95,
                    actual: scenario.footprint_mb.p95,
                    tolerance_pct: tol.footprint_mb_p95_pct,
                });
            }
        }

        // Wakeups/CPU are rates: allow the greater of a relative or absolute slack
        // so tiny baselines don't trip on noise.
        let wk_limit = (base.wakeups_per_s * (1.0 + tol.wakeups_per_s_pct / 100.0))
            .max(base.wakeups_per_s + tol.wakeups_per_s_abs);
        if scenario.wakeups_per_s > wk_limit {
            regressions.push(Regression {
                scenario: scenario.id.clone(),
                metric: "wakeups_per_s".to_string(),
                baseline: base.wakeups_per_s,
                actual: scenario.wakeups_per_s,
                tolerance_pct: tol.wakeups_per_s_pct,
            });
        }

        let cpu_limit =
            (base.cpu_pct * (1.0 + tol.cpu_pct_pct / 100.0)).max(base.cpu_pct + tol.cpu_pct_abs);
        if scenario.cpu_pct > cpu_limit {
            regressions.push(Regression {
                scenario: scenario.id.clone(),
                metric: "cpu_pct".to_string(),
                baseline: base.cpu_pct,
                actual: scenario.cpu_pct,
                tolerance_pct: tol.cpu_pct_pct,
            });
        }
    }

    regressions
}

pub fn report_to_baseline(report: &PerfReport) -> BaselineFile {
    let mut scenarios = BTreeMap::new();
    for s in &report.scenarios {
        scenarios.insert(
            s.id.clone(),
            BaselineScenario {
                footprint_mb: BaselineMetric {
                    p50: s.footprint_mb.p50,
                    p95: s.footprint_mb.p95,
                },
                wakeups_per_s: s.wakeups_per_s,
                cpu_pct: s.cpu_pct,
            },
        );
    }
    BaselineFile {
        platform: report.platform.clone(),
        scenarios,
        tolerances: Tolerances::default(),
    }
}

pub fn write_summary_md(path: &std::path::Path, report: &PerfReport) {
    use std::io::Write;

    let mut f = std::fs::File::create(path).expect("create summary.md");
    writeln!(f, "# Perf harness report\n").ok();
    writeln!(f, "- **run_id:** {}", report.run_id).ok();
    writeln!(f, "- **git:** {} ({})", report.git.head, report.git.branch).ok();
    writeln!(
        f,
        "- **platform:** {} / {}",
        report.platform.os, report.platform.arch
    )
    .ok();
    writeln!(f, "\n## Scenarios\n").ok();
    for s in &report.scenarios {
        writeln!(f, "### {}\n", s.id).ok();
        writeln!(
            f,
            "- Footprint MB: min={:.1} p50={:.1} p95={:.1} max={:.1}",
            s.footprint_mb.min, s.footprint_mb.p50, s.footprint_mb.p95, s.footprint_mb.max
        )
        .ok();
        writeln!(f, "- Wakeups/s: {:.1}", s.wakeups_per_s).ok();
        writeln!(f, "- CPU %: {:.1}", s.cpu_pct).ok();
    }
    if report.regressions.is_empty() {
        writeln!(f, "\n## Result: PASS\n").ok();
    } else {
        writeln!(f, "\n## Result: FAIL\n").ok();
        for r in &report.regressions {
            writeln!(
                f,
                "- **{}** {}: baseline={:.2} actual={:.2} (tol +{:.0}%)",
                r.scenario, r.metric, r.baseline, r.actual, r.tolerance_pct
            )
            .ok();
        }
    }
}
