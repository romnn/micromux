//! Session identity, session-allocated ports, and the variables injected into every service.
//!
//! Several checkouts of one project can each run a session at the same time.
//! A session has a hostname-safe name, and it can ask for TCP ports that stay the same across its
//! restarts while no two live sessions of the same user ever share one.
//! Both reach services through pinned environment variables that override every `env_file` and
//! inline `environment` definition of the same name.

mod git;
mod registry;
mod routes;
mod status;

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::config::{ConfigFile, SessionConfig};

pub use git::GitContext;
pub(crate) use registry::{
    DEFAULT_PORT_RANGE, Lease, PortRequest, create_private_dir, user_state_directory,
};
pub use registry::{PortRegistry, STATE_DIR_VARIABLE, state_directory_override};
pub use routes::{CATCH_ALL_HOST, Route, RouteError, route_path_problem};
pub(crate) use routes::{owner as route_owner, resolve as resolve_routes};
pub use status::{RoutingStatus, SessionStatus, SessionSummary};

/// Parent domain of every session when the config does not name one.
pub const DEFAULT_DOMAIN: &str = "localhost";

/// Session name used when the config directory has no usable basename, such as `/`.
const FALLBACK_SESSION_NAME: &str = "micromux";

/// Longest hostname label accepted by DNS.
const MAX_LABEL_LEN: usize = 63;

/// Longest hostname accepted by DNS.
const MAX_HOSTNAME_LEN: usize = 253;

/// Environment variable carrying the session name.
pub const SESSION_VARIABLE: &str = "MICROMUX_SESSION";

/// Environment variable carrying `<session>.<domain>`.
pub const SESSION_DOMAIN_VARIABLE: &str = "MICROMUX_SESSION_DOMAIN";

/// Environment variable carrying the port the shared proxy serves the session's routes on.
pub const PROXY_PORT_VARIABLE: &str = "MICROMUX_PROXY_PORT";

/// Environment variable carrying what follows `<route>.` in a browser URL.
pub const HOST_SUFFIX_VARIABLE: &str = "MICROMUX_HOST_SUFFIX";

/// Longest `allocate_ports` name, which keeps a session's description small.
const MAX_ALLOCATION_NAME_LEN: usize = 128;

/// Variables micromux sets on every spawned service itself, which an allocation must not shadow.
const SPAWN_OWNED_VARIABLES: &[&str] = &["TERM", "CLICOLOR", "CLICOLOR_FORCE", "FORCE_COLOR"];

/// Errors from establishing a session's identity and ports.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// A registry file or directory could not be accessed.
    #[error("failed to access the port registry at {}: {source}", path.display())]
    Io {
        /// Path that could not be accessed.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// One process held the port registry lock for longer than a session waits.
    #[error(
        "the port registry lock {} is still held after {}s{}; another micromux is stuck while \
         allocating ports, so stop it and start the session again",
        path.display(),
        waited.as_secs(),
        holder.map(|pid| format!(" by pid {pid}")).unwrap_or_default()
    )]
    RegistryLocked {
        /// Path of the registry lock file.
        path: PathBuf,
        /// How long the session waited without the lock changing hands.
        waited: std::time::Duration,
        /// Pid the holder recorded in the lock file, when it could be read.
        holder: Option<u32>,
    },
    /// No per-user state directory could be determined for the port registry.
    #[error("cannot determine a per-user state directory for the port registry")]
    NoStateDirectory,
    /// Allocations and routes need the config file's path, which identifies the session across
    /// restarts.
    #[error("allocate_ports and routes need a config loaded from a file")]
    NoConfigPath,
    /// Another micromux already runs this config.
    #[error(
        "another micromux is already running {}; stop it before starting a second session for the \
         same config",
        config_path.display()
    )]
    AlreadyRunning {
        /// Canonical path of the config file.
        config_path: PathBuf,
    },
    /// Another live session already uses this session name.
    #[error(
        "session name `{name}` is already used by the live session for {other_config}; set \
         `session.name` to tell the two apart"
    )]
    NameInUse {
        /// The contested session name.
        name: String,
        /// Config path of the other live session.
        other_config: String,
    },
    /// The port persisted for a variable is held by a process outside this session.
    #[error(
        "port {port} assigned to `{variable}` is in use by another process; stop that process \
         (it may be left over from an earlier run) and start the session again"
    )]
    PortBusy {
        /// The allocated variable.
        variable: String,
        /// Its persisted port.
        port: u16,
    },
    /// The port persisted for a variable is reserved for another purpose, such as the proxy.
    #[error(
        "port {port} assigned to `{variable}` is reserved by micromux itself; remove `{variable}` \
         from {} to get a new port",
        record.display()
    )]
    PortReserved {
        /// The allocated variable.
        variable: String,
        /// Its persisted port.
        port: u16,
        /// The registry record holding the assignment.
        record: PathBuf,
    },
    /// Every port in the allocation range is taken.
    #[error("no free port is left in {start}-{end} for `{variable}`; widen `session.port_range`")]
    Exhausted {
        /// The variable that could not be assigned.
        variable: String,
        /// Lowest port of the allocation range.
        start: u16,
        /// Highest port of the allocation range.
        end: u16,
    },
    /// The registry record of a live session cannot be read, so its ports are unknown.
    #[error("cannot read the port registry record {}: {message}", path.display())]
    UnreadableRecord {
        /// Path of the record.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
}

