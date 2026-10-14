use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
use tokio::process::Command;

pub struct RepositoryRoot {
    path: PathBuf,
    directory: File,
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 64,
        "Repository names must have 1–64 ASCII characters"
    );
    ensure!(
        name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "Use letters, digits, hyphens and underscores; start with a letter or digit"
    );
    Ok(())
}

impl RepositoryRoot {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "Repository root must be absolute");
        let canonical = path
            .canonicalize()
            .context("Repository root must already exist")?;
        ensure!(
            canonical == path,
            "Repository root must be canonical and contain no symlinks"
        );
        let directory: File = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?
        .into();
        Ok(Self {
            path: canonical,
            directory,
        })
    }

    fn check_root(&self) -> Result<()> {
        let a = self.directory.metadata()?;
        let b = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino() && !b.file_type().is_symlink(),
            "Repository root was replaced"
        );
        Ok(())
    }

    pub fn directory(&self, name: &str) -> Result<(PathBuf, File)> {
        validate_name(name)?;
        self.check_root()?;
        let directory: File = openat(
            &self.directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context("Repository must be a real directory directly under REPOS_ROOT")?
        .into();
        let path = self.path.join(name);
        ensure!(
            path.canonicalize()? == path,
            "Symlink repository paths are not allowed"
        );
        let a = directory.metadata()?;
        let b = std::fs::symlink_metadata(&path)?;
        ensure!(
            a.dev() == b.dev() && a.ino() == b.ino(),
            "Repository changed while opening it"
        );
        Ok((path, directory))
    }

    pub async fn validate(&self, name: &str) -> Result<PathBuf> {
        let (path, directory) = self.directory(name)?;
        // v1 intentionally accepts ordinary repositories, not .git files pointing elsewhere.
        let _: File = openat(
            &directory,
            ".git",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .context(
            "Expected a .git directory; symlinked metadata and linked worktrees are not supported",
        )?
        .into();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            git(&path).args(["rev-parse", "--show-toplevel"]).output(),
        )
        .await
        .context("Git validation timed out")??;
        ensure!(
            output.status.success(),
            "Directory is not a usable Git repository"
        );
        let top = String::from_utf8(output.stdout)?;
        ensure!(
            Path::new(top.trim_end()) == path,
            "Repository Git top-level must match its registered directory"
        );
        self.directory(name)?;
        Ok(path)
    }

    pub async fn create(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        self.check_root()?;
        // mkdirat is exclusive: even a dangling symlink or empty directory causes failure.
        mkdirat(&self.directory, name, Mode::from_raw_mode(0o750))
            .context("Project already exists or cannot be created")?;
        let (path, _) = self.directory(name)?;
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            git(&path)
                .args(["init", "--initial-branch=main", "--template="])
                .output(),
        )
        .await
        .context("Git initialization timed out; the directory was preserved")??;
        if !output.status.success() {
            bail!("Git initialization failed; the new directory was preserved for inspection");
        }
        self.validate(name).await
    }
}

fn git(path: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .current_dir(path)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_traversal_and_shell_names() {
        for s in [
            "", ".", "..", "../x", "/tmp", "a/b", "a\\b", "-option", "a b", "a;id", "💥", "a.b",
        ] {
            assert!(validate_name(s).is_err(), "accepted {s:?}");
        }
        assert!(validate_name(&"a".repeat(65)).is_err());
        assert!(validate_name("mini-redis_2").is_ok());
    }

    #[tokio::test]
    async fn creation_is_exclusive_and_validated() {
        let temp = tempfile::tempdir().unwrap();
        let root = RepositoryRoot::open(temp.path()).unwrap();
        let p = root.create("project").await.unwrap();
        std::fs::write(p.join("keep"), "content").unwrap();
        assert!(root.create("project").await.is_err());
        assert_eq!(std::fs::read_to_string(p.join("keep")).unwrap(), "content");
        assert_eq!(root.validate("project").await.unwrap(), p);
        std::fs::create_dir(temp.path().join("plain")).unwrap();
        assert!(root.validate("plain").await.is_err());
    }

    #[tokio::test]
    async fn rejects_symlink_escapes_and_git_indirection() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = RepositoryRoot::open(temp.path()).unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();
        assert!(root.validate("escape").await.is_err());
        assert!(root.create("escape").await.is_err());
        std::fs::create_dir(temp.path().join("indirect")).unwrap();
        symlink(outside.path(), temp.path().join("indirect/.git")).unwrap();
        assert!(root.validate("indirect").await.is_err());
        let link = temp.path().join("root-link");
        symlink(outside.path(), &link).unwrap();
        assert!(RepositoryRoot::open(&link).is_err());
    }
}
