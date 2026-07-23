use std::collections::{BTreeMap, VecDeque};

use micromux::ServiceEvent;
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SessionServiceEvent {
    pub(crate) service: String,
    #[serde(flatten)]
    pub(crate) event: ServiceEvent,
}

pub(crate) struct ServiceEventPage {
    pub(crate) service: String,
    pub(crate) after_seq: Option<u64>,
    pub(crate) events: Vec<ServiceEvent>,
    pub(crate) truncated: bool,
}

pub(crate) struct MergedServiceEvents {
    pub(crate) events: Vec<SessionServiceEvent>,
    pub(crate) next: BTreeMap<String, u64>,
    pub(crate) truncated: bool,
}

#[must_use]
pub(crate) fn merge_service_events(
    pages: Vec<ServiceEventPage>,
    limit: usize,
    tail_mode: bool,
) -> MergedServiceEvents {
    let page_truncated = pages.iter().any(|page| page.truncated);
    let page_cursors = pages
        .iter()
        .map(|page| {
            (
                page.service.clone(),
                page.after_seq,
                page.events.last().map(|event| event.seq),
                !page.events.is_empty(),
            )
        })
        .collect::<Vec<_>>();
    let mut groups = pages
        .into_iter()
        .map(|page| (page.service, VecDeque::from(page.events)))
        .collect::<BTreeMap<_, _>>();
    let total = groups.values().map(VecDeque::len).sum();
    let mut events = Vec::with_capacity(total);

    while !groups.is_empty() {
        // Selecting only among queue heads preserves each service's sequence even when its clock
        // timestamps arrive out of order.
        let selected = groups
            .iter()
            .filter_map(|(service, events)| {
                events.front().map(|event| {
                    (
                        service.clone(),
                        (event.at_unix_ms, service.as_str(), event.seq),
                    )
                })
            })
            .min_by(|(_, left), (_, right)| left.cmp(right))
            .map(|(service, _)| service);
        let Some(service) = selected else {
            break;
        };
        let Some(group) = groups.get_mut(&service) else {
            break;
        };
        if let Some(event) = group.pop_front() {
            events.push(SessionServiceEvent {
                service: service.clone(),
                event,
            });
        }
        if group.is_empty() {
            groups.remove(&service);
        }
    }

    let merge_truncated = events.len() > limit;
    if merge_truncated {
        if tail_mode {
            events = events.split_off(events.len() - limit);
        } else {
            events.truncate(limit);
        }
    }

    let returned = returned_cursors(&events);
    let mut next = BTreeMap::new();
    for (service, after_seq, raw_next_seq, had_events) in page_cursors {
        let cursor = if merge_truncated && !tail_mode {
            returned
                .get(&service)
                .copied()
                .or_else(|| (!had_events).then_some(raw_next_seq).flatten())
                .or(after_seq)
        } else {
            raw_next_seq.or(after_seq).or(tail_mode.then_some(0))
        };
        if let Some(cursor) = cursor {
            next.insert(service, cursor);
        }
    }

    MergedServiceEvents {
        events,
        next,
        truncated: page_truncated || merge_truncated,
    }
}

fn returned_cursors(events: &[SessionServiceEvent]) -> BTreeMap<String, u64> {
    let mut cursors = BTreeMap::new();
    for event in events {
        cursors
            .entry(event.service.clone())
            .and_modify(|seq: &mut u64| *seq = (*seq).max(event.event.seq))
            .or_insert(event.event.seq);
    }
    cursors
}

#[cfg(test)]
mod tests {
    use super::{ServiceEventPage, merge_service_events};
    use micromux::{ServiceEvent, ServiceEventKind};
    use similar_asserts::assert_eq;

    fn event(seq: u64, at_unix_ms: u64) -> ServiceEvent {
        ServiceEvent {
            seq,
            at_unix_ms,
            run_generation: 1,
            kind: ServiceEventKind::Spawned,
            detail: "spawned".to_string(),
            exit_code: None,
            pid: None,
            delay_ms: None,
            blocked_on: None,
        }
    }

    fn interleaved_pages() -> Vec<ServiceEventPage> {
        vec![
            ServiceEventPage {
                service: "alpha".to_string(),
                after_seq: Some(0),
                events: vec![event(1, 1), event(2, 3)],
                truncated: false,
            },
            ServiceEventPage {
                service: "beta".to_string(),
                after_seq: Some(0),
                events: vec![event(1, 2), event(2, 4)],
                truncated: false,
            },
        ]
    }

    #[test]
    fn merge_orders_service_heads_and_preserves_each_sequence() {
        let merged = merge_service_events(interleaved_pages(), 4, false);
        let order = merged
            .events
            .iter()
            .map(|event| (event.service.as_str(), event.event.seq))
            .collect::<Vec<_>>();

        assert_eq!(
            order,
            vec![("alpha", 1), ("beta", 1), ("alpha", 2), ("beta", 2)]
        );
        assert_eq!(merged.next.get("alpha"), Some(&2));
        assert_eq!(merged.next.get("beta"), Some(&2));
        assert!(!merged.truncated);
    }

    #[test]
    fn shared_bound_advances_only_returned_incremental_events_but_all_tail_cursors() {
        let page = merge_service_events(interleaved_pages(), 2, false);
        let incremental = page
            .events
            .iter()
            .map(|event| (event.service.as_str(), event.event.seq))
            .collect::<Vec<_>>();
        assert_eq!(incremental, vec![("alpha", 1), ("beta", 1)]);
        assert_eq!(page.next.get("alpha"), Some(&1));
        assert_eq!(page.next.get("beta"), Some(&1));
        assert!(page.truncated);

        let tail = merge_service_events(interleaved_pages(), 2, true);
        let latest = tail
            .events
            .iter()
            .map(|event| (event.service.as_str(), event.event.seq))
            .collect::<Vec<_>>();
        assert_eq!(latest, vec![("alpha", 2), ("beta", 2)]);
        assert_eq!(tail.next.get("alpha"), Some(&2));
        assert_eq!(tail.next.get("beta"), Some(&2));
        assert!(tail.truncated);
    }
}