/// A session's resolved name and parent domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// Hostname label naming the session, such as `luup2`.
    pub name: String,
    /// Parent domain, such as `localhost`.
    pub domain: String,
}

impl SessionIdentity {
    /// Resolves the identity from the config, defaulting the name to the slugified basename of
    /// the config directory.
    #[must_use]
    pub fn resolve(settings: &SessionConfig, config_dir: &Path) -> Self {
        let name = settings.name.as_ref().map_or_else(
            || {
                let directory =
                    std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
                directory
                    .file_name()
                    .map(|name| slugify(&name.to_string_lossy()))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| FALLBACK_SESSION_NAME.to_string())
            },
            |name| name.inner.clone(),
        );
        let domain = settings
            .domain
            .as_ref()
            .map_or_else(|| DEFAULT_DOMAIN.to_string(), |domain| domain.inner.clone());
        Self { name, domain }
    }

    /// The domain every route of this session lives under, `<name>.<domain>`.
    #[must_use]
    pub fn session_domain(&self) -> String {
        format!("{}.{}", self.name, self.domain)
    }
}

/// Variables micromux injects into every interpolation and every service process.
///
/// Each one is pinned: an `env_file` line or inline `environment` entry of the same name is
/// ignored, and later references see the pinned value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionEnvironment {
    variables: IndexMap<String, String>,
}

impl SessionEnvironment {
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self {
            variables: pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    /// The pinned variables in injection order.
    #[must_use]
    pub fn variables(&self) -> &IndexMap<String, String> {
        &self.variables
    }

    /// Whether a configured definition of `key` is overridden by a pinned variable.
    ///
    /// Names compare exactly, as interpolation looks them up, so a pinned value and the
    /// references to it always agree.
    #[must_use]
    pub fn pins(&self, key: &str) -> bool {
        self.variables.contains_key(key)
    }

    /// Overlays the pinned variables onto a process environment.
    ///
    /// Windows folds environment names by case when it builds the child's environment, so there a
    /// differently cased definition is removed too; otherwise it could replace the pinned value.
    pub(crate) fn apply_to(&self, environment: &mut IndexMap<String, String>) {
        environment.retain(|key, _| {
            if cfg!(windows) {
                !self
                    .variables
                    .keys()
                    .any(|pinned| pinned.eq_ignore_ascii_case(key))
            } else {
                !self.pins(key)
            }
        });
        environment.extend(self.variables.clone());
    }

    /// `base` with the pinned variables layered on top.
    pub(crate) fn layered_over(
        &self,
        base: &std::collections::HashMap<String, String>,
    ) -> std::collections::HashMap<String, String> {
        let mut layered = base.clone();
        layered.retain(|key, _| !self.pins(key));
        layered.extend(self.variables.clone());
        layered
    }
}

/// A session's identity, its allocated ports, and the registry lease keeping them reserved.
#[derive(Debug)]
pub struct Session {
    identity: SessionIdentity,
    settings: SessionConfig,
    ports: IndexMap<String, u16>,
    provisional: Vec<String>,
    proxy_port: u16,
    preferred_proxy_port: u16,
    lease: Option<Lease>,
}

impl Session {
    /// Resolves the session for `config`, registers it as live, and reserves its allocated ports.
    ///
    /// A config loaded from a file is registered until the returned session (and every
    /// [`crate::Micromux`] built from it) is dropped, which keeps session names unique among live
    /// sessions and reserves the allocated ports.
    /// A config without a file path is not registered and can use neither `allocate_ports` nor
    /// `routes`.
    /// No allocated port ever equals the configured proxy ports.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AlreadyRunning`] when another micromux runs the same config,
    /// [`SessionError::NameInUse`] when another live session has the same name,
    /// [`SessionError::PortBusy`] when a persisted port is held by a foreign process,
    /// [`SessionError::PortReserved`] when a persisted port became a proxy port,
    /// [`SessionError::NoConfigPath`] when a config without a file path allocates ports or has
    /// routes, and an I/O variant when the registry cannot be accessed.
    pub fn establish<F>(
        config: &ConfigFile<F>,
        registry: &PortRegistry,
    ) -> Result<Self, SessionError> {
        Self::establish_with_proxy_port(config, registry, None)
    }

