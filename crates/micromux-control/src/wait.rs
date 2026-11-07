//! Waiting for a service to become healthy or exit, shared by `micromux ctl wait` and the MCP
//! waits so both resolve, fail fast, and honor generations the same way.

use std::time::Duration;

use micromux::{ChangeKind, DependencyFailure, DependencyFailureReason, Desired, Execution};
use micromux::{Health, ServiceKind, ServiceSnapshot, SessionChange};

use crate::{Client, ControlEndpoint, Subscription};

/// What a wait waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitFor {
    /// A run that is up, and healthy if the service has a healthcheck.
    Healthy,
    /// A run that exited.
    Exit,
}

/// How a wait resolved against a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitVerdict {
    /// The target run is up, and healthy if a healthcheck is configured.
    Healthy,
    /// The target run of a job completed with exit status 0, which is a job's readiness.
    Completed,
    /// The target run exited; carries its exit code.
    Exited(Option<i32>),
    /// The service is disabled or retired, with no qualifying run in flight.
    Unavailable,
    /// A job that finished, or a manual job that never ran, and that nothing asked to run again.
    Idle,
    /// The service is blocked behind a dependency that cannot meet its condition.
    DependencyFailed(DependencyFailure),
}

/// Return whether a snapshot belongs to the run selected by a generation gate.
///
/// With `after = Some(G)` only a run with `run_generation > G` qualifies, so a caller that just
/// restarted a service never observes the pre-restart run. With `after = None` any run qualifies,
/// but the service must have started at least once.
#[must_use]
pub fn generation_ready(snapshot: &ServiceSnapshot, after: Option<u64>) -> bool {
    match after {
        Some(generation) => snapshot.run_generation > generation,
        None => snapshot.run_generation >= 1,
    }
}

/// Evaluate a generation-gated wait against a snapshot; `None` means keep waiting.
#[must_use]
pub fn evaluate(
    snapshot: &ServiceSnapshot,
    wait_for: WaitFor,
    after: Option<u64>,
) -> Option<WaitVerdict> {
    let ready = generation_ready(snapshot, after);
    let unavailable = snapshot.desired == Desired::Disabled || snapshot.retired.is_some();
    let job = snapshot.kind == ServiceKind::Job;
    // A finished job, or a manual job that never ran, runs again only when asked to. The session
    // never reports either while a run of it is requested, so neither can be about to run.
    let idle = matches!(snapshot.execution, Execution::Completed | Execution::Failed)
        || (snapshot.manual && snapshot.execution == Execution::Pending);
    match wait_for {
        WaitFor::Healthy => {
            // Neither a disabled service nor one blocked for good will become healthy, whatever
            // generation the caller waits for.
            if unavailable {
                return Some(WaitVerdict::Unavailable);
            }
            if let Some(failure) = &snapshot.dependency_failure {
                return Some(WaitVerdict::DependencyFailed(failure.clone()));
            }
            if !ready {
                return idle.then_some(WaitVerdict::Idle);
            }
            match snapshot.execution {
                Execution::Completed => Some(WaitVerdict::Completed),
                Execution::Failed => Some(WaitVerdict::Exited(snapshot.last_exit_code)),
                // A job that exited is about to retry; a service that exited is down.
                Execution::Exited if !job => Some(WaitVerdict::Exited(snapshot.last_exit_code)),
                // A job is ready once it completed, not while it runs.
                Execution::Running
                    if !job
                        && (!snapshot.healthcheck_configured
                            || snapshot.health == Some(Health::Healthy)) =>
                {
                    Some(WaitVerdict::Healthy)
                }
                Execution::Exited
                | Execution::Running
                | Execution::Pending
                | Execution::Blocked
                | Execution::Starting
                | Execution::Stopping
                | Execution::Unknown => None,
            }
        }
        WaitFor::Exit => {
            if ready && snapshot.execution.has_exited() {
                return Some(WaitVerdict::Exited(snapshot.last_exit_code));
            }
            // A disabled run that is still draining will still exit.
            let run_in_flight = ready
                && matches!(
                    snapshot.execution,
                    Execution::Starting | Execution::Running | Execution::Stopping
                );
            if unavailable && !run_in_flight {
                return Some(WaitVerdict::Unavailable);
            }
            if let Some(failure) = &snapshot.dependency_failure {
                return Some(WaitVerdict::DependencyFailed(failure.clone()));
            }
            idle.then_some(WaitVerdict::Idle)
        }
    }
}

