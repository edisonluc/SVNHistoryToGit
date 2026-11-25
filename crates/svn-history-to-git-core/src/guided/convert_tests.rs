use super::*;
use crate::guided::ScopeRef;
use std::process::Command;

fn run(cwd: &Path, program: &str, args: &[&str]) -> Vec<u8> {
    let out = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn git(cwd: &Path, args: &[&str]) -> String {
    String::from_utf8(run(cwd, "git", args))
        .unwrap()
        .trim()
        .into()
}

#[tokio::test]
async fn root_snapshot_applies_only_confirmed_exclusions_and_lfs() {
    use sha2::{Digest, Sha256};
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path();
    run(cwd, "svnadmin", &["create", "source"]);
    let url = url::Url::from_file_path(cwd.join("source"))
        .unwrap()
        .to_string();
    let content = vec![0xDC; 1 << 20];
    std::fs::write(cwd.join("input"), &content).unwrap();
    run(
        cwd,
        "svnmucc",
        &[
            "-U",
            &url,
            "-m",
            "assets",
            "mkdir",
            "assets",
            "put",
            "input",
            "assets/large.bin",
            "put",
            "input",
            "excluded.bin",
        ],
    );
    let uuid = String::from_utf8(run(
        cwd,
        "svn",
        &["info", "--show-item", "repos-uuid", &url],
    ))
    .unwrap()
    .trim()
    .into();
    let manifest = Manifest {
        svn_url: url,
        uuid,
        revision: 1,
        remote: "test".into(),
        web_url: String::new(),
        refs: vec![ScopeRef {
            kind: "trunk".into(),
            path: String::new(),
            name: "delivery".into(),
        }],
        svn_credential_id: None,
        git_credential_id: None,
        authors: vec![],
        exclude_globs: vec!["excluded.bin".into()],
        lfs_threshold_mb: Some(1),
    };
    let dest = cwd.join("target");
    let refs = convert(&manifest, &dest, &[], None, &|_, _| {})
        .await
        .unwrap();
    assert_eq!(refs[0].files, 2);
    assert_eq!(
        git(&dest, &["ls-tree", "-r", "--name-only", "delivery"])
            .lines()
            .collect::<Vec<_>>(),
        ["assets/.gitattributes", "assets/large.bin"]
    );
    let digest = hex::encode(Sha256::digest(&content));
    assert_eq!(
        git(&dest, &["show", "delivery:assets/large.bin"]),
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize 1048576")
    );
    let object = dest
        .join("lfs/objects")
        .join(&digest[..2])
        .join(&digest[2..4])
        .join(&digest);
    assert_eq!(std::fs::read(&object).unwrap(), content);
    std::fs::write(object, b"tampered").unwrap();
    let error = verify::selected(
        &Svn { secret: None },
        &manifest,
        &manifest.refs[0],
        &dest,
        &[],
        &|_, _| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains(&digest), "{error}");
}

#[tokio::test]
async fn preserves_selected_svn_histories_and_repository_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path();
    run(cwd, "svnadmin", &["create", "source"]);
    let url = url::Url::from_file_path(cwd.join("source"))
        .unwrap()
        .to_string();
    let put = |name: &str, content: &[u8]| {
        let path = cwd.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let source = put("text", b"$Id$\nrun\n");
    let binary = put("binary", &[0, 255, 10, 13]);
    let link = put("link", b"link script");
    let commit = |msg: &str, ops: &[&str]| {
        let mut args = vec!["--username", "alice", "-U", &url, "-m", msg];
        args.extend_from_slice(ops);
        run(cwd, "svnmucc", &args);
    };
    commit(
        "initial",
        &[
            "mkdir",
            "old",
            "mkdir",
            "old/trunk",
            "mkdir",
            "old/branches",
            "mkdir",
            "old/tags",
            "put",
            &source,
            "old/trunk/script",
            "propset",
            "svn:keywords",
            "Id",
            "old/trunk/script",
            "propset",
            "svn:eol-style",
            "CRLF",
            "old/trunk/script",
            "propset",
            "svn:executable",
            "yes",
            "old/trunk/script",
            "put",
            &link,
            "old/trunk/symlink",
            "propset",
            "svn:special",
            "yes",
            "old/trunk/symlink",
            "put",
            &binary,
            "old/trunk/%E8%B5%84%E6%96%99.bin",
        ],
    );
    commit("history only", &["mkdir", "old/trunk/empty"]);
    commit("unrelated", &["mkdir", "other"]);
    commit(
        "release from r1",
        &[
            "cp",
            "1",
            "old/trunk",
            "old/branches/release",
            "cp",
            "1",
            "old/trunk",
            "old/tags/changed",
            "cp",
            "1",
            "old/trunk",
            "old/tags/before-rename",
        ],
    );
    commit(
        "project renamed",
        &[
            "mv",
            "old",
            "project",
            "propset",
            "review:status",
            "confirmed",
            "project/tags",
        ],
    );
    // svnmucc takes URL arguments. Encoding names here also avoids the Windows
    // program's locale-dependent conversion of non-ASCII command line text.
    commit(
        "tag from r2",
        &["cp", "2", "old/trunk", "project/tags/%E7%89%88%20%23%25%40"],
    );
    commit("tag changed", &["rm", "project/tags/changed/script"]);
    commit(
        "after confirmation",
        &["rm", "project/trunk/%E8%B5%84%E6%96%99.bin"],
    );
    let uuid = String::from_utf8(run(cwd, "svnlook", &["uuid", "source"]))
        .unwrap()
        .trim()
        .into();
    let selected = |kind: &str, path: &str, name: &str| ScopeRef {
        kind: kind.into(),
        path: path.into(),
        name: name.into(),
    };
    let manifest = Manifest {
        svn_url: format!("{url}/project"),
        uuid,
        revision: 7,
        remote: "test".into(),
        web_url: String::new(),
        svn_credential_id: None,
        git_credential_id: None,
        authors: vec![],
        exclude_globs: vec![],
        lfs_threshold_mb: None,
        refs: vec![
            selected("trunk", "trunk", "main"),
            selected("branch", "branches/release", "release"),
            selected("tag", "tags/版 #%@", "v1"),
            selected("tag", "tags/changed", "changed"),
            selected("tag", "tags/before-rename", "before-rename"),
        ],
    };
    let authors = [AuthorEntry {
        svn_user: "alice".into(),
        git_name: "Alice 测试".into(),
        git_email: "alice@example.org".into(),
        commit_count: 0,
        updated_at: String::new(),
    }];
    let dest = cwd.join("git");
    let report = convert(&manifest, &dest, &authors, None, &|_, _| {})
        .await
        .unwrap();
    assert_eq!(
        report.iter().map(|r| r.revisions).collect::<Vec<_>>(),
        [3, 3, 3, 4, 3]
    );
    assert_eq!(
        report.iter().map(|r| r.files).collect::<Vec<_>>(),
        [3, 3, 3, 2, 3]
    );
    assert_eq!(
        git(&dest, &["log", "--format=%s", "main"])
            .lines()
            .collect::<Vec<_>>(),
        ["project renamed", "history only", "initial"]
    );
    assert_eq!(
        git(&dest, &["rev-parse", "main^"]),
        git(&dest, &["rev-parse", "refs/tags/v1^{}"])
    );
    assert_eq!(
        git(&dest, &["rev-parse", "refs/tags/before-rename^{}"]),
        git(&dest, &["rev-parse", "main^^"])
    );
    let moved_tag = git(&dest, &["cat-file", "tag", "before-rename"]);
    assert!(
        moved_tag.contains("release from r1") && moved_tag.contains("project renamed"),
        "{moved_tag}"
    );
    let edited_tag = format!("{}\n", moved_tag.replace("project renamed", "wrong move"));
    let bad_tag = super::git_line(
        &dest,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        Some(edited_tag.into_bytes()),
    )
    .await
    .unwrap();
    git(&dest, &["update-ref", "refs/tags/before-rename", &bad_tag]);
    let error = verify::selected(
        &Svn { secret: None },
        &manifest,
        &manifest.refs[4],
        &dest,
        &authors,
        &|_, _| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("before-rename") && error.contains("元数据"),
        "{error}"
    );
    assert_eq!(
        git(&dest, &["merge-base", "main", "release"]),
        git(&dest, &["rev-parse", "main^^"])
    );
    assert_eq!(
        git(&dest, &["show", "-s", "--format=%an <%ae>", "main"]),
        "Alice 测试 <alice@example.org>"
    );
    assert_eq!(git(&dest, &["cat-file", "-t", "v1"]), "tag");
    assert_eq!(
        run(&dest, "git", &["show", "main:资料.bin"]),
        [0, 255, 10, 13]
    );
    assert_eq!(run(&dest, "git", &["show", "main:script"]), b"$Id$\nrun\n");
    assert_eq!(run(&dest, "git", &["show", "main:symlink"]), b"script");
    assert!(git(&dest, &["ls-tree", "main", "script"]).starts_with("100755 blob "));
    assert!(git(&dest, &["ls-tree", "main", "symlink"]).starts_with("120000 blob "));
    assert_eq!(
        git(&dest, &["log", "-1", "--format=%s", "changed"]),
        "tag changed"
    );
    assert!(git(&dest, &["ls-tree", "changed", "script"]).is_empty());

    // Preserve the complete tip tree and metadata, but remove its empty-directory
    // ancestor. A HEAD-only verifier would accept this rewritten history.
    let raw = String::from_utf8(run(&dest, "git", &["cat-file", "commit", "main"])).unwrap();
    let middle = git(&dest, &["rev-parse", "main^"]);
    let oldest = git(&dest, &["rev-parse", "main^^"]);
    let broken = raw.replace(&format!("parent {middle}\n"), &format!("parent {oldest}\n"));
    let install = |body: String| {
        let path = cwd.join("rewritten-commit");
        std::fs::write(&path, body).unwrap();
        let oid = git(
            &dest,
            &["hash-object", "-t", "commit", "-w", path.to_str().unwrap()],
        );
        git(&dest, &["update-ref", "refs/heads/main", &oid]);
    };
    install(broken);
    let audit = || {
        verify::selected(
            &Svn { secret: None },
            &manifest,
            &manifest.refs[0],
            &dest,
            &authors,
            &|_, _| {},
        )
    };
    let error = audit().await.unwrap_err().to_string();
    assert!(error.contains("r2") && error.contains("r1"), "{error}");

    // Keep every revision and author intact, but substitute a tree missing a file.
    let good_tree = git(&dest, &["rev-parse", "main^{tree}"]);
    let missing_script = git(&dest, &["rev-parse", "changed^{tree}"]);
    install(raw.replace(
        &format!("tree {good_tree}\n"),
        &format!("tree {missing_script}\n"),
    ));
    let error = audit().await.unwrap_err().to_string();
    assert!(error.contains("script"), "{error}");
}
