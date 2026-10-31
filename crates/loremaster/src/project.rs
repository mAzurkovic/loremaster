//! Project identity from a working directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Normalizes a remote URL to `host/owner/repo`, so SSH and HTTPS forms of one remote are equal.
/// Returns `None` when it is not a network URL.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.contains(char::is_whitespace) {
        return None;
    }
    let (authority, path) = match url.split_once("://") {
        Some((_scheme, rest)) => rest.split_once('/')?,
        // scp-like syntax: [user@]host:path
        None => url.split_once(':')?,
    };
    let host_port = authority.rsplit('@').next()?;
    let host = host_port.split(':').next()?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_ascii_lowercase()))
}

fn last_segment(cwd: &Path) -> String {
    cwd.file_name()
        .unwrap_or(cwd.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn resolve(cwd: &Path) -> String {
    let remote = match gix::discover(cwd) {
        Ok(repo) => repo
            .config_snapshot()
            .string("remote.origin.url")
            .map(|url| url.to_string()),
        Err(e) => {
            // Not being in a repo is normal; a repo that cannot be read is not.
            if gix::discover::upwards(cwd).is_ok() {
                tracing::warn!(cwd = %cwd.display(), error = %e, "cannot read git repository");
            }
            None
        }
    };
    remote
        .and_then(|url| {
            let normalized = normalize_remote(&url);
            if normalized.is_none() {
                // The URL itself may embed a credential, so it stays out of the log.
                tracing::warn!("origin is not a recognized remote URL");
            }
            normalized
        })
        .unwrap_or_else(|| last_segment(cwd))
}

/// Resolves project IDs, remembering the answer per working directory.
#[derive(Default)]
pub struct ProjectIds {
    cache: Mutex<HashMap<PathBuf, String>>,
}

impl ProjectIds {
    /// Returns the normalized `origin` of the repo containing `cwd`, or else its last segment.
    pub fn get(&self, cwd: &Path) -> String {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = cache.get(cwd) {
            return id.clone();
        }
        let id = resolve(cwd);
        cache.insert(cwd.to_owned(), id.clone());
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn remotes_normalize_to_one_form() {
        for (url, expected) in [
            ("git@github.com:Acme/Web.git", "github.com/Acme/Web"),
            ("https://GitHub.com/Acme/Web", "github.com/Acme/Web"),
            ("https://github.com/Acme/Web.git/", "github.com/Acme/Web"),
            (
                "ssh://git@gitlab.example.com:2222/team/app.git",
                "gitlab.example.com/team/app",
            ),
            ("https://user@bitbucket.org/t/r", "bitbucket.org/t/r"),
            ("git@github-work:a/api.git", "github-work/a/api"),
        ] {
            assert_eq!(normalize_remote(url).as_deref(), Some(expected), "{url}");
        }
    }

    #[test]
    fn non_network_values_do_not_normalize() {
        for url in [
            "not a url",
            "justtext",
            "/srv/git/x.git",
            "../x",
            "file:///srv/x",
            "host:",
            "https://github.com",
        ] {
            assert_eq!(normalize_remote(url), None, "{url}");
        }
    }

    fn git_repo(dir: &Path, origin: Option<&str>) {
        fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(status.status.success());
        };
        git(&["init", "-q"]);
        if let Some(origin) = origin {
            git(&["remote", "add", "origin", origin]);
        }
    }

    #[test]
    fn repo_with_origin_uses_it_from_any_subdirectory() {
        let root = tempfile::tempdir().unwrap();
        let web = root.path().join("web");
        git_repo(&web, Some("git@github.com:Acme/Web.git"));
        let nested = web.join("src/api");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(ProjectIds::default().get(&nested), "github.com/Acme/Web");
    }

    #[test]
    fn repo_without_remote_uses_its_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let tool = root.path().join("tool");
        git_repo(&tool, None);
        assert_eq!(ProjectIds::default().get(&tool), "tool");
    }

    #[test]
    fn plain_directory_uses_its_name() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root.path().join("scratch");
        fs::create_dir_all(&scratch).unwrap();
        assert_eq!(ProjectIds::default().get(&scratch), "scratch");
    }

    #[test]
    fn invalid_origin_falls_back_to_the_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("odd");
        git_repo(&repo, Some("not a url"));
        assert_eq!(ProjectIds::default().get(&repo), "odd");
    }

    #[test]
    fn corrupt_git_config_falls_back_to_the_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("broken");
        git_repo(&repo, Some("git@github.com:a/b.git"));
        fs::write(repo.join(".git/config"), "[[[ not config").unwrap();
        assert_eq!(ProjectIds::default().get(&repo), "broken");
    }

    #[test]
    fn missing_directory_uses_its_name() {
        assert_eq!(
            ProjectIds::default().get(Path::new("/nonexistent/gone")),
            "gone"
        );
    }

    #[test]
    fn filesystem_root_uses_the_root_itself() {
        assert_eq!(last_segment(Path::new("/")), "/");
    }

    #[test]
    fn answers_are_cached_per_directory() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("cached");
        git_repo(&repo, Some("git@github.com:a/b.git"));
        let ids = ProjectIds::default();
        assert_eq!(ids.get(&repo), "github.com/a/b");
        fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(ids.get(&repo), "github.com/a/b");
    }
}
