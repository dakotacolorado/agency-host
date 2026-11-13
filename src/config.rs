use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, PartialEq)]
pub struct Config {
    pub service_url: String,
    pub token: String,
    pub host_id: String,
    /// How many agents this host will run sessions for at once. Unset, the heartbeat leaves
    /// it out and the service places nothing here: room is the owner's to
    /// state, not the host's to deduce from the machine.
    pub capacity: Option<u32>,
    /// What work this host can be matched to, as its owner names it. Checked
    /// by the service, whose refusal names the pattern a label must match.
    pub labels: Option<Vec<String>>,
    /// Which tools a session may use when Claude Code asks.
    pub permissions: Permissions,
    /// Where each agent has its folder. Two hosts on one machine need two of
    /// these as well as two sockets, or each reports the other's agents.
    pub agents_dir: PathBuf,
}

/// The rule a permission request is answered by: a tool named in `deny` is
/// refused, then one named in `allow` is given, and any other refused. A name
/// ending in `*` is every tool it begins, so `*` is every tool and
/// `mcp__github__*` every tool of that server. Unset, every tool is allowed,
/// as when sessions ran with permissions skipped.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default = "every_tool")]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

fn every_tool() -> Vec<String> {
    vec!["*".into()]
}

impl Default for Permissions {
    fn default() -> Self {
        Permissions {
            allow: every_tool(),
            deny: vec![],
        }
    }
}

impl Permissions {
    pub fn allows(&self, tool: &str) -> bool {
        let names = |patterns: &[String]| {
            patterns.iter().any(|p| match p.strip_suffix('*') {
                Some(prefix) => tool.starts_with(prefix),
                None => p == tool,
            })
        };
        !names(&self.deny) && names(&self.allow)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    service_url: Option<String>,
    token: Option<String>,
    host_id: Option<String>,
    capacity: Option<u32>,
    labels: Option<Vec<String>>,
    /// Read by an older release, and accepted so a config that still names
    /// it is not refused.
    #[serde(rename = "tmux_socket")]
    _older: Option<toml::Value>,
    agents_dir: Option<PathBuf>,
    permissions: Option<Permissions>,
}

pub fn default_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("agent-host").join("config.toml"))
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
    // In plain sight in the person's home rather than under a data directory:
    // an agent's folder is where a person goes to see its work.
    let agents_dir = match file.agents_dir {
        Some(dir) => dir,
        None => home
            .filter(|h| !h.as_os_str().is_empty())
            .map(|home| home.join("agents"))
            .ok_or("no agents_dir and no HOME to put ~/agents in")?,
    };
    if !agents_dir.is_absolute() {
        return Err(format!(
            "agents_dir {} is not an absolute path",
            agents_dir.display()
        ));
    }
    Ok(Config {
        service_url: service_url.trim_end_matches('/').to_string(),
        token,
        host_id,
        capacity: file.capacity,
        labels: file.labels,
        agents_dir,
        permissions: file.permissions.unwrap_or_default(),
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
                agents_dir: "/home/me/agents".into(),
                permissions: Permissions::default(),
            }
        );
    }

    #[test]
    fn a_setting_an_older_release_read_is_accepted_and_changes_nothing() {
        let config = read(&format!("{BASE}tmux_socket = \"beta\"")).unwrap();
        assert_eq!(config, read(BASE).unwrap());
    }

    #[test]
    fn every_tool_is_allowed_unless_the_file_denies_it_or_allows_only_some() {
        let all = read(BASE).unwrap().permissions;
        assert!(all.allows("Bash") && all.allows("mcp__github__create_pr"));
        let some = read(&format!(
            "{BASE}[permissions]\nallow = [\"Read\", \"mcp__github__*\"]\ndeny = [\"mcp__github__merge*\"]"
        ))
        .unwrap()
        .permissions;
        assert!(some.allows("Read") && some.allows("mcp__github__create_pr"));
        assert!(!some.allows("Bash"), "not allowed");
        assert!(!some.allows("mcp__github__merge_pr"), "denied wins");
        assert!(!some.allows("Rea"), "a name is whole");
        let denied = read(&format!("{BASE}[permissions]\ndeny = [\"WebFetch\"]"))
            .unwrap()
            .permissions;
        assert!(denied.allows("Bash") && !denied.allows("WebFetch"));
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
    fn agents_live_in_home_unless_the_file_says_otherwise() {
        assert_eq!(
            read(BASE).unwrap().agents_dir,
            PathBuf::from("/home/me/agents")
        );
        let config = read(&format!("{BASE}agents_dir = \"/srv/agents\"")).unwrap();
        assert_eq!(config.agents_dir, PathBuf::from("/srv/agents"));
        let err = read(&format!("{BASE}agents_dir = \"agents\"")).unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(toml::from_str::<File>("service = \"https://x\"").is_err());
    }

    #[test]
    fn the_file_must_exist() {
        let err = load(Some("/nonexistent/agent-host.toml".into())).unwrap_err();
        assert!(err.contains("/nonexistent/agent-host.toml"), "{err}");
    }
}
