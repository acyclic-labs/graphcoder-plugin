//! Renders the previous-session brief as agent-readable text.
//!
//! Budget: under 1KB, always. The `SessionStart` hook prints this into the
//! agent's context on every session start, so it must earn its bytes: where
//! the last session ended, what it changed, which branches it abandoned, and
//! the verbs that reach the rest.

use crate::proto;
use acyclic_engine::product::NAME;

/// Hard cap on the rendered text, including the trailing newline.
pub const BUDGET_BYTES: usize = 1000;

/// Share of the budget a generated summary may take. Roughly a third: it
/// earns a place, but the counts and the abandoned branches are facts and
/// this is prose.
const SUMMARY_BYTES: usize = 320;

pub fn render(info: &proto::BriefInfo) -> String {
    let Some(session) = &info.session else {
        return format!("{NAME}: no previous session on record for this repo.\n");
    };
    let mut lines = Vec::new();
    let ended = match session.ended_at {
        Some(at) => format!("ended {}", age(at)),
        None => "did not end cleanly".to_owned(),
    };
    let end = match session.end_checkpoint {
        Some(id) => format!(" at checkpoint #{id}"),
        None => String::new(),
    };
    let end_turn = match (session.end_turn, &session.end_prompt) {
        (Some(turn), Some(prompt)) => format!(" (turn {turn}: {})", quote(prompt, 80)),
        (Some(turn), None) => format!(" (turn {turn})"),
        _ => String::new(),
    };
    lines.push(format!(
        "{NAME}: last session {}{} {ended}{end}{end_turn}.",
        short(&session.session_id),
        session
            .host
            .as_deref()
            .map(|host| format!(" ({host})"))
            .unwrap_or_default(),
    ));
    let sample = if session.sample_paths.is_empty() {
        String::new()
    } else {
        let listed = u64::try_from(session.sample_paths.len()).unwrap_or(u64::MAX);
        let more = session.files_changed > listed;
        format!(
            " ({}{})",
            session.sample_paths.join(", "),
            if more { ", …" } else { "" }
        )
    };
    lines.push(format!(
        "  {} turns, {} checkpoints, {} files changed{sample}.",
        session.turns, session.checkpoints, session.files_changed
    ));
    // High in the brief and bounded: the most useful line here, but it is
    // generated prose, so it must not be able to crowd out the facts below
    // it. `fit` trims branch detail from the end, never this.
    if let Some(summary) = session.summary.as_ref().filter(|text| !text.is_empty()) {
        lines.push(format!("  last turn: {}", bound(summary, SUMMARY_BYTES)));
    }
    if session.abandoned.is_empty() {
        lines.push("  no abandoned branches.".to_owned());
    } else {
        lines.push(format!(
            "  {} abandoned branch(es):",
            session.abandoned.len()
        ));
        for branch in &session.abandoned {
            let turn = match (branch.turn, &branch.prompt) {
                (Some(turn), Some(prompt)) => format!(" turn {turn} {}", quote(prompt, 60)),
                (Some(turn), None) => format!(" turn {turn}"),
                _ => String::new(),
            };
            lines.push(format!(
                "    #{}..#{}{turn}: {} checkpoints, {} files; rewound to #{}. \
                 `{NAME} diff {} {}` shows it.",
                branch.from_checkpoint,
                branch.to_checkpoint,
                branch.checkpoints,
                branch.files_changed,
                branch.rewound_to,
                branch.rewound_to,
                branch.to_checkpoint,
            ));
        }
    }
    if info.drift_files > 0 {
        lines.push(format!(
            "  tree has moved since: {} files differ from that end state.",
            info.drift_files
        ));
    } else {
        lines.push("  tree unchanged since then.".to_owned());
    }
    lines.push(format!(
        "  verbs: {NAME} turns · timeline · diff --turn N · show <id> · \
         restore <id> <path> · rewind <id>"
    ));
    fit(lines)
}

