use serde::Deserialize;
use std::{env, fs, path::PathBuf};

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
    /// The tmux server workers' sessions run on, by its `-L` name.
    pub tmux_socket: String,
    /// Where each worker has its folder. Two hosts on one machine need two of
    /// these as well as two sockets, or each reports the other's workers.
    pub workers_dir: PathBuf,
}

pub const DEFAULT_TMUX_SOCKET: &str = "worker-host";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_url: Option<String>,
    token: Option<String>,
    host_id: Option<String>,
    capacity: Option<u32>,
    labels: Option<Vec<String>>,
    tmux_socket: Option<String>,
    workers_dir: Option<PathBuf>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("worker-host").join("config.toml"))
}

/// Reads the file, which is the only place settings come from.
pub fn load(path: Option<PathBuf>) -> Result<Config, String> {
    let path = path
        .or_else(default_path)
        .ok_or("no config file: pass --config PATH")?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    from_file(file, env::var_os("HOME").map(PathBuf::from))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn from_file(file: File, home: Option<PathBuf>) -> Result<Config, String> {
    let service_url = file.service_url.ok_or("no service_url")?;
    let token = file.token.ok_or("no token")?;
    let host_id = file
        .host_id
        .or_else(crate::sys::hostname)
        .ok_or("no host_id, and no hostname to use instead")?;
    let tmux_socket = file
        .tmux_socket
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_TMUX_SOCKET.to_string());
    // In plain sight in the person's home rather than under a data directory:
    // a worker's folder is where a person goes to see its work.
    let workers_dir = match file.workers_dir {
        Some(dir) => dir,
        None => home
            .filter(|h| !h.as_os_str().is_empty())
            .map(|home| home.join("workers"))
            .ok_or("no workers_dir and no HOME to put ~/workers in")?,
    };
    if !workers_dir.is_absolute() {
        return Err(format!(
            "workers_dir {} is not an absolute path",
            workers_dir.display()
        ));
    }
    Ok(Config {
        service_url: service_url.trim_end_matches('/').to_string(),
        token,
        host_id,
        capacity: file.capacity,
        labels: file.labels,
        tmux_socket,
        workers_dir,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "service_url = \"https://x\"\ntoken = \"t\"\nhost_id = \"a\"\n";

    fn read(text: &str) -> Result<Config, String> {
        from_file(toml::from_str(text).unwrap(), Some("/home/me".into()))
    }

    #[test]
    fn reads_the_file() {
        let config = read(
            "service_url = \"https://beta.example/\"\ntoken = \"t\"\nhost_id = \"studio\"\n\
             capacity = 2\nlabels = [\"macos\", \"linux\"]",
        )
        .unwrap();
        assert_eq!(
            config,
            Config {
                service_url: "https://beta.example".into(),
                token: "t".into(),
                host_id: "studio".into(),
                capacity: Some(2),
                labels: Some(vec!["macos".into(), "linux".into()]),
                tmux_socket: "worker-host".into(),
                workers_dir: "/home/me/workers".into(),
            }
        );
    }

    #[test]
    fn an_unset_or_empty_tmux_socket_is_worker_hosts_own() {
        for text in [BASE.to_string(), format!("{BASE}tmux_socket = \"\"")] {
            assert_eq!(read(&text).unwrap().tmux_socket, "worker-host");
        }
        let config = read(&format!("{BASE}tmux_socket = \"beta\"")).unwrap();
        assert_eq!(config.tmux_socket, "beta");
    }

    #[test]
    fn capacity_and_labels_are_left_unset_rather_than_guessed() {
        let config = read(BASE).unwrap();
        assert_eq!(config.capacity, None);
        assert_eq!(config.labels, None);
    }

    #[test]
    fn a_missing_token_is_named() {
        let err = read("service_url = \"https://x\"").unwrap_err();
        assert!(err.contains("no token"), "{err}");
    }

    #[test]
    fn workers_live_in_home_unless_the_file_says_otherwise() {
        assert_eq!(
            read(BASE).unwrap().workers_dir,
            PathBuf::from("/home/me/workers")
        );
        let config = read(&format!("{BASE}workers_dir = \"/srv/workers\"")).unwrap();
        assert_eq!(config.workers_dir, PathBuf::from("/srv/workers"));
        let err = read(&format!("{BASE}workers_dir = \"workers\"")).unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(toml::from_str::<File>("service = \"https://x\"").is_err());
    }

    #[test]
    fn the_file_must_exist() {
        let err = load(Some("/nonexistent/worker-host.toml".into())).unwrap_err();
        assert!(err.contains("/nonexistent/worker-host.toml"), "{err}");
    }
}
