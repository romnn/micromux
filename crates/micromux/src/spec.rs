//! Normalized service definitions shared by configuration, control, and scheduling.

use std::path::PathBuf;
use std::time::Duration;

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::DependencyCondition;
use crate::scheduler::ServiceID;
use crate::service::RestartPolicy;

/// Default probe interval when none is configured (matches Docker Compose).
const DEFAULT_HEALTHCHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Default probe timeout when none is configured (matches Docker Compose).
const DEFAULT_HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Default graceful-stop window when none is configured (matches Docker Compose).
pub(crate) const DEFAULT_STOP_GRACE_PERIOD: Duration = Duration::from_secs(10);
/// Longest graceful-stop window accepted for one service.
pub(crate) const MAX_STOP_GRACE_PERIOD: Duration = Duration::from_mins(5);

/// Signal delivered for the graceful stop request before force-kill escalation.
///
/// Defaults to [`StopSignal::Term`]. Services designed around interactive Ctrl-C
/// shutdown can choose `SIGINT`, and nodemon-style supervisors `SIGUSR2`. On Windows
/// the setting is accepted but ignored: termination always goes through the pty
/// backend.
///
/// The signal reaches the service's whole process group, and on Unix a best-effort
/// sweep also delivers it to descendants that escaped into a different group. The
/// sweep observes the process table, so a process forked in the races around its scans
/// can be missed — hard containment would need OS facilities such as cgroups — but
/// descendants it does reach receive the signal chosen for the service. That is worth
/// weighing for the non-`SIGTERM` values, which carry unrelated conventional meanings
/// in other programs (`SIGUSR1` and `SIGUSR2` are commonly reload or upgrade
/// triggers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum StopSignal {
    /// `SIGTERM`, the conventional graceful-termination request.
    #[default]
    #[serde(rename = "SIGTERM", alias = "TERM")]
    Term,
    /// `SIGINT`, equivalent to Ctrl-C for tools that only handle interactive interrupt.
    #[serde(rename = "SIGINT", alias = "INT")]
    Int,
    /// `SIGHUP`, for daemons that treat terminal hangup as shutdown.
    #[serde(rename = "SIGHUP", alias = "HUP")]
    Hup,
    /// `SIGQUIT`, a quit request that conventionally also dumps core.
    #[serde(rename = "SIGQUIT", alias = "QUIT")]
    Quit,
    /// `SIGUSR1`, for tools with a user-defined shutdown convention.
    #[serde(rename = "SIGUSR1", alias = "USR1")]
    Usr1,
    /// `SIGUSR2`, used by nodemon-style supervisors for graceful shutdown.
    #[serde(rename = "SIGUSR2", alias = "USR2")]
    Usr2,
}

impl std::fmt::Display for StopSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Term => "SIGTERM",
            Self::Int => "SIGINT",
            Self::Hup => "SIGHUP",
            Self::Quit => "SIGQUIT",
            Self::Usr1 => "SIGUSR1",
            Self::Usr2 => "SIGUSR2",
        };
        f.write_str(name)
    }
}

