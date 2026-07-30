use std::collections::BTreeMap;

use micromux::LogLine;
use micromux_control::Request;
use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::Serialize;

use crate::select::ToolError;
use crate::{LogFilters, SessionConn, convert, error_data, logproc, service_result};

#[derive(Serialize, JsonSchema)]
pub(crate) struct ServiceFollowGap {
    pub(crate) service: String,
    pub(crate) after_seq: u64,
    pub(crate) first_seq: u64,
    pub(crate) lost_entries_at_least: u64,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct FollowGap {
    pub(crate) after_seq: u64,
    pub(crate) first_seq: u64,
    pub(crate) lost_entries_at_least: u64,
}

pub(crate) struct MergedFollowPage {
    pub(crate) service: String,
    pub(crate) after_seq: Option<u64>,
    pub(crate) raw_next_seq: Option<u64>,
    pub(crate) gap: Option<FollowGap>,
    pub(crate) entries: Vec<logproc::ProcessedEntry>,
    pub(crate) truncated: bool,
}

pub(crate) struct MergedFollowOutput {
    pub(crate) entries: Vec<logproc::ProcessedEntry>,
    pub(crate) next: BTreeMap<String, u64>,
    pub(crate) gaps: Vec<ServiceFollowGap>,
    pub(crate) truncated: bool,
}

pub(crate) fn next_follow_cursor(lines: &[LogLine], after_seq: Option<u64>) -> Option<u64> {
    lines.last().map(|line| line.seq).or(after_seq)
}

pub(crate) fn truncate_wait_matches(
    entries: &mut Vec<logproc::ProcessedEntry>,
    raw_next_seq: u64,
    server_truncated: bool,
    limit: usize,
) -> (u64, bool) {
    if entries.len() <= limit {
        return (raw_next_seq, server_truncated);
    }
    let truncated = logproc::truncate_preserving_record_boundaries(entries, limit);
    let next_seq = if truncated {
        entries.last().map_or(raw_next_seq, |entry| entry.seq)
    } else {
        raw_next_seq
    };
    (next_seq, server_truncated || truncated)
}

pub(crate) fn follow_gap(
    lines: &[LogLine],
    first_retained_seq: Option<u64>,
    after_seq: Option<u64>,
    run_generation: Option<u64>,
) -> Option<FollowGap> {
    if run_generation.is_some() {
        return None;
    }
    let after_seq = after_seq?;
    let first_seq = first_retained_seq.or_else(|| lines.first().map(|line| line.seq))?;
    if first_seq > after_seq.saturating_add(1) {
        Some(FollowGap {
            after_seq,
            first_seq,
            lost_entries_at_least: first_seq.saturating_sub(after_seq).saturating_sub(1),
        })
    } else {
        None
    }
}

pub(crate) fn follow_all_after_seq(
    cursors: &BTreeMap<String, u64>,
    service_id: &str,
) -> Option<u64> {
    if cursors.is_empty() {
        None
    } else {
        Some(cursors.get(service_id).copied().unwrap_or(0))
    }
}

pub(crate) fn merge_follow_pages(
    mut pages: Vec<MergedFollowPage>,
    limit: usize,
    tail_mode: bool,
) -> MergedFollowOutput {
    let page_truncated = pages.iter().any(|page| page.truncated);
    let matched_by_service = pages
        .iter()
        .map(|page| (page.service.clone(), !page.entries.is_empty()))
        .collect::<BTreeMap<_, _>>();
    let entries = pages
        .iter_mut()
        .flat_map(|page| std::mem::take(&mut page.entries))
        .collect::<Vec<_>>();
    let mut entries = logproc::merge_preserving_service_order(entries);
    let mut merge_truncated = false;
    if entries.len() > limit {
        merge_truncated = if tail_mode {
            logproc::tail_preserving_record_boundaries(&mut entries, limit)
        } else {
            logproc::truncate_preserving_record_boundaries(&mut entries, limit)
        };
    }

    let returned = returned_cursors(&entries);
    let mut next = BTreeMap::new();
    for page in &pages {
        let cursor = if merge_truncated && !tail_mode {
            returned
                .get(&page.service)
                .copied()
                .or_else(|| {
                    matched_by_service
                        .get(&page.service)
                        .is_some_and(|matched| !matched)
                        .then_some(page.raw_next_seq)
                        .flatten()
                })
                .or(page.after_seq)
        } else {
            page.raw_next_seq
                .or(page.after_seq)
                .or(tail_mode.then_some(0))
        };
        if let Some(cursor) = cursor {
            next.insert(page.service.clone(), cursor);
        }
    }
    let gaps = pages
        .into_iter()
        .filter_map(|page| {
            page.gap.map(|gap| ServiceFollowGap {
                service: page.service,
                after_seq: gap.after_seq,
                first_seq: gap.first_seq,
                lost_entries_at_least: gap.lost_entries_at_least,
            })
        })
        .collect();

    MergedFollowOutput {
        entries,
        next,
        gaps,
        truncated: page_truncated || merge_truncated,
    }
}

fn returned_cursors(entries: &[logproc::ProcessedEntry]) -> BTreeMap<String, u64> {
    let mut cursors: BTreeMap<String, u64> = BTreeMap::new();
    for entry in entries {
        if let Some(service) = &entry.service {
            cursors
                .entry(service.clone())
                .and_modify(|seq| *seq = (*seq).max(entry.seq))
                .or_insert(entry.seq);
        }
    }
    cursors
}

pub(crate) async fn fetch_and_shape_tail(
    conn: &mut SessionConn,
    service_id: &str,
    run_generation: Option<u64>,
    fetch_tail: usize,
    filters: &LogFilters,
    raw: bool,
) -> Result<(Vec<logproc::ProcessedEntry>, bool, bool), ErrorData> {
    let response = conn
        .request(Request::GetLogs {
            service: service_id.to_string(),
            run_generation,
            tail: Some(fetch_tail),
        })
        .await
        .map_err(error_data)?;
    let logs = service_result(service_id, convert::logs(response)).await?;
    let window_full = logs.lines.len() >= fetch_tail;
    let server_truncated = logs.truncated;
    let entries = logproc::shape(&logs.lines, &filters.shape(raw, None));
    Ok((entries, window_full, server_truncated))
}

pub(crate) async fn current_service_cursor(
    conn: &mut SessionConn,
    service: &str,
) -> Result<u64, ToolError> {
    let response = conn
        .request(Request::ListLogRuns {
            service: service.to_string(),
        })
        .await?;
    let cursor = convert::log_runs(response)?
        .into_iter()
        .rev()
        .find(|run| run.current)
        .and_then(|run| run.last_seq);
    if let Some(cursor) = cursor {
        return Ok(cursor);
    }

    let response = conn
        .request(Request::GetLogs {
            service: service.to_string(),
            run_generation: None,
            tail: Some(1),
        })
        .await?;
    Ok(convert::logs(response)?
        .lines
        .last()
        .map_or(0, |line| line.seq))
}

pub(crate) async fn current_cursors(
    conn: &mut SessionConn,
) -> Result<BTreeMap<String, u64>, ToolError> {
    let response = conn.request(Request::ListServices).await?;
    let services = convert::services(response)?;
    let mut cursors = BTreeMap::new();
    for service in services {
        cursors.insert(
            service.id.clone(),
            current_service_cursor(conn, &service.id).await?,
        );
    }
    Ok(cursors)
}

pub(crate) async fn compact_logs_since(
    conn: &mut SessionConn,
    service: &str,
    after: u64,
    limit: usize,
) -> Result<(Vec<logproc::ProcessedEntry>, bool), ToolError> {
    let response = conn
        .request(Request::FollowLogs {
            service: service.to_string(),
            run_generation: None,
            after: Some(after),
        })
        .await?;
    let logs = convert::logs(response)?;
    let mut entries = logproc::shape(
        &logs.lines,
        &logproc::Shape {
            format: logproc::LogFormat::Compact,
            ..logproc::Shape::default()
        },
    );
    let mut truncated = logs.truncated;
    if entries.len() > limit {
        truncated |= logproc::truncate_preserving_record_boundaries(&mut entries, limit);
    }
    Ok((entries, truncated))
}

pub(crate) async fn follow_all_current_logs(
    conn: &mut SessionConn,
    after: &BTreeMap<String, u64>,
    raw: bool,
    filters: &LogFilters,
    limit: usize,
) -> Result<MergedFollowOutput, ToolError> {
    let tail_mode = after.is_empty();
    let response = conn.request(Request::ListServices).await?;
    let services = convert::services(response)?;
    let mut pages = Vec::new();
    for service in services {
        let after_seq = follow_all_after_seq(after, &service.id);
        let response = conn
            .request(Request::FollowLogs {
                service: service.id.clone(),
                run_generation: None,
                after: after_seq,
            })
            .await?;
        let logs = convert::logs(response)?;
        let raw_next_seq = next_follow_cursor(&logs.lines, after_seq);
        let gap = follow_gap(&logs.lines, logs.first_retained_seq, after_seq, None);
        let mut entries = logproc::shape(&logs.lines, &filters.shape(raw, None));
        for entry in &mut entries {
            entry.service = Some(service.id.clone());
        }
        pages.push(MergedFollowPage {
            service: service.id,
            after_seq,
            raw_next_seq,
            gap,
            entries,
            truncated: logs.truncated,
        });
    }
    Ok(merge_follow_pages(pages, limit, tail_mode))
}
