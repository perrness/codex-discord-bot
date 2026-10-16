use std::{collections::HashSet, env, os::unix::fs::DirBuilderExt, path::PathBuf, time::Duration};

use anyhow::{Context, Result, ensure};

pub const SUPPORTED_CODEX_VERSION: &str = "0.162.1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ApprovalsReviewer {
    #[default]
    AutoReview,
    User,
}

impl ApprovalsReviewer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutoReview => "auto_review",
            Self::User => "user",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AutoReview => "Approve for me",
            Self::User => "Ask for approval",
        }
    }
}

impl std::str::FromStr for ApprovalsReviewer {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto_review" => Ok(Self::AutoReview),
            "user" => Ok(Self::User),
            _ => anyhow::bail!("CODEX_APPROVALS_REVIEWER must be auto_review or user"),
        }
    }
}

// Deliberately not Debug: this object contains secrets.
pub struct Config {
    pub database_url: String,
    pub discord_token: String,
    pub access: Access,
    pub channel_id: u64,
    pub active_category_id: u64,
    pub completed_category_id: u64,
    pub channel_retention: Duration,
    pub repos_root: PathBuf,
    pub state_dir: PathBuf,
    pub codex_bin: PathBuf,
    pub codex_home: PathBuf,
    pub tool_path: String,
    pub model: Option<String>,
    pub validation: crate::validation::Policies,
    pub integration: crate::integration::Policies,
    pub issue_workers: IssueWorkers,
    pub approvals_reviewer: ApprovalsReviewer,
    pub rpc_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueWorkers {
    pub global: u32,
    pub per_repository: u32,
}

impl Default for IssueWorkers {
    fn default() -> Self {
        Self {
            global: 1,
            per_repository: 1,
        }
    }
}

impl IssueWorkers {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=32).contains(&self.global),
            "ISSUE_WORKERS_GLOBAL must be 1–32"
        );
        ensure!(
            (1..=32).contains(&self.per_repository),
            "ISSUE_WORKERS_PER_REPOSITORY must be 1–32"
        );
        Ok(())
    }

    fn load() -> Result<Self> {
        let workers = Self {
            global: env::var("ISSUE_WORKERS_GLOBAL")
                .unwrap_or_else(|_| "1".into())
                .parse()
                .context("Invalid ISSUE_WORKERS_GLOBAL")?,
            per_repository: env::var("ISSUE_WORKERS_PER_REPOSITORY")
                .unwrap_or_else(|_| "1".into())
                .parse()
                .context("Invalid ISSUE_WORKERS_PER_REPOSITORY")?,
        };
        workers.validate()?;
        Ok(workers)
    }
}

#[derive(Clone)]
pub struct Access {
    pub users: HashSet<u64>,
    pub guild: Option<u64>,
}

impl Access {
    pub fn allows(&self, user: u64, guild: Option<u64>, is_bot: bool) -> bool {
        !is_bot
            && guild.is_some()
            && self.users.contains(&user)
            && self.guild.is_none_or(|g| guild == Some(g))
    }
}

impl Config {
    pub fn prepare_directories(&self) -> Result<()> {
        // Resolve the actual locations before spawning a process with credentials.
        let root = self
            .repos_root
            .canonicalize()
            .context("REPOS_ROOT must exist")?;
        ensure!(
            root == self.repos_root,
            "REPOS_ROOT must be canonical, without symlinks"
        );
        for (label, path) in [
            ("STATE_DIR", &self.state_dir),
            ("CODEX_HOME", &self.codex_home),
        ] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
            let canonical = path.canonicalize()?;
            ensure!(
                canonical == *path && !canonical.starts_with(&root),
                "{label} must be canonical and outside REPOS_ROOT"
            );
        }
        Ok(())
    }

    pub fn load() -> Result<Self> {
        let users: HashSet<u64> = required("ALLOWED_USER_IDS")?
            .split(',')
            .map(|s| s.trim().parse::<u64>().context("Invalid ALLOWED_USER_IDS"))
            .collect::<Result<_>>()?;
        ensure!(
            !users.is_empty() && !users.contains(&0),
            "An explicit user allowlist is required"
        );
        let guild = env::var("DISCORD_GUILD_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|s| s.parse::<u64>().context("Invalid DISCORD_GUILD_ID"))
            .transpose()?;
        ensure!(guild != Some(0), "Invalid guild ID");
        let config = Self {
            database_url: required("DATABASE_URL")?,
            discord_token: required("DISCORD_TOKEN")?,
            access: Access { users, guild },
            // Preserve the old control-channel setting during upgrades. Zero means discover/create.
            channel_id: optional_id("DISCORD_CONTROL_CHANNEL_ID")?
                .or(optional_id("DISCORD_CHANNEL_ID")?)
                .unwrap_or(0),
            active_category_id: required("DISCORD_ACTIVE_CATEGORY_ID")?
                .parse()
                .context("Invalid active category ID")?,
            completed_category_id: required("DISCORD_COMPLETED_CATEGORY_ID")?
                .parse()
                .context("Invalid completed category ID")?,
            channel_retention: retention(
                &env::var("DISCORD_TASK_RETENTION_HOURS").unwrap_or_else(|_| "48".into()),
            )?,
            repos_root: path("REPOS_ROOT", Some("/repos"))?,
            state_dir: path("STATE_DIR", Some("/var/lib/codex-discord-bot"))?,
            codex_bin: path("CODEX_BIN", None)?,
            codex_home: path("CODEX_HOME", None)?,
            tool_path: required("TOOL_PATH")?,
            model: env::var("CODEX_MODEL").ok().filter(|v| !v.is_empty()),
            validation: match env::var("VALIDATION_CONFIG") {
                Ok(path) if !path.is_empty() => {
                    crate::validation::Policies::load(std::path::Path::new(&path))?
                }
                Ok(_) | Err(env::VarError::NotPresent) => Default::default(),
                Err(error) => return Err(error).context("Invalid VALIDATION_CONFIG"),
            },
            integration: match env::var("INTEGRATION_CONFIG") {
                Ok(path) if !path.is_empty() => {
                    crate::integration::Policies::load(std::path::Path::new(&path))?
                }
                Ok(_) | Err(env::VarError::NotPresent) => Default::default(),
                Err(error) => return Err(error).context("Invalid INTEGRATION_CONFIG"),
            },
            approvals_reviewer: match env::var("CODEX_APPROVALS_REVIEWER") {
                Ok(value) => value.parse()?,
                Err(env::VarError::NotPresent) => ApprovalsReviewer::default(),
                Err(error) => return Err(error).context("Invalid CODEX_APPROVALS_REVIEWER"),
            },
            rpc_timeout: Duration::from_secs(30),
            issue_workers: IssueWorkers::load()?,
        };
        ensure!(
            config.active_category_id != 0
                && config.completed_category_id != 0
                && config.active_category_id != config.completed_category_id,
            "Configure distinct, nonzero active and completed category IDs"
        );
        ensure!(
            !config.codex_home.starts_with(&config.repos_root),
            "CODEX_HOME must be outside repositories"
        );
        ensure!(
            !config.state_dir.starts_with(&config.repos_root),
            "STATE_DIR must be outside repositories"
        );
        Ok(config)
    }
}