    /// Establishes a session with an optional preferred proxy port override.
    ///
    /// The override replaces `proxy.port` for port allocation and proxy startup.
    /// The configured fallback still applies, and reloads compare the original config settings.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::establish`], with the override excluded from allocation.
    pub fn establish_with_proxy_port<F>(
        config: &ConfigFile<F>,
        registry: &PortRegistry,
        proxy_port: Option<std::num::NonZeroU16>,
    ) -> Result<Self, SessionError> {
        let settings = config.config.session.clone();
        let identity = SessionIdentity::resolve(&settings, &config.config_dir);
        let proxy_port =
            proxy_port.map_or_else(|| settings.proxy.port(), std::num::NonZeroU16::get);
        let Some(config_path) = config.config_path.as_deref() else {
            if settings.needs_config_path() {
                return Err(SessionError::NoConfigPath);
            }
            return Ok(Self {
                identity,
                settings,
                ports: IndexMap::new(),
                provisional: Vec::new(),
                proxy_port,
                preferred_proxy_port: proxy_port,
                lease: None,
            });
        };
        let requests = port_requests(&settings);
        let excluded = std::iter::once(proxy_port)
            .chain(settings.proxy.fallback_port())
            .collect::<Vec<_>>();
        let (ports, lease) = registry.acquire(
            config_path,
            &identity.name,
            &requests,
            &excluded,
            &settings.port_range(),
        )?;
        Ok(Self {
            identity,
            settings,
            ports,
            provisional: Vec::new(),
            proxy_port,
            preferred_proxy_port: proxy_port,
            lease: Some(lease),
        })
    }

    /// Resolves the session for validation without reserving anything.
    ///
    /// Names with a persisted assignment the config still allows use it; every other name gets a
    /// distinct provisional port, listed by [`Self::provisional_ports`], since the next start
    /// assigns it anew.
    /// A name whose persisted port is not its preferred one is provisional too, showing the
    /// preferred port the next start tries first; other sessions' records are not consulted.
    /// Every other provisional port comes from the port range.
    /// Crate-private because a preview holds no reservation and must never run services.
    pub(crate) fn preview<F>(config: &ConfigFile<F>, registry: &PortRegistry) -> Self {
        let settings = config.config.session.clone();
        let identity = SessionIdentity::resolve(&settings, &config.config_dir);
        let persisted = config
            .config_path
            .as_deref()
            .and_then(|path| registry.persisted(path))
            .unwrap_or_default();
        let range = settings.port_range();
        let requests = port_requests(&settings);
        let kept = |request: &PortRequest| {
            persisted
                .get(&request.name)
                .copied()
                .filter(|port| request.allows(*port, &range))
        };
        let kept_ports = requests
            .iter()
            .filter_map(kept)
            .collect::<std::collections::HashSet<_>>();
        // Provisional range ports skip every port another name keeps or prefers.
        let reserved = requests
            .iter()
            .filter_map(|request| request.prefer)
            .chain(kept_ports.iter().copied())
            .collect::<std::collections::HashSet<_>>();
        let mut ports = IndexMap::new();
        let mut provisional = Vec::new();
        let mut next = *range.start();
        for request in &requests {
            let kept_port = kept(request);
            // Mirrors the registry: a preferred port another name keeps is out of reach.
            let preferred = request
                .prefer
                .filter(|prefer| kept_port != Some(*prefer) && !kept_ports.contains(prefer));
            let port = match (preferred, kept_port) {
                (Some(prefer), _) => {
                    provisional.push(request.name.clone());
                    prefer
                }
                (None, Some(port)) => port,
                (None, None) => {
                    provisional.push(request.name.clone());
                    let port = (next..=u16::MAX)
                        .find(|candidate| {
                            !reserved.contains(candidate)
                                && !ports.values().any(|assigned| assigned == candidate)
                        })
                        .unwrap_or(next);
                    next = port.saturating_add(1);
                    port
                }
            };
            ports.insert(request.name.clone(), port);
        }
        Self {
            identity,
            proxy_port: settings.proxy.port(),
            preferred_proxy_port: settings.proxy.port(),
            settings,
            ports,
            provisional,
            lease: None,
        }
    }

