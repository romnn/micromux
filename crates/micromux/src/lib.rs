use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct ComposeFile {
    pub version: Option<String>,
    pub services: HashMap<String, Service>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Service {
    pub command: Option<String>,
    pub environment: Option<HashMap<String, String>>,
    pub depends_on: Option<Vec<String>>,
    pub healthcheck: Option<HealthCheck>,
    // ports, inputs, watch, restart policy
    // You can add more fields such as ports, volumes, etc.
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    /// The healthcheck test.
    ///
    /// E.g. ["pg_isready", "-U", "postgres"]
    pub test: Option<Vec<String>>,
    /// e.g. "30s"
    pub interval: Option<String>,
    /// e.g. "10s"
    pub timeout: Option<String>,
    /// Number of retries before marking unhealthy.
    pub retries: Option<u32>,
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    #[test]
    fn parse_config() -> eyre::Result<()> {
        let yaml = r#"
version: "3"
services:
  app:
    command: "./start.sh"
    environment:
      APP_ENV: production
      APP_DEBUG: "false"
    depends_on:
      - db
    healthcheck:
      test: ["CMD-SHELL", "curl -f http://localhost/health || exit 1"]
      interval: "30s"
      timeout: "10s"
      retries: 3
  db:
    environment:
      POSTGRES_PASSWORD: example
    healthcheck:
      test: ["CMD", "pg_isready", "-U", "postgres"]
      interval: "10s"
      timeout: "5s"
      retries: 5
"#;

        // Parse the YAML into our ComposeFile struct.
        let compose: super::ComposeFile = serde_yaml::from_str(yaml)?;
        println!("{:#?}", compose);

        Ok(())
    }
}