/// Explain a dependency failure for `service` and what would resolve it.
#[must_use]
pub fn dependency_failure_message(service: &str, failure: &DependencyFailure) -> String {
    let remedy = match failure.reason {
        DependencyFailureReason::Exited => format!("fix it and restart `{}`", failure.service),
        DependencyFailureReason::Disabled => format!("enable `{}`", failure.service),
        DependencyFailureReason::NotRun => {
            format!("run it with a restart of `{}`", failure.service)
        }
        DependencyFailureReason::NoHealthcheck => format!(
            "give `{}` a healthcheck and restart it, or depend on it with `condition: started`",
            failure.service
        ),
        DependencyFailureReason::Retired
        | DependencyFailureReason::Missing
        | DependencyFailureReason::Unknown => {
            format!("bring `{}` back into the session", failure.service)
        }
    };
    format!(
        "service `{service}` is blocked for good: {}; {remedy}",
        failure.describe()
    )
}

/// Sleeps between the polls of a wait, waking early when the service changes.
///
/// Session change notifications only shorten a sleep; waking at least every `poll` keeps a wait
/// correct when they are lost or the subscription is unavailable.
pub struct ChangeWaker {
    service: String,
    poll: Duration,
    subscription: Option<Subscription>,
}

impl ChangeWaker {
    /// Subscribe to the session's changes, falling back to plain polling when that fails.
    pub async fn subscribe(endpoint: &ControlEndpoint, service: &str, poll: Duration) -> Self {
        Self {
            service: service.to_string(),
            poll,
            subscription: Client::subscribe(endpoint).await.ok(),
        }
    }

