// Push a staged TheDiscDB submission as a GitHub pull request via
// the `gh` CLI. Pipeline:
//
//   1. Confirm `gh` is on $PATH and the user is authenticated.
//   2. Ensure the user has a fork of TheDiscDb/data. If not, fork
//      via `gh repo fork --remote=false --clone=false`.
//   3. Maintain a local clone of that fork under $XDG_CACHE_HOME/
//      ripsaw/discdb-fork. If missing, clone fresh; if present,
//      pull main from upstream so we branch from the latest tip.
//   4. Copy the staged `data/...` tree into the fork checkout.
//   5. Create a feature branch, commit the staged files, push to
//      the fork, then `gh pr create` against TheDiscDb/data:main.
//
// All shell-outs use `gh`, `git`, and posix `cp` -- no extra deps.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

pub const UPSTREAM_REPO: &str = "TheDiscDb/data";
const UPSTREAM_DEFAULT_BRANCH: &str = "main";

/// Top-level result of `push_pr`. Carries the URL of the opened PR
/// so the caller can surface it in a toast / hyperlink.
#[derive(Debug, Clone)]
pub struct PrPushResult {
    pub branch: String,
    pub fork_repo: String,
    pub pr_url: String,
}

/// Inputs to `push_pr`. Built by the caller from the staging dir +
/// the user's GitHub identity (Preferences).
pub struct PrPushRequest<'a> {
    /// Absolute path to the staged submission's `data/` root (the
    /// directory that contains `movie/` or `series/` subfolders).
    pub staged_data_root: &'a Path,
    /// Subpath under `staged_data_root` that contains *just this
    /// submission's* tree (e.g. `series/The Many Loves of Dobie
    /// Gillis (1959)`). Required because the staging tree
    /// accumulates older submissions; without scoping, an earlier
    /// staged record would silently land in this PR.
    pub staged_subpath: &'a Path,
    /// Title slug used to build the branch name + PR title.
    pub slug: &'a str,
    /// Human-readable title for the PR (e.g. "The Many Loves of
    /// Dobie Gillis (1959) S1 D1+D2").
    pub pr_title: &'a str,
    /// PR body. Free-form Markdown.
    pub pr_body: &'a str,
}

pub fn push_pr(req: &PrPushRequest<'_>) -> Result<PrPushResult> {
    ensure_gh_available()?;
    let user = current_github_user().context(
        "could not resolve current GitHub user via `gh api user` -- run `gh auth login` first",
    )?;
    let fork_repo = ensure_fork(&user)?;
    let checkout = ensure_fork_checkout(&user)?;
    sync_main_from_upstream(&checkout)?;

    let branch = build_branch_name(req.slug);
    create_branch(&checkout, &branch)?;
    copy_staged_data(req.staged_data_root, req.staged_subpath, &checkout)?;
    commit_and_push(&checkout, &branch, req.pr_title)?;
    let pr_url = open_pr(&fork_repo, &branch, req.pr_title, req.pr_body)?;

    Ok(PrPushResult {
        branch,
        fork_repo,
        pr_url,
    })
}

fn ensure_gh_available() -> Result<()> {
    let out = Command::new("gh")
        .arg("--version")
        .output()
        .context("running `gh --version` -- is the GitHub CLI installed?")?;
    if !out.status.success() {
        bail!("`gh --version` exited {}", out.status);
    }
    Ok(())
}

/// Return `<owner>/<repo>` for the user's fork of TheDiscDb/data,
/// creating it on github.com if it doesn't yet exist. We avoid
/// cloning here (`--remote=false --clone=false`) because we manage
/// our own checkout under XDG cache.
fn ensure_fork(user: &str) -> Result<String> {
    let candidate = format!("{user}/data");
    // `gh repo view` returns non-zero when the repo doesn't exist.
    let probe = Command::new("gh")
        .args(["repo", "view", &candidate, "--json", "nameWithOwner"])
        .output()
        .context("probing for existing fork via gh repo view")?;
    if probe.status.success() {
        return Ok(candidate);
    }
    // Not found -- create. `gh repo fork <repo>` defaults to no
    // remote-add and no clone; we manage our own clone explicitly.
    // `--default-branch-only` keeps the fork lean.
    let status = Command::new("gh")
        .args([
            "repo",
            "fork",
            UPSTREAM_REPO,
            "--default-branch-only",
        ])
        .status()
        .context("creating fork via gh repo fork")?;
    if !status.success() {
        bail!("gh repo fork {UPSTREAM_REPO} exited {status}");
    }
    Ok(candidate)
}

fn fork_checkout_root() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            home.join(".cache")
        });
    cache.join("ripsaw").join("discdb-fork")
}

fn ensure_fork_checkout(user: &str) -> Result<PathBuf> {
    let path = fork_checkout_root();
    if path.join(".git").is_dir() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let repo = format!("{user}/data");
    let status = Command::new("gh")
        .args(["repo", "clone", &repo, path.to_str().unwrap()])
        .status()
        .context("cloning fork via gh repo clone")?;
    if !status.success() {
        bail!("gh repo clone {repo} exited {status}");
    }
    // Make sure upstream is wired so we can pull main from it.
    let _ = Command::new("git")
        .args(["-C", path.to_str().unwrap(), "remote", "add", "upstream",
              &format!("https://github.com/{UPSTREAM_REPO}.git")])
        .status();
    Ok(path)
}

