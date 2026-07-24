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

/// The normalized, origin-independent definition of one supervised service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
}

impl ServiceSpec {
    /// Normalize commands for the platform and floor healthcheck retries to one attempt.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::EmptyCommand`] when either the service command or its healthcheck
    /// contains no executable command.
    pub fn normalize(&mut self) -> Result<(), SpecError> {
        self.command = normalize_command(&self.command)?;
        if let Some(healthcheck) = &mut self.healthcheck {
            healthcheck.test = normalize_command(&healthcheck.test)?;
            healthcheck.retries = healthcheck.retries.max(1);
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
}