    /// The session's name and domain.
    #[must_use]
    pub fn identity(&self) -> &SessionIdentity {
        &self.identity
    }

    /// The allocated port of every `allocate_ports` name, in config order.
    #[must_use]
    pub fn ports(&self) -> &IndexMap<String, u16> {
        &self.ports
    }

    /// Whether the session has routes and therefore needs the shared proxy.
    #[must_use]
    pub fn has_routes(&self) -> bool {
        !self.settings.routes.is_empty()
    }

    /// The effective proxy port preference, including an override, and the configured fallback.
    #[must_use]
    pub fn proxy_preference(&self) -> (u16, Option<u16>) {
        (
            self.preferred_proxy_port,
            self.settings.proxy.fallback_port(),
        )
    }

    /// Latches the port the shared proxy actually listens on, which the injected
    /// `MICROMUX_PROXY_PORT` and `MICROMUX_HOST_SUFFIX` report.
    ///
    /// Without a call, the configured preferred port is assumed.
    pub fn set_proxy_port(&mut self, port: u16) {
        self.proxy_port = port;
    }

    /// Names whose port in a [`Self::preview`] is made up because none was assigned yet.
    pub(crate) fn provisional_ports(&self) -> &[String] {
        &self.provisional
    }

    /// The variables this session injects.
    #[must_use]
    pub fn environment(&self) -> SessionEnvironment {
        let mut variables = IndexMap::new();
        variables.insert(SESSION_VARIABLE.to_string(), self.identity.name.clone());
        variables.insert(
            SESSION_DOMAIN_VARIABLE.to_string(),
            self.identity.session_domain(),
        );
        variables.insert(PROXY_PORT_VARIABLE.to_string(), self.proxy_port.to_string());
        variables.insert(
            HOST_SUFFIX_VARIABLE.to_string(),
            host_suffix(&self.identity, self.proxy_port),
        );
        for (name, port) in &self.ports {
            variables.insert(name.clone(), port.to_string());
        }
        SessionEnvironment { variables }
    }

    pub(crate) fn into_context(self) -> SessionContext {
        SessionContext {
            environment: self.environment(),
            identity: self.identity,
            settings: self.settings,
            ports: self.ports,
            proxy_port: self.proxy_port,
            lease: self.lease,
        }
    }
}

/// The latched session a running supervisor resolves every config load against.
#[derive(Debug)]
pub(crate) struct SessionContext {
    pub(crate) identity: SessionIdentity,
    pub(crate) settings: SessionConfig,
    pub(crate) environment: SessionEnvironment,
    pub(crate) ports: IndexMap<String, u16>,
    pub(crate) proxy_port: u16,
    #[expect(
        dead_code,
        reason = "held for RAII: the registry lease keeps the allocated ports reserved"
    )]
    pub(crate) lease: Option<Lease>,
}

#[cfg(all(test, unix))]
impl SessionContext {
    /// A session that injects nothing, matching services materialized without a session.
    pub(crate) fn unconfigured() -> Self {
        Self {
            identity: SessionIdentity {
                name: "test".to_string(),
                domain: DEFAULT_DOMAIN.to_string(),
            },
            settings: SessionConfig::default(),
            environment: SessionEnvironment::default(),
            ports: IndexMap::new(),
            proxy_port: 80,
            lease: None,
        }
    }
}

/// The port each `allocate_ports` entry asks the registry for, in config order.
fn port_requests(settings: &SessionConfig) -> Vec<PortRequest> {
    settings
        .allocate_ports
        .iter()
        .map(|allocation| PortRequest {
            name: allocation.name.inner.clone(),
            prefer: allocation.prefer.as_ref().map(|prefer| prefer.inner),
        })
        .collect()
}