/// Joins lines under [`BUDGET_BYTES`], dropping abandoned-branch detail
/// lines from the end first (they are the only unbounded part), then
/// truncating the last surviving line.
fn fit(mut lines: Vec<String>) -> String {
    let total = |lines: &[String]| lines.iter().map(|line| line.len() + 1).sum::<usize>();
    while total(&lines) > BUDGET_BYTES && lines.len() > 4 {
        // Remove the last branch line (index len-3: before drift and verbs).
        let branch_index = lines.len().saturating_sub(3);
        if lines
            .get(branch_index)
            .is_some_and(|line| line.starts_with("    #"))
        {
            lines.remove(branch_index);
            let listed = lines
                .iter()
                .filter(|line| line.starts_with("    #"))
                .count();
            if let Some(header) = lines
                .iter_mut()
                .find(|line| line.contains("abandoned branch"))
            {
                *header = format!("  {listed}+ abandoned branches (more not shown):");
            }
        } else {
            break;
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    if text.len() > BUDGET_BYTES {
        let mut cut = BUDGET_BYTES - 2;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("…\n");
    }
    text
}

/// Excerpt bounded to `max` bytes on a char boundary, whitespace collapsed.
fn bound(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max {
        return collapsed;
    }
    let mut cut = max.saturating_sub(1);
    while cut > 0 && !collapsed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", collapsed.get(..cut).unwrap_or(&collapsed))
}

/// Quoted excerpt bounded to `max` bytes on a char boundary.
pub fn quote(prompt: &str, max: usize) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= max {
        return format!("\"{collapsed}\"");
    }
    let mut cut = max.saturating_sub(1);
    while cut > 0 && !collapsed.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("\"{}…\"", collapsed.get(..cut).unwrap_or(&collapsed))
}

fn short(session_id: &str) -> String {
    let mut short: String = session_id.chars().take(8).collect();
    if session_id.chars().count() > 8 {
        short.push('…');
    }
    short
}

fn age(at: i64) -> String {
    let delta = (acyclic_engine::unix_now() - at).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(abandoned: usize) -> proto::BriefInfo {
        summarized_session(abandoned, None)
    }

    fn summarized_session(abandoned: usize, summary: Option<String>) -> proto::BriefInfo {
        proto::BriefInfo {
            session: Some(proto::BriefSession {
                session_id: "0123456789abcdef".into(),
                host: Some("claude-code".into()),
                started_at: 0,
                ended_at: Some(0),
                turns: 9,
                checkpoints: 41,
                end_checkpoint: Some(41),
                end_turn: Some(9),
                end_prompt: Some("JWT refactor, approach 3, run the tests".repeat(4)),
                files_changed: 12,
                sample_paths: vec!["src/auth.rs".into(), "src/jwt.rs".into()],
                summary,
                abandoned: (0..abandoned)
                    .map(|index| proto::BriefAbandoned {
                        from_checkpoint: 10 + index as i64 * 10,
                        to_checkpoint: 19 + index as i64 * 10,
                        rewound_to: 9 + index as i64 * 10,
                        turn: Some(index as i64 + 2),
                        prompt: Some("try approach number ".repeat(6)),
                        checkpoints: 9,
                        files_changed: 4,
                    })
                    .collect(),
            }),
            drift_files: 3,
        }
    }

    /// A summary is model output, so its length is not ours to trust. The
    /// brief's byte budget is a hard contract with the host — it is printed
    /// straight into the agent's context — so a rambling summary must be cut
    /// rather than allowed to push the facts out.
    #[test]
    fn a_long_summary_cannot_blow_the_budget() {
        let rambling = "it refactored the whole authentication layer ".repeat(80);
        for abandoned in [0, 3, 25] {
            let text = render(&summarized_session(abandoned, Some(rambling.clone())));
            assert!(
                text.len() <= BUDGET_BYTES,
                "{abandoned} branches: {} bytes",
                text.len()
            );
            assert!(text.contains("last turn:"), "the summary should survive");
            // And the facts it sits above survive with it.
            assert!(
                text.contains("checkpoints"),
                "counts must not be crowded out"
            );
        }
    }

    #[test]
    fn an_empty_summary_prints_nothing() {
        let text = render(&summarized_session(1, Some(String::new())));
        assert!(!text.contains("last turn:"), "{text}");
    }

    #[test]
    fn renders_under_budget_however_many_branches() {
        for abandoned in [0, 1, 3, 25] {
            let text = render(&session(abandoned));
            assert!(
                text.len() <= BUDGET_BYTES,
                "{abandoned}: {} bytes",
                text.len()
            );
            assert!(text.contains("last session 01234567…"));
            assert!(text.contains("checkpoint #41"));
            assert!(text.contains("3 files differ"));
            assert!(text.ends_with('\n'));
        }
        let text = render(&session(2));
        assert!(text.contains("2 abandoned branch(es)"));
        assert!(text.contains("#10..#19 turn 2"));
        assert!(text.contains("rewound to #9"));
    }

    #[test]
    fn no_session_is_one_line() {
        let text = render(&proto::BriefInfo::default());
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn quote_bounds_on_char_boundary() {
        let quoted = quote(&"é".repeat(100), 20);
        assert!(quoted.len() <= 20 + "\"\"…".len());
        assert!(quoted.ends_with("…\""));
    }
}
