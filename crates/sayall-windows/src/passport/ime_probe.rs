//! P3.1：Passport 按住说话的输入法开麦观测（**只观测，不恢复**）。
//!
//! 判据全部复用 BLE 路径已有机制：
//! - `ime::mic_marker_for` 决定该工具在 `ConsentStore` 里的可观测条目；
//! - `wetype_revive::mic_observation_for` + `response_since` 判断条目是否在本次会话里前进；
//! - `key_suppressor::wetype_marker_count` + `wetype_revive::reaction_verdict` 合并出
//!   `reacted / not_reacted / unknown`（微信自注入标记是唯一正面标记）。
//!
//! 这里不新增判据，也不调用任何恢复动作。`failed` 仅在“两个证据源都确认未触发”
//! 且该工具确实存在自注入标记（目前只有微信）时给出；其他工具的
//! `not_reacted` 只是“没有正面证据”，沿用 issue #118 的结论归入 `unconfirmed`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sayall_core::settings::VoiceInputTool;

use crate::wetype_revive::{self, MicObservation, MicResponse, WetypeReaction};
use crate::{ime, key_suppressor};

/// 第一次观测（任务要求约 800ms）。
pub(crate) const EARLY_MS: u64 = 800;
/// 第二次观测（任务要求约 1500ms；比 BLE 的 2500ms 更早，只观测不裁决恢复）。
pub(crate) const LATE_MS: u64 = 1500;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Baseline {
    pub(crate) tool: &'static str,
    pub(crate) marker: &'static str,
    pub(crate) observation: Option<MicObservation>,
    pub(crate) marker_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// 第一次观测就有正面证据。
    Passed,
    /// 第一次没有、第二次确认。
    Delayed,
    /// 两个证据源都确认未触发，且该工具有自注入标记（仅微信）。
    Failed,
    /// 观察不到结论：不得据此判定失败。
    Unconfirmed,
}

impl Verdict {
    pub(crate) fn as_log_str(self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Delayed => "delayed",
            Verdict::Failed => "failed",
            Verdict::Unconfirmed => "unconfirmed",
        }
    }
}

/// `press` 之前调用：记录该工具的开麦基线与微信标记基线。
pub(crate) fn baseline(tool: Option<VoiceInputTool>) -> Option<Baseline> {
    let marker = ime::mic_marker_for(tool)?;
    Some(Baseline {
        tool: ime::tool_label(tool),
        marker,
        observation: wetype_revive::mic_observation_for(marker),
        marker_count: key_suppressor::wetype_marker_count(),
    })
}

/// 超出当前代际的迟到观测必须丢弃，避免污染新会话。
pub(crate) fn is_stale(generation: u64, current: u64) -> bool {
    generation != current
}

pub(crate) fn classify(tool: &str, early: WetypeReaction, late: WetypeReaction) -> Verdict {
    if early == WetypeReaction::Reacted {
        return Verdict::Passed;
    }
    if late == WetypeReaction::Reacted {
        return Verdict::Delayed;
    }
    // 只有微信存在自注入标记：两个窗口都 not_reacted 才是"两源确认未触发"。
    if tool == "wechat" && early == WetypeReaction::NotReacted && late == WetypeReaction::NotReacted
    {
        return Verdict::Failed;
    }
    Verdict::Unconfirmed
}

fn reaction_token(reaction: WetypeReaction) -> &'static str {
    match reaction {
        WetypeReaction::Reacted => "reacted",
        WetypeReaction::NotReacted => "not_reacted",
        WetypeReaction::Unknown => "unknown",
    }
}

/// 取一次当前观测并按既有规则合并出反应。
pub(crate) fn sample(baseline: &Baseline) -> (MicResponse, WetypeReaction) {
    let mic = wetype_revive::response_since(
        baseline.observation,
        wetype_revive::mic_observation_for(baseline.marker),
    );
    let reaction = wetype_revive::reaction_verdict(
        mic,
        baseline.marker_count,
        key_suppressor::wetype_marker_count(),
    );
    (mic, reaction)
}

