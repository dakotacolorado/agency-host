use serde::Deserialize;
use std::{env, fs, io, path::PathBuf};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub service_url: String,
    pub token: String,
    pub host_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_url: Option<String>,
    token: Option<String>,
    host_id: Option<String>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("agent-host").join("config.toml"))
}

/// Reads the file, then lets the environment override it. A file that was
/// named explicitly must exist; the default one may be absent when the
/// environment carries everything.
pub fn load(path: Option<PathBuf>, var: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let explicit = path.is_some();
    let file = match path.or_else(default_path) {
        None => File::default(),
        Some(path) => match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !explicit => File::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        },
    };
    from_parts(file, var)
}

fn from_parts(file: File, var: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let pick =
        |name: &str, from_file: Option<String>| var(name).filter(|v| !v.is_empty()).or(from_file);
    let service_url = pick("AGENT_HOST_SERVICE_URL", file.service_url)
        .ok_or("no service url: set service_url in the config file or AGENT_HOST_SERVICE_URL")?;
    let token = pick("AGENT_HOST_TOKEN", file.token)
        .ok_or("no token: set token in the config file or AGENT_HOST_TOKEN")?;
    let host_id = pick("AGENT_HOST_ID", file.host_id)
        .or_else(crate::sys::hostname)
        .ok_or("no host id: set host_id in the config file or AGENT_HOST_ID")?;
    Ok(Config {
        service_url: service_url.trim_end_matches('/').to_string(),
        token,
        host_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn file(text: &str) -> File {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn reads_the_file() {
        let config = from_parts(
            file("service_url = \"https://beta.example/\"\ntoken = \"t\"\nhost_id = \"studio\""),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                service_url: "https://beta.example".into(),
                token: "t".into(),
                host_id: "studio".into(),
            }
        );
    }

    #[test]
    fn the_environment_overrides_the_file() {
        let config = from_parts(
            file("service_url = \"https://prod.example\"\ntoken = \"file\"\nhost_id = \"a\""),
            env(&[
                ("AGENT_HOST_SERVICE_URL", "https://beta.example"),
                ("AGENT_HOST_TOKEN", "env"),
            ]),
        )
        .unwrap();
        assert_eq!(config.service_url, "https://beta.example");
        assert_eq!(config.token, "env");
        assert_eq!(config.host_id, "a");
    }

    #[test]
    fn a_missing_token_is_named() {
        let err = from_parts(file("service_url = \"https://x\""), env(&[])).unwrap_err();
        assert!(err.contains("AGENT_HOST_TOKEN"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(toml::from_str::<File>("service = \"https://x\"").is_err());
    }

    #[test]
    fn an_explicit_file_must_exist() {
        let err = load(Some("/nonexistent/agent-host.toml".into()), env(&[])).unwrap_err();
        assert!(err.contains("/nonexistent/agent-host.toml"), "{err}");
    }
}