/// What follows `<route>.` in a browser URL: the session domain, plus the proxy port unless it is
/// the HTTP default.
#[must_use]
pub fn host_suffix(identity: &SessionIdentity, proxy_port: u16) -> String {
    if proxy_port == 80 {
        identity.session_domain()
    } else {
        format!("{}:{proxy_port}", identity.session_domain())
    }
}

/// Turns arbitrary text into a hostname label: lowercase ASCII letters, digits, and single
/// hyphens, without leading or trailing hyphens, at most 63 bytes.
#[must_use]
pub fn slugify(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(MAX_LABEL_LEN);
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Whether `label` is a lowercase hostname label: letters, digits, and inner hyphens.
#[must_use]
pub fn is_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Whether `hostname` is one or more dot-separated lowercase hostname labels.
#[must_use]
pub fn is_hostname(hostname: &str) -> bool {
    hostname.len() <= MAX_HOSTNAME_LEN && hostname.split('.').all(is_hostname_label)
}

/// Why a name cannot receive an allocated port, or `None` when it can.
#[must_use]
pub fn allocation_name_problem(name: &str) -> Option<&'static str> {
    let mut bytes = name.bytes();
    let valid = bytes
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
    if !valid {
        return Some("must be an environment variable name ([A-Za-z_][A-Za-z0-9_]*)");
    }
    if name.len() > MAX_ALLOCATION_NAME_LEN {
        return Some("is longer than 128 characters");
    }
    if name.to_ascii_uppercase().starts_with("MICROMUX_") {
        return Some("the MICROMUX_ prefix is reserved for variables micromux injects");
    }
    if SPAWN_OWNED_VARIABLES
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(name))
    {
        return Some("micromux sets this variable on every service itself");
    }
    None
}