/// 观测日志：正常进诊断日志；P3.1 数据采集时可用环境变量同时打印到 stderr
/// （`SAYALL_PASSPORT_IME_TRACE=1`，仅开发/验收用，不改变任何行为）。
pub(crate) fn note(line: String) {
    crate::gatt_note(line.clone());
    if std::env::var_os("SAYALL_PASSPORT_IME_TRACE").is_some() {
        eprintln!("[ime] {line}");
    }
}

/// 两次延迟观测；代际变化即停止，不写任何音频内容。
pub(crate) fn spawn(generation: u64, current: Arc<AtomicU64>, baseline: Baseline) {
    let _ = std::thread::Builder::new()
        .name("sayall-passport-ime".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let mut early = WetypeReaction::Unknown;
            for (index, target_ms) in [EARLY_MS, LATE_MS].into_iter().enumerate() {
                let elapsed = started.elapsed().as_millis() as u64;
                if elapsed < target_ms {
                    std::thread::sleep(Duration::from_millis(target_ms - elapsed));
                }
                if is_stale(generation, current.load(Ordering::SeqCst)) {
                    note(format!(
                        "passport voice_verify generation={generation} at_ms={target_ms} stale=true result=skipped"
                    ));
                    return;
                }
                let (mic, reaction) = sample(&baseline);
                note(format!(
                    "passport voice_verify generation={generation} at_ms={target_ms} tool={} marker={} mic={} reaction={}",
                    baseline.tool,
                    baseline.marker,
                    mic.as_log_str(),
                    reaction_token(reaction)
                ));
                if index == 0 {
                    early = reaction;
                } else {
                    let verdict = classify(baseline.tool, early, reaction);
                    note(format!(
                        "passport voice_verify generation={generation} verdict={} early={} late={}",
                        verdict.as_log_str(),
                        reaction_token(early),
                        reaction_token(reaction)
                    ));
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_prefers_positive_early_evidence() {
        assert_eq!(
            classify("wechat", WetypeReaction::Reacted, WetypeReaction::Reacted),
            Verdict::Passed
        );
        assert_eq!(
            classify("doubao", WetypeReaction::Reacted, WetypeReaction::Unknown),
            Verdict::Passed
        );
    }

    #[test]
    fn classification_reports_delayed_only_when_late_reacted() {
        assert_eq!(
            classify("wechat", WetypeReaction::Unknown, WetypeReaction::Reacted),
            Verdict::Delayed
        );
        assert_eq!(
            classify(
                "doubao",
                WetypeReaction::NotReacted,
                WetypeReaction::Reacted
            ),
            Verdict::Delayed
        );
    }

    #[test]
    fn failed_requires_two_source_confirmation_on_a_marker_tool() {
        // 微信：两个窗口都 not_reacted = 两源确认未触发。
        assert_eq!(
            classify(
                "wechat",
                WetypeReaction::NotReacted,
                WetypeReaction::NotReacted
            ),
            Verdict::Failed
        );
        // 豆包/Vokie 没有自注入标记：not_reacted 只是没有正面证据。
        assert_eq!(
            classify(
                "doubao",
                WetypeReaction::NotReacted,
                WetypeReaction::NotReacted
            ),
            Verdict::Unconfirmed
        );
        assert_eq!(
            classify(
                "vokie",
                WetypeReaction::NotReacted,
                WetypeReaction::NotReacted
            ),
            Verdict::Unconfirmed
        );
    }

    #[test]
    fn unknown_or_mixed_observations_stay_unconfirmed() {
        assert_eq!(
            classify("unknown", WetypeReaction::Unknown, WetypeReaction::Unknown),
            Verdict::Unconfirmed
        );
        assert_eq!(
            classify(
                "wechat",
                WetypeReaction::NotReacted,
                WetypeReaction::Unknown
            ),
            Verdict::Unconfirmed
        );
        assert_eq!(
            classify(
                "wechat",
                WetypeReaction::Unknown,
                WetypeReaction::NotReacted
            ),
            Verdict::Unconfirmed
        );
    }

    #[test]
    fn stale_generation_is_detected() {
        assert!(is_stale(4, 5));
        assert!(is_stale(4, 0));
        assert!(!is_stale(4, 4));
    }

    #[test]
    fn tools_without_an_observable_marker_have_no_baseline() {
        assert!(baseline(None).is_none());
        assert!(baseline(Some(VoiceInputTool::Other)).is_none());
    }
}