fn sync_main_from_upstream(checkout: &Path) -> Result<()> {
    let checkout = checkout.to_str().unwrap();
    let res = Command::new("git")
        .args(["-C", checkout, "remote"])
        .output()?;
    let stdout = String::from_utf8_lossy(&res.stdout);
    if !stdout.lines().any(|l| l.trim() == "upstream") {
        let status = Command::new("git")
            .args([
                "-C",
                checkout,
                "remote",
                "add",
                "upstream",
                &format!("https://github.com/{UPSTREAM_REPO}.git"),
            ])
            .status()?;
        if !status.success() {
            bail!("git remote add upstream exited {status}");
        }
    }
    let status = Command::new("git")
        .args([
            "-C",
            checkout,
            "fetch",
            "upstream",
            UPSTREAM_DEFAULT_BRANCH,
        ])
        .status()?;
    if !status.success() {
        bail!("git fetch upstream {UPSTREAM_DEFAULT_BRANCH} exited {status}");
    }
    let status = Command::new("git")
        .args(["-C", checkout, "checkout", UPSTREAM_DEFAULT_BRANCH])
        .status()?;
    if !status.success() {
        bail!("git checkout {UPSTREAM_DEFAULT_BRANCH} exited {status}");
    }
    let status = Command::new("git")
        .args([
            "-C",
            checkout,
            "reset",
            "--hard",
            &format!("upstream/{UPSTREAM_DEFAULT_BRANCH}"),
        ])
        .status()?;
    if !status.success() {
        bail!("git reset --hard upstream/{UPSTREAM_DEFAULT_BRANCH} exited {status}");
    }
    Ok(())
}

fn build_branch_name(slug: &str) -> String {
    let safe: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("add-{safe}-{secs}")
}

fn create_branch(checkout: &Path, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["-C", checkout.to_str().unwrap(), "checkout", "-b", branch])
        .status()?;
    if !status.success() {
        bail!("git checkout -b {branch} exited {status}");
    }
    Ok(())
}

fn copy_staged_data(
    staged_data_root: &Path,
    staged_subpath: &Path,
    checkout: &Path,
) -> Result<()> {
    // Scope the copy to a single submission's subtree so an older
    // staged record from a previous session doesn't get swept into
    // this PR. `cp -r src/. dst/` merges src into dst without an
    // extra nesting level; the trailing `.` is what makes cp do that.
    let dst = checkout.join("data").join(staged_subpath);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(&dst)?;
    let src = staged_data_root.join(staged_subpath);
    if !src.is_dir() {
        bail!("staged subpath {} is not a directory", src.display());
    }
    let src_arg = format!("{}/.", src.display());
    let status = Command::new("cp")
        .args(["-r", &src_arg, dst.to_str().unwrap()])
        .status()?;
    if !status.success() {
        bail!("cp -r {src_arg} {} exited {status}", dst.display());
    }
    Ok(())
}

fn commit_and_push(checkout: &Path, branch: &str, message: &str) -> Result<()> {
    let checkout = checkout.to_str().unwrap();
    let status = Command::new("git")
        .args(["-C", checkout, "add", "data"])
        .status()?;
    if !status.success() {
        bail!("git add data exited {status}");
    }
    let status = Command::new("git")
        .args(["-C", checkout, "commit", "-m", message])
        .status()?;
    if !status.success() {
        bail!("git commit exited {status} -- nothing to commit, or git identity not configured?");
    }
    let status = Command::new("git")
        .args(["-C", checkout, "push", "-u", "origin", branch])
        .status()?;
    if !status.success() {
        bail!("git push -u origin {branch} exited {status}");
    }
    Ok(())
}

fn open_pr(fork_repo: &str, branch: &str, title: &str, body: &str) -> Result<String> {
    let head = format!("{}:{}", fork_repo.split('/').next().unwrap(), branch);
    let out = Command::new("gh")
        .args([
            "pr",
            "create",
            "--repo",
            UPSTREAM_REPO,
            "--base",
            UPSTREAM_DEFAULT_BRANCH,
            "--head",
            &head,
            "--title",
            title,
            "--body",
            body,
        ])
        .output()
        .context("gh pr create")?;
    if !out.status.success() {
        bail!(
            "gh pr create exited {} -- stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // gh prints the PR URL on stdout when successful.
    let url = String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        Err(anyhow!("gh pr create returned no URL"))
    } else {
        Ok(url)
    }
}

fn current_github_user() -> Result<String> {
    let out = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .context("gh api user")?;
    if !out.status.success() {
        bail!(
            "gh api user exited {} -- stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if user.is_empty() {
        bail!("gh api user returned an empty login")
    } else {
        Ok(user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_is_kebab_safe_and_includes_timestamp() {
        let b = build_branch_name("the-many-loves of dobie gillis!1959");
        assert!(b.starts_with("add-the-many-loves-of-dobie-gillis-1959-"));
        // Trailing timestamp: 10+ digits.
        let suffix = b.split('-').last().unwrap();
        assert!(suffix.chars().all(|c| c.is_ascii_digit()));
        assert!(suffix.len() >= 10);
    }

    #[test]
    fn fork_checkout_root_uses_xdg_cache_when_set() {
        // SAFETY: single-threaded test, env mutated and restored.
        let original = std::env::var_os("XDG_CACHE_HOME");
        std::env::set_var("XDG_CACHE_HOME", "/tmp/ripsaw-test-cache");
        let p = fork_checkout_root();
        assert_eq!(p, PathBuf::from("/tmp/ripsaw-test-cache/ripsaw/discdb-fork"));
        match original {
            Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
            None => std::env::remove_var("XDG_CACHE_HOME"),
        }
    }
}
