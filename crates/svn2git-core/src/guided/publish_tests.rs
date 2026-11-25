use super::*;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Repos {
    _dir: TempDir,
    work: PathBuf,
    bare: PathBuf,
    parent: String,
    tip: String,
    tag: String,
}

impl Repos {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        let bare = dir.path().join("destination.git");
        git(dir.path(), &["init", "-b", "local", "work"]);
        git(dir.path(), &["init", "--bare", "destination.git"]);
        git(&work, &["config", "user.name", "Migration Test"]);
        git(&work, &["config", "user.email", "test@example.org"]);
        git(&work, &["config", "commit.gpgSign", "false"]);
        git(&work, &["config", "tag.gpgSign", "false"]);
        git(&work, &["commit", "--allow-empty", "-m", "ancestor"]);
        let parent = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["commit", "--allow-empty", "-m", "tip"]);
        let tip = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["tag", "-a", "v1", "-m", "release"]);
        let tag = git(&work, &["rev-parse", "refs/tags/v1"]);
        Self {
            _dir: dir,
            work,
            bare,
            parent,
            tip,
            tag,
        }
    }

    fn url(&self) -> &str {
        self.bare.to_str().unwrap()
    }

    fn selected(&self) -> Vec<VerifiedRef> {
        vec![
            VerifiedRef {
                full_ref: "refs/heads/main".into(),
                oid: self.tip.clone(),
                commit_oid: self.tip.clone(),
                files: 0,
                revisions: 2,
            },
            VerifiedRef {
                full_ref: "refs/tags/v1".into(),
                oid: self.tag.clone(),
                commit_oid: self.tip.clone(),
                files: 0,
                revisions: 2,
            },
        ]
    }

    fn seed(&self, source: &str, destination: &str) {
        git(
            &self.work,
            &[
                "push",
                "--no-follow-tags",
                self.url(),
                &format!("{source}:{destination}"),
            ],
        );
    }

    fn names(&self) -> Vec<String> {
        git(&self.bare, &["for-each-ref", "--format=%(refname)"])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[tokio::test]
async fn tag_only_remote_is_not_empty_even_when_it_matches_selection() {
    let repo = Repos::new();
    let env = HashMap::new();
    check_empty(repo.url(), &env).await.unwrap();
    repo.seed(&repo.tag, "refs/tags/v1");
    assert!(check_empty(repo.url(), &env)
        .await
        .unwrap_err()
        .to_string()
        .contains("refs/tags/v1"));
    let before_write = AtomicBool::new(false);
    let result = publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &env,
        false,
        false,
        &|_, _| {
            before_write.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    assert!(result.is_err());
    assert!(!before_write.load(Ordering::SeqCst));
    assert_eq!(repo.names(), ["refs/tags/v1"]);
}

#[tokio::test]
async fn publishes_only_verified_oids_and_detects_refs_created_during_publication() {
    let repo = Repos::new();
    let refs = repo.selected();
    git(&repo.work, &["config", "push.followTags", "true"]);
    git(
        &repo.work,
        &["tag", "-a", "unselected", "-m", "must stay local"],
    );
    git(&repo.work, &["branch", "main", &repo.parent]);
    let raced = AtomicBool::new(false);
    let extra = publish(
        repo.url(),
        &repo.work,
        &refs,
        &HashMap::new(),
        false,
        false,
        &|phase, _| {
            if phase == "push" && !raced.swap(true, Ordering::SeqCst) {
                repo.seed(&repo.parent, "refs/heads/parallel");
            }
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(extra, ["refs/heads/parallel"]);
    assert_eq!(
        repo.names(),
        ["refs/heads/main", "refs/heads/parallel", "refs/tags/v1"]
    );
    assert_eq!(git(&repo.bare, &["rev-parse", "refs/heads/main"]), repo.tip);
    assert_eq!(git(&repo.bare, &["rev-parse", "refs/tags/v1"]), repo.tag);
    assert_eq!(git(&repo.bare, &["rev-parse", "refs/tags/v1^{}"]), repo.tip);
}

#[tokio::test]
async fn concurrent_fast_forwardable_destination_is_preserved_and_atomic_push_is_all_or_nothing() {
    let repo = Repos::new();
    let raced = AtomicBool::new(false);
    let result = publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|phase, _| {
            if phase == "push" && !raced.swap(true, Ordering::SeqCst) {
                repo.seed(&repo.parent, "refs/heads/main");
            }
            Ok(())
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        result.contains("refs/heads/main") && result.contains(&repo.parent),
        "{result}"
    );
    assert_eq!(repo.names(), ["refs/heads/main"]);
    assert_eq!(
        git(&repo.bare, &["rev-parse", "refs/heads/main"]),
        repo.parent
    );
}

#[tokio::test]
async fn partial_publication_resumes_and_full_retry_only_reads() {
    let repo = Repos::new();
    let refs = repo.selected();
    repo.seed(&repo.tip, "refs/heads/main");
    repo.seed(&repo.parent, "refs/heads/parallel");
    let env = HashMap::new();
    let extra = publish(repo.url(), &repo.work, &refs, &env, true, false, &|_, _| {
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(extra, ["refs/heads/parallel"]);
    let extra = publish(
        repo.url(),
        &repo.work,
        &refs,
        &env,
        true,
        false,
        &|phase, _| {
            assert_ne!(phase, "push");
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(extra, ["refs/heads/parallel"]);
    assert_eq!(git(&repo.bare, &["rev-parse", "refs/tags/v1"]), repo.tag);

    let mut incorrect = refs;
    incorrect[1].commit_oid = repo.parent.clone();
    let error = publish(
        repo.url(),
        &repo.work,
        &incorrect,
        &env,
        true,
        false,
        &|phase, _| {
            assert_ne!(phase, "push");
            Ok(())
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("refs/tags/v1") && error.contains(&repo.tip),
        "{error}"
    );
}

#[tokio::test]
async fn verification_retry_reports_a_deleted_branch_without_republishing_it() {
    let repo = Repos::new();
    let refs = repo.selected();
    let env = HashMap::new();
    publish(
        repo.url(),
        &repo.work,
        &refs,
        &env,
        false,
        false,
        &|_, _| Ok(()),
    )
    .await
    .unwrap();
    repo.seed(&repo.parent, "refs/heads/concurrent");
    assert_eq!(
        verify_remote(repo.url(), &repo.work, &refs, &env)
            .await
            .unwrap(),
        ["refs/heads/concurrent"]
    );
    git(
        &repo.bare,
        &["update-ref", "-d", "refs/heads/main", &repo.tip],
    );
    let error = verify_remote(repo.url(), &repo.work, &refs, &env)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("refs/heads/main"), "{error}");
    assert_eq!(repo.names(), ["refs/heads/concurrent", "refs/tags/v1"]);
}

#[tokio::test]
async fn server_without_atomic_support_creates_selected_refs_individually() {
    let repo = Repos::new();
    git(&repo.bare, &["config", "receive.advertiseAtomic", "false"]);
    publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(repo.names(), ["refs/heads/main", "refs/tags/v1"]);
}

#[tokio::test]
async fn unreadable_last_non_atomic_write_leaves_the_job_in_remote_verification() {
    let repo = Repos::new();
    git(&repo.bare, &["config", "receive.advertiseAtomic", "false"]);
    git(
        &repo.bare,
        &["config", "uploadpack.hideRefs", "refs/tags/v1"],
    );
    let last = std::sync::Mutex::new(String::new());
    let result = publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|phase, _| {
            *last.lock().unwrap() = phase.to_owned();
            Ok(())
        },
    )
    .await
    .unwrap_err();
    assert!(result.to_string().contains("refs/tags/v1"), "{result}");
    assert_eq!(*last.lock().unwrap(), "remote_verify");
    assert_eq!(repo.names(), ["refs/heads/main", "refs/tags/v1"]);
}

#[tokio::test]
async fn pushes_to_the_checked_repository_even_with_a_configured_push_rewrite() {
    let repo = Repos::new();
    let other = repo.work.join("other.git");
    git(&repo.work, &["init", "--bare", "other.git"]);
    git(
        &repo.work,
        &[
            "config",
            &format!("url.{}.pushInsteadOf", other.display()),
            repo.url(),
        ],
    );
    publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(repo.names(), ["refs/heads/main", "refs/tags/v1"]);
    assert!(git(&other, &["for-each-ref"]).is_empty());
}

#[tokio::test]
async fn checkpoint_write_failure_is_returned_even_if_remote_now_matches() {
    let repo = Repos::new();
    git(&repo.bare, &["config", "receive.advertiseAtomic", "false"]);
    let result = publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|phase, _| {
            // A concurrent matching tag must not swallow the first receipt's storage failure.
            if phase == "push" && repo.names().len() == 1 {
                repo.seed(&repo.tag, "refs/tags/v1");
                return Err(Svn2GitError::Storage("无法保存逐项发布记录".into()));
            }
            Ok(())
        },
    )
    .await;
    assert!(
        matches!(result, Err(Svn2GitError::Storage(_))),
        "{result:?}"
    );
    assert_eq!(repo.names(), ["refs/heads/main", "refs/tags/v1"]);
}

#[tokio::test]
async fn remote_verification_checks_the_tag_object_even_if_the_commit_still_matches() {
    let repo = Repos::new();
    let error = publish(
        repo.url(),
        &repo.work,
        &repo.selected(),
        &HashMap::new(),
        false,
        false,
        &|phase, _| {
            if phase == "remote_verify" {
                git(
                    &repo.bare,
                    &["update-ref", "refs/tags/v1", &repo.tip, &repo.tag],
                );
            }
            Ok(())
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains(&repo.tag) && error.contains(&repo.tip),
        "{error}"
    );
    assert_eq!(git(&repo.bare, &["rev-parse", "refs/tags/v1"]), repo.tip);
}

#[test]
fn accepts_clone_locations_and_rejects_git_helpers_unknown_schemes_and_options() {
    for accepted in [
        "http://git.example.org/team/repo.git",
        "https://git.example.org/team/repo.git",
        "ssh://git@git.example.org:2222/team/repo",
        "git@git.example.org:team/repo",
        "git@[::1]:team/repo",
    ] {
        validate_remote(accepted).unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    validate_remote(temp.path().to_str().unwrap()).unwrap();
    let file_url = url::Url::from_directory_path(temp.path()).unwrap();
    validate_remote(file_url.as_str()).unwrap();
    for rejected in [
        "--help",
        "custom::example",
        "ftp://example.org/repo",
        "git://example.org/repo",
        "https://user:pass@example.org/repo",
    ] {
        assert!(validate_remote(rejected).is_err(), "{rejected}");
    }
}

fn add_lfs_version(repo: &Repos, content: &str) -> String {
    std::fs::write(repo.work.join("asset.bin"), content).unwrap();
    git(&repo.work, &["add", ".gitattributes", "asset.bin"]);
    git(&repo.work, &["commit", "-m", content]);
    git(&repo.work, &["show", "HEAD:asset.bin"])
        .lines()
        .find_map(|line| line.strip_prefix("oid sha256:"))
        .unwrap()
        .to_owned()
}

fn lfs_object(git_dir: &Path, oid: &str) -> PathBuf {
    git_dir
        .join("lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
}

#[tokio::test]
async fn lfs_publishes_current_and_historical_objects_only_for_verified_histories() {
    let repo = Repos::new();
    git(&repo.work, &["lfs", "install", "--local", "--skip-repo"]);
    git(&repo.work, &["lfs", "track", "*.bin"]);
    // Exercise the explicit upload independently of the optional local pre-push hook.
    git(&repo.work, &["config", "core.hooksPath", "unused-hooks"]);
    let old = add_lfs_version(&repo, "old asset");
    let new = add_lfs_version(&repo, "released asset");
    let tip = git(&repo.work, &["rev-parse", "HEAD"]);
    let excluded = add_lfs_version(&repo, "unselected future asset");
    let mut refs = repo.selected();
    refs.truncate(1);
    refs[0].oid = tip.clone();
    refs[0].commit_oid = tip;
    let other = repo.work.join("other.git");
    git(&repo.work, &["init", "--bare", "other.git"]);
    let other_url = url::Url::from_directory_path(&other).unwrap();
    std::fs::write(
        repo.work.join(".lfsconfig"),
        format!("[lfs]\nurl = {other_url}\npushurl = {other_url}\nallowincompletepush = true\n"),
    )
    .unwrap();
    git(
        &repo.work,
        &[
            "config",
            &format!("url.{}.pushInsteadOf", other.display()),
            repo.url(),
        ],
    );

    let env = HashMap::new();
    let local_old = lfs_object(&repo.work.join(".git"), &old);
    let backup = local_old.with_extension("backup");
    std::fs::rename(&local_old, &backup).unwrap();
    let result = publish(repo.url(), &repo.work, &refs, &env, false, true, &|_, _| {
        Ok(())
    })
    .await;
    assert!(result.is_err(), "{result:?}");
    assert!(repo.names().is_empty());
    std::fs::rename(backup, local_old).unwrap();

    publish(repo.url(), &repo.work, &refs, &env, false, true, &|_, _| {
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(lfs_object(&repo.bare, &old)).unwrap(),
        b"old asset"
    );
    assert_eq!(
        std::fs::read(lfs_object(&repo.bare, &new)).unwrap(),
        b"released asset"
    );
    assert!(!lfs_object(&repo.bare, &excluded).exists());
    assert!(!lfs_object(&other, &new).exists());
    assert_eq!(repo.names(), ["refs/heads/main"]);
}
