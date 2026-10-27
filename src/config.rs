use serde::Deserialize;
use std::collections::BTreeMap;
use std::{env, fs, io, path::PathBuf};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub service_url: String,
    pub token: String,
    pub host_id: String,
    /// How many agents this host will run at once. Unset, the beat leaves it
    /// out and the service places nothing here: room is the owner's to state,
    /// not the host's to deduce from the machine.
    pub capacity: Option<u32>,
    /// What work this host can be matched to, as its owner names it. Checked
    /// by the service, whose refusal names the pattern a label must match.
    pub labels: Option<Vec<String>>,
    /// The checkouts an agent can be given a worktree of, by the name a
    /// placement uses for each. Listing one here is not advertising it: that
    /// is a `repo:` label, which is the owner's to add alongside.
    pub repositories: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_url: Option<String>,
    token: Option<String>,
    host_id: Option<String>,
    capacity: Option<u32>,
    labels: Option<Vec<String>>,
    repositories: Option<BTreeMap<String, PathBuf>>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("agent-host").join("config.toml"))
}

/// Where agents' folders go: data, not configuration, and kept apart from any
/// checkout a person works in on the same machine.
pub fn agents_dir() -> Option<PathBuf> {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|dir| dir.join("agent-host").join("agents"))
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
    let capacity = match var("AGENT_HOST_CAPACITY").filter(|v| !v.is_empty()) {
        Some(n) => Some(
            n.parse()
                .map_err(|_| format!("AGENT_HOST_CAPACITY {n}: not a number"))?,
        ),
        None => file.capacity,
    };
    let labels = var("AGENT_HOST_LABELS")
        .filter(|v| !v.is_empty())
        .map(|v| v.split(',').map(|l| l.trim().to_string()).collect())
        .or(file.labels);
    let repositories = match var("AGENT_HOST_REPOSITORIES").filter(|v| !v.is_empty()) {
        Some(v) => v
            .split(',')
            .map(|pair| match pair.split_once('=') {
                Some((name, path)) => Ok((name.trim().to_string(), PathBuf::from(path.trim()))),
                None => Err(format!("AGENT_HOST_REPOSITORIES {pair}: not name=path")),
            })
            .collect::<Result<_, _>>()?,
        None => file.repositories.unwrap_or_default(),
    };
    // The host runs as a daemon whose working directory is nobody's choice.
    if let Some((name, path)) = repositories.iter().find(|(_, p)| !p.is_absolute()) {
        return Err(format!(
            "repository {name}: {} is not an absolute path",
            path.display()
        ));
    }
    Ok(Config {
        service_url: service_url.trim_end_matches('/').to_string(),
        token,
        host_id,
        capacity,
        labels,
        repositories,
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
            file(
                "service_url = \"https://beta.example/\"\ntoken = \"t\"\nhost_id = \"studio\"\n\
                 capacity = 2\nlabels = [\"macos\", \"repo:agent-service\"]",
            ),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                service_url: "https://beta.example".into(),
                token: "t".into(),
                host_id: "studio".into(),
                capacity: Some(2),
                labels: Some(vec!["macos".into(), "repo:agent-service".into()]),
                repositories: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn capacity_and_labels_are_left_unset_rather_than_guessed() {
        let config = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\""),
            env(&[]),
        )
        .unwrap();
        assert_eq!(config.capacity, None);
        assert_eq!(config.labels, None);
    }

    #[test]
    fn the_environment_names_capacity_and_labels() {
        let config = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\ncapacity = 1"),
            env(&[
                ("AGENT_HOST_CAPACITY", "3"),
                ("AGENT_HOST_LABELS", "linux, power:mains"),
            ]),
        )
        .unwrap();
        assert_eq!(config.capacity, Some(3));
        assert_eq!(
            config.labels,
            Some(vec!["linux".into(), "power:mains".into()])
        );
    }

    #[test]
    fn a_capacity_that_is_not_a_number_is_named() {
        let err = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\""),
            env(&[("AGENT_HOST_CAPACITY", "lots")]),
        )
        .unwrap_err();
        assert!(err.contains("AGENT_HOST_CAPACITY lots"), "{err}");
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
    fn repositories_are_named_in_the_file_or_the_environment() {
        let base = "service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\n";
        let config = from_parts(
            file(&format!(
                "{base}repositories = {{ agent-service = \"/src/agent-service\" }}"
            )),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            config.repositories,
            BTreeMap::from([("agent-service".into(), "/src/agent-service".into())])
        );
        let config = from_parts(
            file(base),
            env(&[("AGENT_HOST_REPOSITORIES", "a=/src/a, b = /src/b")]),
        )
        .unwrap();
        assert_eq!(
            config.repositories,
            BTreeMap::from([("a".into(), "/src/a".into()), ("b".into(), "/src/b".into())])
        );
    }

    #[test]
    fn a_repository_must_be_an_absolute_path() {
        let err = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\""),
            env(&[("AGENT_HOST_REPOSITORIES", "a=src/a")]),
        )
        .unwrap_err();
        assert!(err.contains("repository a: src/a"), "{err}");
        let err = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\""),
            env(&[("AGENT_HOST_REPOSITORIES", "a")]),
        )
        .unwrap_err();
        assert!(err.contains("not name=path"), "{err}");
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