/// Whether a service id is safe for control selectors and filesystem-backed log names.
#[must_use]
pub fn service_id_is_valid(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The normalized, origin-independent definition of one supervised service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSpec {
    /// Optional display name. The service id is used when this is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Resolved command argv.
    #[serde(default)]
    pub command: Vec<String>,
    /// Resolved absolute working directory, or the session directory when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Literal environment entries layered over the supervisor process environment.
    #[serde(default)]
    pub environment: IndexMap<String, String>,
    /// Services that must reach their requested conditions before this service starts.
    #[serde(default)]
    pub depends_on: Vec<DependencySpec>,
    /// Fully resolved healthcheck definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthcheckSpec>,
    /// Informational ports advertised by this service.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Automatic restart behavior.
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Time allowed for graceful termination before the process is force-killed.
    ///
    /// Values must be greater than zero and no longer than five minutes.
    #[serde(with = "duration", default = "default_stop_grace_period")]
    #[schemars(with = "String")]
    pub stop_grace_period: Duration,
    /// Signal delivered for the graceful stop request.
    #[serde(default)]
    pub stop_signal: StopSignal,
}

impl Default for ServiceSpec {
    fn default() -> Self {
        Self {
            name: None,
            command: Vec::new(),
            working_dir: None,
            environment: IndexMap::new(),
            depends_on: Vec::new(),
            healthcheck: None,
            ports: Vec::new(),
            restart: RestartPolicy::default(),
            stop_grace_period: DEFAULT_STOP_GRACE_PERIOD,
            stop_signal: StopSignal::default(),
        }
    }
}

impl ServiceSpec {
    /// Normalize commands for the platform and validate timing invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when either command is empty, a recurring duration is zero, or the graceful
    /// stop window is outside the supported range.
    pub fn normalize(&mut self) -> Result<(), SpecError> {
        self.command = normalize_command(&self.command)?;
        if self.stop_grace_period.is_zero() {
            return Err(SpecError::ZeroStopGracePeriod);
        }
        if self.stop_grace_period > MAX_STOP_GRACE_PERIOD {
            return Err(SpecError::StopGracePeriodTooLong);
        }
        if let Some(healthcheck) = &mut self.healthcheck {
            healthcheck.test = normalize_command(&healthcheck.test)?;
            healthcheck.retries = healthcheck.retries.max(1);
            if healthcheck.interval.is_zero() {
                return Err(SpecError::ZeroHealthcheckInterval);
            }
            if healthcheck.timeout.is_zero() {
                return Err(SpecError::ZeroHealthcheckTimeout);
            }
        }
        Ok(())
    }

    /// The overridden working directory as a display string.
    #[must_use]
    pub fn working_dir_display(&self) -> Option<String> {
        self.working_dir
            .as_ref()
            .map(|dir| dir.display().to_string())
    }
}

/// A normalized dependency edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DependencySpec {
    /// Target service id.
    pub service: ServiceID,
    /// State the target must reach.
    #[serde(default)]
    pub condition: DependencyCondition,
}

/// A fully resolved healthcheck definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HealthcheckSpec {
    /// Probe command argv.
    #[serde(default)]
    pub test: Vec<String>,
    /// Delay before the first probe.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration::option"
    )]
    #[schemars(with = "Option<String>")]
    pub start_delay: Option<Duration>,
    /// Delay between probe attempts.
    #[serde(with = "duration", default = "default_healthcheck_interval")]
    #[schemars(with = "String")]
    pub interval: Duration,
    /// Maximum duration of one probe attempt.
    #[serde(with = "duration", default = "default_healthcheck_timeout")]
    #[schemars(with = "String")]
    pub timeout: Duration,
    /// Consecutive failures required before the service becomes unhealthy.
    ///
    /// Probe attempts never overlap. A failing transition may therefore take up to
    /// `retries * timeout + (retries - 1) * interval` after the first attempt begins.
    #[serde(default = "default_healthcheck_retries")]
    pub retries: usize,
}

// Serde field defaults so hand-written dynamic-service healthchecks only need `test`;
// `HealthcheckSpec::default()` delegates here so the two cannot drift.
fn default_healthcheck_interval() -> Duration {
    DEFAULT_HEALTHCHECK_INTERVAL
}

fn default_healthcheck_timeout() -> Duration {
    DEFAULT_HEALTHCHECK_TIMEOUT
}

fn default_healthcheck_retries() -> usize {
    1
}

pub(crate) fn default_stop_grace_period() -> Duration {
    DEFAULT_STOP_GRACE_PERIOD
}

impl Default for HealthcheckSpec {
    fn default() -> Self {
        Self {
            test: Vec::new(),
            start_delay: None,
            interval: default_healthcheck_interval(),
            timeout: default_healthcheck_timeout(),
            retries: default_healthcheck_retries(),
        }
    }
}

impl From<crate::config::HealthCheck> for HealthcheckSpec {
    fn from(healthcheck: crate::config::HealthCheck) -> Self {
        let (program, args) = healthcheck.test;
        let mut test = vec![program.into_inner()];
        test.extend(args.into_iter().map(yaml_spanned::Spanned::into_inner));
        Self {
            test,
            start_delay: healthcheck
                .start_delay
                .map(yaml_spanned::Spanned::into_inner),
            interval: healthcheck
                .interval
                .map(yaml_spanned::Spanned::into_inner)
                .unwrap_or(DEFAULT_HEALTHCHECK_INTERVAL),
            timeout: healthcheck
                .timeout
                .map(yaml_spanned::Spanned::into_inner)
                .unwrap_or(DEFAULT_HEALTHCHECK_TIMEOUT),
            retries: healthcheck
                .retries
                .map(yaml_spanned::Spanned::into_inner)
                .unwrap_or(1)
                .max(1),
        }
    }
}