    /// Sleep until the service changes, `poll` elapses, or `deadline` passes, whichever is first.
    pub async fn sleep(&mut self, deadline: tokio::time::Instant) {
        let wait = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(self.poll);
        let Some(stream) = self.subscription.as_mut() else {
            tokio::time::sleep(wait).await;
            return;
        };
        // Wake on a change to this service, or a session-wide invalidation synthesized after
        // broadcast loss, but not on log appends, which leave snapshots alone.
        let service = self.service.as_str();
        let woke = tokio::time::timeout(wait, async {
            loop {
                match stream.recv().await {
                    Ok(Some(change))
                        if (change.service_id == service
                            || change.service_id == SessionChange::SESSION_WIDE)
                            && !matches!(change.kind, ChangeKind::Logs | ChangeKind::Heartbeat) =>
                    {
                        return true;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => return false,
                }
            }
        })
        .await;
        if matches!(woke, Ok(false)) {
            // The stream ended; keep polling without it.
            self.subscription = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use micromux::{DependencyCondition, RestartPolicy};
    use similar_asserts::assert_eq;

    fn snapshot() -> ServiceSnapshot {
        let mut snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.execution = Execution::Running;
        snapshot.run_generation = 1;
        snapshot
    }

    fn seed_failure() -> DependencyFailure {
        DependencyFailure {
            service: "seed".to_string(),
            condition: DependencyCondition::CompletedSuccessfully,
            reason: DependencyFailureReason::Exited,
            exit_code: Some(1),
            via: vec!["worker".to_string()],
        }
    }

    #[test]
    fn waits_for_a_new_generation_after_restart() {
        let mut snap = snapshot();
        // Restart returned G=1; the pre-restart run must not satisfy the wait.
        assert_eq!(evaluate(&snap, WaitFor::Healthy, Some(1)), None);
        snap.run_generation = 2;
        assert_eq!(
            evaluate(&snap, WaitFor::Healthy, Some(1)),
            Some(WaitVerdict::Healthy)
        );
    }

    #[test]
    fn requires_healthy_when_a_healthcheck_is_configured() {
        let mut snap = snapshot();
        snap.healthcheck_configured = true;
        assert_eq!(evaluate(&snap, WaitFor::Healthy, None), None);
        snap.health = Some(Health::Unhealthy);
        assert_eq!(evaluate(&snap, WaitFor::Healthy, None), None);
        snap.health = Some(Health::Healthy);
        assert_eq!(
            evaluate(&snap, WaitFor::Healthy, None),
            Some(WaitVerdict::Healthy)
        );
    }

    #[test]
    fn disabled_fails_a_health_wait_whatever_the_generation() {
        let mut snap = snapshot();
        snap.desired = Desired::Disabled;
        assert_eq!(
            evaluate(&snap, WaitFor::Healthy, None),
            Some(WaitVerdict::Unavailable)
        );
        assert_eq!(
            evaluate(&snap, WaitFor::Healthy, Some(5)),
            Some(WaitVerdict::Unavailable)
        );
    }

    #[test]
    fn exits_report_their_exit_code() {
        let mut snap = snapshot();
        snap.run_generation = 2;
        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(3);
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(
                evaluate(&snap, wait_for, Some(1)),
                Some(WaitVerdict::Exited(Some(3)))
            );
        }
    }

    #[test]
    fn a_service_that_never_started_is_pending() {
        let mut snap = snapshot();
        snap.run_generation = 0;
        snap.execution = Execution::Pending;
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(evaluate(&snap, wait_for, None), None);
        }
    }

    #[test]
    fn starting_and_stopping_are_pending() {
        let mut snap = snapshot();
        for execution in [Execution::Starting, Execution::Stopping] {
            snap.execution = execution;
            assert_eq!(evaluate(&snap, WaitFor::Healthy, None), None);
        }
    }

    #[test]
    fn a_blocked_service_waits_until_its_dependency_fails() {
        let mut snap = snapshot();
        snap.run_generation = 0;
        snap.execution = Execution::Blocked;
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(evaluate(&snap, wait_for, None), None);
        }

        snap.dependency_failure = Some(seed_failure());
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(
                evaluate(&snap, wait_for, Some(4)),
                Some(WaitVerdict::DependencyFailed(seed_failure()))
            );
        }
    }

    #[test]
    fn an_exit_wait_lets_a_disabled_run_finish_draining() {
        let mut snap = snapshot();
        snap.desired = Desired::Disabled;
        snap.execution = Execution::Stopping;
        assert_eq!(evaluate(&snap, WaitFor::Exit, None), None);

        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(3);
        assert_eq!(
            evaluate(&snap, WaitFor::Exit, None),
            Some(WaitVerdict::Exited(Some(3)))
        );
        assert_eq!(
            evaluate(&snap, WaitFor::Exit, Some(1)),
            Some(WaitVerdict::Unavailable)
        );
    }

    fn job(execution: Execution, exit_code: Option<i32>) -> ServiceSnapshot {
        let mut snap = snapshot();
        snap.kind = ServiceKind::Job;
        snap.execution = execution;
        snap.last_exit_code = exit_code;
        snap
    }

    #[test]
    fn a_job_is_ready_once_it_completed_and_not_while_it_runs_or_retries() {
        assert_eq!(
            evaluate(&job(Execution::Running, None), WaitFor::Healthy, None),
            None
        );
        assert_eq!(
            evaluate(&job(Execution::Exited, Some(1)), WaitFor::Healthy, None),
            None
        );
        assert_eq!(
            evaluate(&job(Execution::Completed, Some(0)), WaitFor::Healthy, None),
            Some(WaitVerdict::Completed)
        );
        assert_eq!(
            evaluate(&job(Execution::Failed, Some(2)), WaitFor::Healthy, None),
            Some(WaitVerdict::Exited(Some(2)))
        );
        // An exit wait resolves on every exit, a retried one included.
        assert_eq!(
            evaluate(&job(Execution::Exited, Some(1)), WaitFor::Exit, None),
            Some(WaitVerdict::Exited(Some(1)))
        );
        assert_eq!(
            evaluate(&job(Execution::Completed, Some(0)), WaitFor::Exit, None),
            Some(WaitVerdict::Exited(Some(0)))
        );
    }

    #[test]
    fn a_wait_for_a_run_that_nothing_will_start_fails_fast() {
        // A finished job waited on for a newer run.
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(
                evaluate(&job(Execution::Completed, Some(0)), wait_for, Some(1)),
                Some(WaitVerdict::Idle)
            );
        }
        // A manual job that never ran.
        let mut manual = job(Execution::Pending, None);
        manual.manual = true;
        manual.run_generation = 0;
        for wait_for in [WaitFor::Healthy, WaitFor::Exit] {
            assert_eq!(evaluate(&manual, wait_for, None), Some(WaitVerdict::Idle));
        }
        // Once it was asked to run, it waits.
        manual.execution = Execution::Blocked;
        assert_eq!(evaluate(&manual, WaitFor::Healthy, None), None);
    }

    #[test]
    fn the_failure_message_names_the_dependency_its_state_and_the_remedy() {
        assert_eq!(
            dependency_failure_message("api", &seed_failure()),
            "service `api` is blocked for good: dependency `seed` (through `worker`) exited with \
             code 1 and will not restart, so it cannot complete successfully; fix it and restart \
             `seed`"
        );
    }
}