fn optional_id(name: &str) -> Result<Option<u64>> {
    let value = env::var(name).ok().filter(|v| !v.is_empty());
    value
        .map(|v| {
            let id = v
                .parse::<u64>()
                .with_context(|| format!("Invalid {name}"))?;
            ensure!(id != 0, "Invalid {name}");
            Ok(id)
        })
        .transpose()
}

fn retention(value: &str) -> Result<Duration> {
    let hours: u64 = value
        .parse()
        .context("Invalid DISCORD_TASK_RETENTION_HOURS")?;
    ensure!(
        (1..=87600).contains(&hours),
        "Retention must be 1–87600 hours"
    );
    Ok(Duration::from_secs(hours * 3600))
}

pub fn required(key: &str) -> Result<String> {
    let value = env::var(key).with_context(|| format!("Missing {key}"))?;
    ensure!(!value.trim().is_empty(), "{key} must not be empty");
    Ok(value)
}

fn path(key: &str, default: Option<&str>) -> Result<PathBuf> {
    let p = PathBuf::from(
        env::var(key)
            .ok()
            .or_else(|| default.map(str::to_owned))
            .with_context(|| format!("Missing {key}"))?,
    );
    ensure!(p.is_absolute(), "{key} must be an absolute path");
    ensure!(
        !p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "{key} cannot contain .."
    );
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_defaults_to_auto_review_and_rejects_unknown_modes() {
        assert_eq!(ApprovalsReviewer::default(), ApprovalsReviewer::AutoReview);
        assert_eq!(
            "auto_review".parse::<ApprovalsReviewer>().unwrap(),
            ApprovalsReviewer::AutoReview
        );
        assert_eq!(
            "user".parse::<ApprovalsReviewer>().unwrap(),
            ApprovalsReviewer::User
        );
        for value in ["", "never", "allow", "auto", "USER"] {
            assert!(value.parse::<ApprovalsReviewer>().is_err());
        }
    }

    #[test]
    fn authorization_checks_all_entry_contexts() {
        let access = Access {
            users: [42].into(),
            guild: Some(7),
        };
        assert!(access.allows(42, Some(7), false));
        assert!(!access.allows(43, Some(7), false));
        assert!(!access.allows(42, Some(8), false));
        assert!(!access.allows(42, None, false));
        assert!(!access.allows(42, Some(7), true));
    }

    #[test]
    fn private_directories_cannot_resolve_inside_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repos");
        std::fs::create_dir(&root).unwrap();
        let mut config = Config {
            database_url: String::new(),
            discord_token: String::new(),
            access: Access {
                users: [42].into(),
                guild: Some(7),
            },
            channel_id: 8,
            active_category_id: 9,
            completed_category_id: 10,
            channel_retention: Duration::from_secs(48 * 3600),
            repos_root: root.clone(),
            state_dir: temp.path().join("state"),
            codex_home: temp.path().join("codex"),
            codex_bin: "/unused".into(),
            tool_path: "/usr/bin:/bin".into(),
            model: None,
            validation: Default::default(),
            integration: Default::default(),
            issue_workers: Default::default(),
            approvals_reviewer: ApprovalsReviewer::default(),
            rpc_timeout: Duration::from_secs(1),
        };
        config.prepare_directories().unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        config.codex_home = alias;
        assert!(config.prepare_directories().is_err());
        config.codex_home = root.join("credentials");
        assert!(config.prepare_directories().is_err());
    }

    #[test]
    fn retention_is_bounded_and_defaults_to_two_days() {
        assert_eq!(retention("48").unwrap(), Duration::from_secs(172800));
        for value in ["0", "-1", "forever", "87601"] {
            assert!(retention(value).is_err());
        }
    }

    #[test]
    fn issue_concurrency_defaults_to_one_and_is_bounded() {
        assert_eq!(
            IssueWorkers::default(),
            IssueWorkers {
                global: 1,
                per_repository: 1
            }
        );
        for (global, per_repository) in [(0, 1), (1, 0), (33, 1), (1, 33)] {
            assert!(
                IssueWorkers {
                    global,
                    per_repository
                }
                .validate()
                .is_err()
            );
        }
        IssueWorkers {
            global: 4,
            per_repository: 2,
        }
        .validate()
        .unwrap();
    }
}