/// Where a supervised service came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ServiceOrigin {
    /// Loaded from the session configuration.
    Configured,
    /// Created through the control plane.
    Dynamic(DynamicOrigin),
}

/// Provenance and lease metadata for a runtime-created service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DynamicOrigin {
    /// Creation time in Unix milliseconds.
    pub created_at_unix_ms: u64,
    /// Lease expiry time in Unix milliseconds, or `None` for a session-lifetime lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    /// Optional caller-supplied ownership label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Optimistic-concurrency revision, starting at one.
    pub revision: u64,
}

/// A dynamic-service lease, either bounded by a duration or valid for the session lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema)]
#[schemars(with = "String")]
pub enum Lease {
    /// A bounded lease duration.
    After(Duration),
    /// A lease with no expiry before the session stops.
    Unbounded,
}

impl Serialize for Lease {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::After(duration) => {
                serializer.serialize_str(&humantime::format_duration(*duration).to_string())
            }
            Self::Unbounded => serializer.serialize_str("none"),
        }
    }
}

impl<'de> Deserialize<'de> for Lease {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        if raw == "none" {
            Ok(Self::Unbounded)
        } else {
            humantime::parse_duration(&raw)
                .map(Self::After)
                .map_err(|err| {
                    serde::de::Error::custom(format!(
                        "expected a duration like \"30m\" or the literal \"none\": {err}"
                    ))
                })
        }
    }
}

/// An optional mutation field that can also explicitly clear its current value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SpecField<T> {
    /// The caller did not provide this field.
    #[default]
    Unspecified,
    /// The caller provided `null` to clear this field.
    Clear,
    /// The caller provided a replacement value.
    Value(T),
}

impl<T> SpecField<T> {
    fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified)
    }
}

impl<T: Serialize> Serialize for SpecField<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Unspecified | Self::Clear => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for SpecField<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Option::<T>::deserialize(deserializer)?.map_or(Self::Clear, Self::Value))
    }
}

/// Optional service fields accepted by dynamic-service mutations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PartialServiceSpec {
    /// Display-name override; `null` clears a cloned name.
    #[serde(default, skip_serializing_if = "SpecField::is_unspecified")]
    #[schemars(with = "Option<String>")]
    pub name: SpecField<String>,
    /// Command argv override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Working-directory override; `null` restores the session directory.
    #[serde(default, skip_serializing_if = "SpecField::is_unspecified")]
    #[schemars(with = "Option<PathBuf>")]
    pub working_dir: SpecField<PathBuf>,
    /// Environment entries merged over a cloned service's environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<IndexMap<String, String>>,
    /// Dependency list replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<DependencySpec>>,
    /// Healthcheck replacement; `null` removes a cloned healthcheck.
    #[serde(default, skip_serializing_if = "SpecField::is_unspecified")]
    #[schemars(with = "Option<HealthcheckSpec>")]
    pub healthcheck: SpecField<HealthcheckSpec>,
    /// Advertised-port replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<u16>>,
    /// Restart-policy replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<RestartPolicy>,
    /// Graceful-stop window replacement.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration::option"
    )]
    #[schemars(with = "Option<String>")]
    pub stop_grace_period: Option<Duration>,
    /// Graceful-stop signal replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<StopSignal>,
}

impl PartialServiceSpec {
    /// Overlay the provided fields on a base definition.
    #[must_use]
    pub fn apply_to(self, mut base: ServiceSpec) -> ServiceSpec {
        match self.name {
            SpecField::Unspecified => {}
            SpecField::Clear => base.name = None,
            SpecField::Value(name) => base.name = Some(name),
        }
        if let Some(command) = self.command {
            base.command = command;
        }
        match self.working_dir {
            SpecField::Unspecified => {}
            SpecField::Clear => base.working_dir = None,
            SpecField::Value(working_dir) => base.working_dir = Some(working_dir),
        }
        if let Some(environment) = self.environment {
            base.environment.extend(environment);
        }
        if let Some(depends_on) = self.depends_on {
            base.depends_on = depends_on;
        }
        match self.healthcheck {
            SpecField::Unspecified => {}
            SpecField::Clear => base.healthcheck = None,
            SpecField::Value(healthcheck) => base.healthcheck = Some(healthcheck),
        }
        if let Some(ports) = self.ports {
            base.ports = ports;
        }
        if let Some(restart) = self.restart {
            base.restart = restart;
        }
        if let Some(stop_grace_period) = self.stop_grace_period {
            base.stop_grace_period = stop_grace_period;
        }
        if let Some(stop_signal) = self.stop_signal {
            base.stop_signal = stop_signal;
        }
        base
    }
}

