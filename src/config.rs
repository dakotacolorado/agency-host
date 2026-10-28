use serde::Deserialize;
use std::collections::BTreeMap;
use std::{env, fs, io, path::PathBuf};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub service_url: String,
    pub token: String,
    pub host_id: String,
    /// How many workers this host will run sessions for at once. Unset, the heartbeat leaves
    /// it out and the service places nothing here: room is the owner's to
    /// state, not the host's to deduce from the machine.
    pub capacity: Option<u32>,
    /// What work this host can be matched to, as its owner names it. Checked
    /// by the service, whose refusal names the pattern a label must match.
    pub labels: Option<Vec<String>>,
    /// The checkouts a worker can be given a worktree of, by the name a
    /// worker's `repositories` use for each. Listing one here is not
    /// advertising it: that is a `repo:` label, which is the owner's to add
    /// alongside.
    pub repositories: BTreeMap<String, PathBuf>,
    /// The tmux server workers' sessions run on, by its `-L` name.
    pub tmux_socket: String,
    /// Where each worker has its folder. Two hosts on one machine need two of
    /// these as well as two sockets, or each reports the other's workers.
    pub workers_dir: PathBuf,
}

pub const DEFAULT_TMUX_SOCKET: &str = "worker-host";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_url: Option<String>,
    token: Option<String>,
    host_id: Option<String>,
    capacity: Option<u32>,
    labels: Option<Vec<String>>,
    repositories: Option<BTreeMap<String, PathBuf>>,
    tmux_socket: Option<String>,
    workers_dir: Option<PathBuf>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("worker-host").join("config.toml"))
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
    let service_url = pick("WORKER_HOST_SERVICE_URL", file.service_url)
        .ok_or("no service url: set service_url in the config file or WORKER_HOST_SERVICE_URL")?;
    let token = pick("WORKER_HOST_TOKEN", file.token)
        .ok_or("no token: set token in the config file or WORKER_HOST_TOKEN")?;
    let host_id = pick("WORKER_HOST_ID", file.host_id)
        .or_else(crate::sys::hostname)
        .ok_or("no host id: set host_id in the config file or WORKER_HOST_ID")?;
    let tmux_socket = pick("WORKER_HOST_TMUX_SOCKET", file.tmux_socket)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_TMUX_SOCKET.to_string());
    // In plain sight in the person's home rather than under a data directory:
    // a worker's folder is where a person goes to see its work.
    let workers_dir = match pick("WORKER_HOST_WORKERS_DIR", None)
        .map(PathBuf::from)
        .or(file.workers_dir)
    {
        Some(dir) => dir,
        None => var("HOME")
            .filter(|h| !h.is_empty())
            .map(|home| PathBuf::from(home).join("workers"))
            .ok_or("no workers_dir and no HOME to put ~/workers in")?,
    };
    if !workers_dir.is_absolute() {
        return Err(format!(
            "workers_dir {} is not an absolute path",
            workers_dir.display()
        ));
    }
    let capacity = match var("WORKER_HOST_CAPACITY").filter(|v| !v.is_empty()) {
        Some(n) => Some(
            n.parse()
                .map_err(|_| format!("WORKER_HOST_CAPACITY {n}: not a number"))?,
        ),
        None => file.capacity,
    };
    let labels = var("WORKER_HOST_LABELS")
        .filter(|v| !v.is_empty())
        .map(|v| v.split(',').map(|l| l.trim().to_string()).collect())
        .or(file.labels);
    let repositories = match var("WORKER_HOST_REPOSITORIES").filter(|v| !v.is_empty()) {
        Some(v) => v
            .split(',')
            .map(|pair| match pair.split_once('=') {
                Some((name, path)) => Ok((name.trim().to_string(), PathBuf::from(path.trim()))),
                None => Err(format!("WORKER_HOST_REPOSITORIES {pair}: not name=path")),
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
        tmux_socket,
        workers_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        map.entry("HOME".into())
            .or_insert_with(|| "/home/me".into());
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
                 capacity = 2\nlabels = [\"macos\", \"repo:workplace-ui\"]",
            ),
            env(&[("HOME", "/home/me")]),
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                service_url: "https://beta.example".into(),
                token: "t".into(),
                host_id: "studio".into(),
                capacity: Some(2),
                labels: Some(vec!["macos".into(), "repo:workplace-ui".into()]),
                repositories: BTreeMap::new(),
                tmux_socket: "worker-host".into(),
                workers_dir: "/home/me/workers".into(),
            }
        );
    }

    #[test]
    fn the_tmux_socket_is_named_in_the_file_or_the_environment() {
        let base = "service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\n";
        let config = from_parts(file(&format!("{base}tmux_socket = \"beta\"")), env(&[])).unwrap();
        assert_eq!(config.tmux_socket, "beta");
        let config = from_parts(
            file(&format!("{base}tmux_socket = \"beta\"")),
            env(&[("WORKER_HOST_TMUX_SOCKET", "prod")]),
        )
        .unwrap();
        assert_eq!(config.tmux_socket, "prod");
    }

    #[test]
    fn an_unset_tmux_socket_is_worker_hosts_own() {
        let base = "service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\n";
        for text in [base.to_string(), format!("{base}tmux_socket = \"\"")] {
            let config = from_parts(file(&text), env(&[("WORKER_HOST_TMUX_SOCKET", "")])).unwrap();
            assert_eq!(config.tmux_socket, "worker-host");
        }
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
                ("WORKER_HOST_CAPACITY", "3"),
                ("WORKER_HOST_LABELS", "linux, power:mains"),
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
            env(&[("WORKER_HOST_CAPACITY", "lots")]),
        )
        .unwrap_err();
        assert!(err.contains("WORKER_HOST_CAPACITY lots"), "{err}");
    }

    #[test]
    fn the_environment_overrides_the_file() {
        let config = from_parts(
            file("service_url = \"https://prod.example\"\ntoken = \"file\"\nhost_id = \"a\""),
            env(&[
                ("WORKER_HOST_SERVICE_URL", "https://beta.example"),
                ("WORKER_HOST_TOKEN", "env"),
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
                "{base}repositories = {{ workplace-ui = \"/src/workplace-ui\" }}"
            )),
            env(&[]),
        )
        .unwrap();
        assert_eq!(
            config.repositories,
            BTreeMap::from([("workplace-ui".into(), "/src/workplace-ui".into())])
        );
        let config = from_parts(
            file(base),
            env(&[("WORKER_HOST_REPOSITORIES", "a=/src/a, b = /src/b")]),
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
            env(&[("WORKER_HOST_REPOSITORIES", "a=src/a")]),
        )
        .unwrap_err();
        assert!(err.contains("repository a: src/a"), "{err}");
        let err = from_parts(
            file("service_url = \"https://x\"\ntoken = \"t\""),
            env(&[("WORKER_HOST_REPOSITORIES", "a")]),
        )
        .unwrap_err();
        assert!(err.contains("not name=path"), "{err}");
    }

    #[test]
    fn a_missing_token_is_named() {
        let err = from_parts(file("service_url = \"https://x\""), env(&[])).unwrap_err();
        assert!(err.contains("WORKER_HOST_TOKEN"), "{err}");
    }

    #[test]
    fn workers_live_in_home_unless_the_file_or_the_environment_says_otherwise() {
        let base = "service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\n";
        let config = from_parts(file(base), env(&[("HOME", "/home/me")])).unwrap();
        assert_eq!(config.workers_dir, PathBuf::from("/home/me/workers"));
        let config = from_parts(
            file(&format!("{base}workers_dir = \"/srv/workers\"")),
            env(&[("HOME", "/home/me")]),
        )
        .unwrap();
        assert_eq!(config.workers_dir, PathBuf::from("/srv/workers"));
        let config = from_parts(
            file(&format!("{base}workers_dir = \"/srv/workers\"")),
            env(&[("WORKER_HOST_WORKERS_DIR", "/beta/workers")]),
        )
        .unwrap();
        assert_eq!(config.workers_dir, PathBuf::from("/beta/workers"));
        let err =
            from_parts(file(base), env(&[("WORKER_HOST_WORKERS_DIR", "workers")])).unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(toml::from_str::<File>("service = \"https://x\"").is_err());
    }

    #[test]
    fn an_explicit_file_must_exist() {
        let err = load(Some("/nonexistent/worker-host.toml".into()), env(&[])).unwrap_err();
        assert!(err.contains("/nonexistent/worker-host.toml"), "{err}");
    }
}
