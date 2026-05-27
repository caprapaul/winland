use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to parse config TOML")]
    Parse(#[from] toml::de::Error),
}

pub fn parse_toml(input: &str) -> Result<Config, ConfigError> {
    Ok(toml::from_str(input)?)
}

fn default_log_level() -> String {
    "info".to_owned()
}