/// Parameters shared by dynamic-service control requests and MCP tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DynamicServiceParams {
    /// Stable id for the dynamic service.
    pub service: ServiceID,
    /// Optional service-definition fields.
    #[serde(flatten)]
    pub spec: PartialServiceSpec,
    /// Existing service whose current definition should be cloned server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_service: Option<ServiceID>,
    /// Arguments appended after materializing and normalizing the command.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Requested lease. The session policy may clamp an unbounded or long lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_after: Option<Lease>,
    /// Optional caller-supplied ownership label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Retry key for safely replaying a mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// A service definition cannot be normalized into executable argv.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    /// The command has no executable component.
    #[error("command is empty")]
    EmptyCommand,
    /// A zero interval would run probes continuously.
    #[error("healthcheck interval must be greater than zero")]
    ZeroHealthcheckInterval,
    /// A zero timeout would immediately kill every probe.
    #[error("healthcheck timeout must be greater than zero")]
    ZeroHealthcheckTimeout,
    /// A zero stop grace would skip graceful termination.
    #[error("stop grace period must be greater than zero")]
    ZeroStopGracePeriod,
    /// An excessive stop grace would make supervisor shutdown unreasonably long.
    #[error("stop grace period must not exceed 5m")]
    StopGracePeriodTooLong,
}

/// Lower a plain argv or Compose-style `CMD`/`CMD-SHELL` form for the current platform.
pub(crate) fn normalize_command(command: &[String]) -> Result<Vec<String>, SpecError> {
    let Some(first) = command.first() else {
        return Err(SpecError::EmptyCommand);
    };
    match first.as_str() {
        "CMD" => command
            .get(1..)
            .filter(|rest| !rest.is_empty())
            .map_or(Err(SpecError::EmptyCommand), |rest| Ok(rest.to_vec())),
        "CMD-SHELL" => {
            let Some(rest) = command.get(1..).filter(|rest| !rest.is_empty()) else {
                return Err(SpecError::EmptyCommand);
            };
            let command = rest.join(" ");
            #[cfg(unix)]
            let argv = vec!["sh".to_string(), "-c".to_string(), command];
            #[cfg(windows)]
            let argv = vec![
                "cmd.exe".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                command,
            ];
            Ok(argv)
        }
        _ => Ok(command.to_vec()),
    }
}

