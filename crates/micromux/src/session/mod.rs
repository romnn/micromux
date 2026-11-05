//! Session identity, session-allocated ports, and the variables injected into every service.
//!
//! Several checkouts of one project can each run a session at the same time.
//! A session has a hostname-safe name, and it can ask for TCP ports that stay the same across its
//! restarts while no two live sessions of the same user ever share one.
//! Both reach services through pinned environment variables that override every `env_file` and
//! inline `environment` definition of the same name.

mod registry;

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::config::{ConfigFile, SessionConfig};

pub use registry::PortRegistry;
pub(crate) use registry::{Lease, PORT_RANGE};

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
    /// No per-user state directory could be determined for the port registry.
    #[error("cannot determine a per-user state directory for the port registry")]
    NoStateDirectory,
    /// Allocation needs the config file's path, which identifies the session across restarts.
    #[error("allocate_ports needs a config loaded from a file")]
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
    #[error("no free port is left in {}-{} for `{variable}`", PORT_RANGE.start(), PORT_RANGE.end())]
    Exhausted {
        /// The variable that could not be assigned.
        variable: String,
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
    lease: Option<Lease>,
}

impl Session {
    /// Resolves the session for `config`, registers it as live, and reserves its allocated ports.
    ///
    /// A config loaded from a file is registered until the returned session (and every
    /// [`crate::Micromux`] built from it) is dropped, which keeps session names unique among live
    /// sessions and reserves the allocated ports.
    /// A config without a file path is not registered and cannot allocate ports.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::AlreadyRunning`] when another micromux runs the same config,
    /// [`SessionError::NameInUse`] when another live session has the same name,
    /// [`SessionError::PortBusy`] when a persisted port is held by a foreign process,
    /// [`SessionError::NoConfigPath`] when a config without a file path allocates ports, and an
    /// I/O variant when the registry cannot be accessed.
    pub fn establish<F>(
        config: &ConfigFile<F>,
        registry: &PortRegistry,
    ) -> Result<Self, SessionError> {
        Self::establish_excluding(config, registry, &[])
    }

    /// Like [`Self::establish`], but never assigns any port in `excluded`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::establish`], plus [`SessionError::PortReserved`] when a persisted port is
    /// in `excluded`.
    pub fn establish_excluding<F>(
        config: &ConfigFile<F>,
        registry: &PortRegistry,
        excluded: &[u16],
    ) -> Result<Self, SessionError> {
        let settings = config.config.session.clone();
        let identity = SessionIdentity::resolve(&settings, &config.config_dir);
        let Some(config_path) = config.config_path.as_deref() else {
            if !settings.allocate_ports.is_empty() {
                return Err(SessionError::NoConfigPath);
            }
            return Ok(Self {
                identity,
                settings,
                ports: IndexMap::new(),
                provisional: Vec::new(),
                lease: None,
            });
        };
        let names = settings
            .allocate_ports
            .iter()
            .map(|name| name.inner.clone())
            .collect::<Vec<_>>();
        let (ports, lease) = registry.acquire(config_path, &identity.name, &names, excluded)?;
        Ok(Self {
            identity,
            settings,
            ports,
            provisional: Vec::new(),
            lease: Some(lease),
        })
    }

    /// Resolves the session for validation without reserving anything.
    ///
    /// Names with a persisted assignment use it; every other name gets a distinct provisional
    /// port, listed by [`Self::provisional_ports`].
    /// Crate-private because a preview holds no reservation and must never run services.
    pub(crate) fn preview<F>(config: &ConfigFile<F>, registry: &PortRegistry) -> Self {
        let settings = config.config.session.clone();
        let identity = SessionIdentity::resolve(&settings, &config.config_dir);
        let persisted = config
            .config_path
            .as_deref()
            .and_then(|path| registry.persisted(path))
            .unwrap_or_default();
        let mut ports = IndexMap::new();
        let mut provisional = Vec::new();
        let mut next = *PORT_RANGE.start();
        for name in &settings.allocate_ports {
            let port = if let Some(port) = persisted.get(&name.inner) {
                *port
            } else {
                provisional.push(name.inner.clone());
                next = next.saturating_add(1);
                next
            };
            ports.insert(name.inner.clone(), port);
        }
        Self {
            identity,
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
    #[expect(
        dead_code,
        reason = "held for RAII: the registry lease keeps the allocated ports reserved"
    )]
    pub(crate) lease: Option<Lease>,
}

#[cfg(test)]
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
            lease: None,
        }
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