/// A stable digest of a canonical config path, keying everything micromux stores per session.
#[must_use]
pub fn config_path_key(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(path_bytes(path).as_ref());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt as _;

    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(path.to_string_lossy().into_owned().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    #[test]
    fn slugify_produces_hostname_labels() {
        assert_eq!(slugify("luup2"), "luup2");
        assert_eq!(slugify("My Project_v2"), "my-project-v2");
        assert_eq!(slugify("--a..b--"), "a-b");
        assert_eq!(slugify("äöü"), "");
        assert_eq!(slugify(&"x".repeat(80)).len(), MAX_LABEL_LEN);
        assert!(is_hostname_label(&slugify("weird -- name -")));
    }

    #[test]
    fn hostnames_are_lowercase_labels() {
        assert!(is_hostname("localhost"));
        assert!(is_hostname("dev.example.test"));
        assert!(!is_hostname("Localhost"));
        assert!(!is_hostname("a..b"));
        assert!(!is_hostname("-a"));
        assert!(!is_hostname(""));
    }

    /// Allocation names must be plain variable names that neither micromux's own injected
    /// variables nor its spawn-time terminal variables already claim.
    #[test]
    fn allocation_names_reject_reserved_and_invalid_names() {
        assert_eq!(allocation_name_problem("API_PORT"), None);
        assert_eq!(allocation_name_problem("_port2"), None);
        assert!(allocation_name_problem("2PORT").is_some());
        assert!(allocation_name_problem("API-PORT").is_some());
        assert!(allocation_name_problem("MICROMUX_SESSION").is_some());
        assert!(allocation_name_problem("micromux_x").is_some());
        assert!(allocation_name_problem("force_color").is_some());
    }

    fn session_config(yaml: &str) -> color_eyre::eyre::Result<ConfigFile<usize>> {
        let yaml = format!("version: 1\n{yaml}\nservices:\n  app:\n    command: [\"true\"]\n");
        let mut config = crate::config::from_str(
            &yaml,
            Path::new("/projects/one"),
            0usize,
            None,
            &mut Vec::new(),
        )?;
        config.config_path = Some(PathBuf::from("/projects/one/micromux.yaml"));
        Ok(config)
    }

    #[test]
    fn proxy_override_is_excluded_from_allocations_and_keeps_source_settings() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = session_config(
            "session: { port_range: { start: 40000, end: 40999 } }\nallocate_ports: [WEB]",
        )?;
        let mut session = Session::establish_with_proxy_port(
            &config,
            &registry,
            std::num::NonZeroU16::new(40000),
        )?;

        assert!(session.ports().values().all(|port| *port != 40000));
        assert_eq!(session.proxy_preference(), (40000, None));
        assert!(session.settings.same_settings(&config.config.session));

        // Latching an actual listener port does not change the startup preference.
        session.set_proxy_port(8181);
        assert_eq!(session.proxy_preference(), (40000, None));
        assert_eq!(
            session.environment().variables.get(PROXY_PORT_VARIABLE),
            Some(&"8181".to_string())
        );
        Ok(())
    }

    /// A port range change reassigns ports at the next start, and validation shows the ports it
    /// will replace as provisional values from the new range.
    #[test]
    fn port_range_changes_reassign_ports() -> color_eyre::eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let default = session_config("allocate_ports: [A]")?;
        let ranged = session_config(
            "session: { port_range: { start: 40000, end: 40999 } }\nallocate_ports: [A]",
        )?;

        drop(Session::establish(&default, &registry)?);
        let preview = Session::preview(&ranged, &registry);
        let established = Session::establish(&ranged, &registry)?;

        assert_eq!(preview.provisional_ports(), ["A".to_string()]);
        assert_eq!(preview.ports().get("A"), Some(&40000));
        assert!(
            established
                .ports()
                .values()
                .all(|port| (40000..=40999).contains(port))
        );
        assert!(
            !default.config.session.same_settings(&ranged.config.session),
            "a changed range must not pass as the latched settings"
        );
        Ok(())
    }

    /// Validation shows a preferred port as the provisional value of a name the session has no
    /// port for yet, and gives the other names distinct ports from the range.
    #[test]
    fn previews_show_preferred_ports_as_provisional() -> color_eyre::eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        // WEB prefers the first port of the range, which API would otherwise get.
        let config = session_config(
            "session: { port_range: { start: 40000, end: 40999 } }\n\
             allocate_ports: [API, { name: WEB, prefer: 40000 }]",
        )?;

        let preview = Session::preview(&config, &registry);

        assert_eq!(
            preview.provisional_ports(),
            ["API".to_string(), "WEB".to_string()]
        );
        assert_eq!(preview.ports().get("WEB"), Some(&40000));
        assert_eq!(preview.ports().get("API"), Some(&40001));
        Ok(())
    }

    /// Validation shows a persisted port as provisional when the name now prefers another one,
    /// since the next start tries the preferred port first, and keeps the other names' ports.
    #[test]
    fn previews_show_persisted_fallbacks_as_provisional() -> color_eyre::eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let plain = session_config("allocate_ports: [WEB, API]")?;
        let preferring = session_config("allocate_ports: [{ name: WEB, prefer: 3000 }, API]")?;
        let established = Session::establish(&plain, &registry)?;
        let api = established.ports().get("API").copied();
        drop(established);

        let preview = Session::preview(&preferring, &registry);

        assert_eq!(preview.provisional_ports(), ["WEB".to_string()]);
        assert_eq!(preview.ports().get("WEB"), Some(&3000));
        assert_eq!(preview.ports().get("API").copied(), api);
        Ok(())
    }

    /// The digest is part of persisted file names and control endpoints, so it must never change.
    #[test]
    fn config_path_key_is_stable() {
        assert_eq!(
            config_path_key(Path::new("/home/user/project/micromux.yaml")),
            "3f98f83e6e2c8d23"
        );
    }

    /// Windows folds environment names by case, so a differently cased definition must not
    /// survive next to the pinned one.
    #[cfg(windows)]
    #[test]
    fn pinned_variables_replace_differently_cased_definitions_on_windows() {
        let pinned = SessionEnvironment::from_pairs(&[("API_PORT", "20001")]);
        let mut environment = IndexMap::new();
        environment.insert("api_port".to_string(), "9540".to_string());

        pinned.apply_to(&mut environment);

        assert_eq!(environment.len(), 1);
        assert_eq!(
            environment.get("API_PORT").map(String::as_str),
            Some("20001")
        );
    }

    /// Pinned variables replace configured definitions of the same name and keep their order.
    #[test]
    fn pinned_variables_override_configured_ones() {
        let mut variables = IndexMap::new();
        variables.insert("API_PORT".to_string(), "20001".to_string());
        let pinned = SessionEnvironment { variables };

        let mut environment = IndexMap::new();
        environment.insert("API_PORT".to_string(), "9540".to_string());
        environment.insert("OTHER".to_string(), "x".to_string());
        pinned.apply_to(&mut environment);

        assert_eq!(
            environment.get("API_PORT").map(String::as_str),
            Some("20001")
        );
        assert_eq!(environment.get("OTHER").map(String::as_str), Some("x"));
    }
}