mod duration {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&humantime::format_duration(*value).to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        humantime::parse_duration(&raw).map_err(serde::de::Error::custom)
    }

    pub mod option {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        #[expect(
            clippy::ref_option,
            reason = "Serde with-module serializers must accept a shared reference to the field type"
        )]
        pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            value
                .map(|duration| humantime::format_duration(duration).to_string())
                .serialize(serializer)
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: Deserializer<'de>,
        {
            Option::<String>::deserialize(deserializer)?
                .map(|raw| humantime::parse_duration(&raw).map_err(serde::de::Error::custom))
                .transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    #[test]
    fn service_spec_round_trips_humantime_without_normalizing_commands() -> eyre::Result<()> {
        let spec = ServiceSpec {
            command: vec!["CMD-SHELL".to_string(), "echo hi".to_string()],
            healthcheck: Some(HealthcheckSpec {
                test: vec!["CMD".to_string(), "true".to_string()],
                start_delay: Some(Duration::from_millis(250)),
                interval: Duration::from_secs(2),
                timeout: Duration::from_secs(1),
                retries: 3,
            }),
            ..ServiceSpec::default()
        };

        let json = serde_json::to_value(&spec)?;
        assert_eq!(json["healthcheck"]["start_delay"], "250ms");
        assert_eq!(json["healthcheck"]["interval"], "2s");
        assert_eq!(serde_json::from_value::<ServiceSpec>(json)?, spec);
        Ok(())
    }

    #[test]
    fn command_normalization_is_explicit() -> eyre::Result<()> {
        let mut spec = ServiceSpec {
            command: vec![
                "CMD-SHELL".to_string(),
                "echo".to_string(),
                "hello".to_string(),
            ],
            ..ServiceSpec::default()
        };
        spec.normalize()?;
        #[cfg(unix)]
        assert_eq!(spec.command, vec!["sh", "-c", "echo hello"]);
        #[cfg(windows)]
        assert_eq!(spec.command, vec!["cmd.exe", "/S", "/C", "echo hello"]);
        Ok(())
    }

    #[test]
    fn normalization_floors_healthcheck_retries() -> eyre::Result<()> {
        let mut spec = ServiceSpec {
            command: vec!["true".to_string()],
            healthcheck: Some(HealthcheckSpec {
                test: vec!["true".to_string()],
                retries: 0,
                ..HealthcheckSpec::default()
            }),
            ..ServiceSpec::default()
        };

        spec.normalize()?;

        assert_eq!(
            spec.healthcheck.map(|healthcheck| healthcheck.retries),
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn normalization_rejects_zero_action_durations() {
        let healthcheck = HealthcheckSpec {
            test: vec!["true".to_string()],
            ..HealthcheckSpec::default()
        };
        let cases = [
            (
                ServiceSpec {
                    command: vec!["true".to_string()],
                    healthcheck: Some(HealthcheckSpec {
                        interval: Duration::ZERO,
                        ..healthcheck.clone()
                    }),
                    ..ServiceSpec::default()
                },
                SpecError::ZeroHealthcheckInterval,
            ),
            (
                ServiceSpec {
                    command: vec!["true".to_string()],
                    healthcheck: Some(HealthcheckSpec {
                        timeout: Duration::ZERO,
                        ..healthcheck
                    }),
                    ..ServiceSpec::default()
                },
                SpecError::ZeroHealthcheckTimeout,
            ),
            (
                ServiceSpec {
                    command: vec!["true".to_string()],
                    stop_grace_period: Duration::ZERO,
                    ..ServiceSpec::default()
                },
                SpecError::ZeroStopGracePeriod,
            ),
            (
                ServiceSpec {
                    command: vec!["true".to_string()],
                    stop_grace_period: MAX_STOP_GRACE_PERIOD + Duration::from_secs(1),
                    ..ServiceSpec::default()
                },
                SpecError::StopGracePeriodTooLong,
            ),
        ];

        for (mut spec, expected) in cases {
            assert_eq!(spec.normalize(), Err(expected));
        }
    }

    #[test]
    fn partial_spec_distinguishes_omitted_fields_from_explicit_null() -> eyre::Result<()> {
        let partial = serde_json::from_value::<PartialServiceSpec>(serde_json::json!({
            "name": null,
            "healthcheck": null,
            "environment": {"NEW": "value"}
        }))?;
        let base = ServiceSpec {
            name: Some("base".to_string()),
            environment: IndexMap::from([
                ("KEPT".to_string(), "yes".to_string()),
                ("NEW".to_string(), "old".to_string()),
            ]),
            healthcheck: Some(HealthcheckSpec {
                test: vec!["true".to_string()],
                ..HealthcheckSpec::default()
            }),
            ..ServiceSpec::default()
        };

        let applied = partial.apply_to(base);
        assert_eq!(applied.name, None);
        assert_eq!(applied.healthcheck, None);
        assert_eq!(
            applied.environment.get("KEPT").map(String::as_str),
            Some("yes")
        );
        assert_eq!(
            applied.environment.get("NEW").map(String::as_str),
            Some("value")
        );
        Ok(())
    }

    /// `stop_signal` serializes to the canonical `SIGTERM`-style spelling, accepts the
    /// short alias on input, and overrides through a partial spec.
    #[test]
    fn stop_signal_roundtrips_and_applies_through_partial_spec() -> eyre::Result<()> {
        assert_eq!(
            serde_json::to_value(StopSignal::Int)?,
            serde_json::json!("SIGINT")
        );
        assert_eq!(
            serde_json::from_value::<StopSignal>(serde_json::json!("USR2"))?,
            StopSignal::Usr2
        );

        let partial = serde_json::from_value::<PartialServiceSpec>(serde_json::json!({
            "stop_signal": "SIGINT"
        }))?;
        let applied = partial.apply_to(ServiceSpec::default());
        assert_eq!(applied.stop_signal, StopSignal::Int);

        // An omitted field keeps the base's configured signal.
        let untouched = PartialServiceSpec::default().apply_to(ServiceSpec {
            stop_signal: StopSignal::Hup,
            ..ServiceSpec::default()
        });
        assert_eq!(untouched.stop_signal, StopSignal::Hup);
        Ok(())
    }
}
