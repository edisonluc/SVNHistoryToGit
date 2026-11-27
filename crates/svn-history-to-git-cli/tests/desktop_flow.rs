use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn invoke(root: &Path, program: &str, args: &[&str]) -> String {
    let result = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn events(root: &Path, request: Value) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_svn-history-to-git-cli"))
        .args([
            "--data-dir",
            root.join("state").to_str().unwrap(),
            "desktop",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let raw = String::from_utf8(out.stdout).unwrap();
    let values: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!values.is_empty(), "{:?}", out.stderr);
    assert_eq!(
        out.status.success(),
        values.last().unwrap()["type"] == "result",
        "{raw}"
    );
    values
}

fn request(root: &Path, req: Value) -> Value {
    let result = events(root, req).pop().unwrap();
    assert_eq!(result["type"], "result", "{result}");
    result["data"].clone()
}

fn svn_repository(root: &Path, name: &str) -> String {
    invoke(root, "svnadmin", &["create", name]);
    url::Url::from_file_path(root.join(name))
        .unwrap()
        .to_string()
}

fn svn_commit(root: &Path, base: &str, user: &str, message: &str, operations: &[&str]) {
    let mut args = vec!["-U", base, "--username", user, "-m", message];
    args.extend_from_slice(operations);
    invoke(root, "svnmucc", &args);
}

fn save_alice(root: &Path, email: &str) {
    request(
        root,
        json!({"op":"save_authors","authors":[
            {"svn_user":"alice","git_name":"Alice","git_email":email}
        ]}),
    );
}

fn author<'a>(rows: &'a Value, user: &str) -> &'a Value {
    rows.as_array()
        .unwrap()
        .iter()
        .find(|row| row["svn_user"] == user)
        .unwrap()
}

#[test]
fn inspect_nested_branch_project_uses_its_actual_layout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let base = svn_repository(root, "branch-layout");
    let branch = "branches/V0.X";
    let project = format!("{branch}/SampleApp");
    let url = format!("{base}/{project}");
    let mut dirs = vec!["branches".to_string(), branch.to_string(), project.clone()];
    for child in ["branches", "branches/internal", "tags", "tags/internal"] {
        dirs.push(format!("{project}/{child}"));
    }
    let operations: Vec<_> = dirs
        .iter()
        .flat_map(|dir| ["mkdir", dir.as_str()])
        .collect();
    svn_commit(root, &base, "dev", "flat project", &operations);

    let defaults =
        json!({"op":"inspect","url":url,"trunk":"trunk","branches":"branches","tags":"tags"});
    let mut auto = defaults.clone();
    auto["auto_branch_subdir"] = json!(true);
    let flat = request(root, auto.clone());
    assert_eq!(flat["url"], url);
    assert_eq!(flat["revision"], 1);
    assert_eq!(flat["trunk"], "");
    assert_eq!(flat["branches"], json!([]));
    assert_eq!(flat["tags"], json!([]));

    let mut custom = auto.clone();
    custom["trunk"] = json!("custom-main");
    let mut custom_branches = auto.clone();
    custom_branches["branches"] = json!("releases");
    for req in [defaults, custom, custom_branches] {
        let failure = events(root, req).pop().unwrap();
        assert_eq!(failure["type"], "error", "{failure}");
        assert!(failure["message"]
            .as_str()
            .unwrap()
            .contains("未找到主线目录"));
    }

    svn_commit(
        root,
        &base,
        "dev",
        "standard layout",
        &[
            "mkdir",
            &format!("{project}/trunk"),
            "mkdir",
            "trunk",
            "mkdir",
            "trunk/VehicleCheckDataPark",
            "mkdir",
            "trunk/VehicleCheckDataPark/src",
        ],
    );
    let structured = request(root, auto);
    assert_eq!(structured["url"], url);
    assert_eq!(structured["revision"], 2);
    assert_eq!(structured["trunk"], "trunk");
    assert_eq!(structured["branches"], json!(["branches/internal"]));
    assert_eq!(structured["tags"], json!(["tags/internal"]));

    let trunk_sub_url = format!("{base}/trunk/VehicleCheckDataPark");
    let trunk_sub = request(
        root,
        json!({
            "op":"inspect",
            "url":trunk_sub_url,
            "trunk":"trunk",
            "branches":"branches",
            "tags":"tags",
            "auto_branch_subdir":true
        }),
    );
    assert_eq!(trunk_sub["url"], trunk_sub_url);
    assert_eq!(trunk_sub["trunk"], "");
    assert_eq!(trunk_sub["branches"], json!([]));
    assert_eq!(trunk_sub["tags"], json!([]));
}

#[test]
fn author_scan_follows_only_selected_paths_at_the_pinned_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let base = svn_repository(root, "scan-source");
    let commit = |user: &str, ops: &[&str]| svn_commit(root, &base, user, user, ops);
    fs::write(root.join("file"), "source\n").unwrap();
    commit(
        "alice",
        &[
            "mkdir",
            "trunk",
            "mkdir",
            "branches",
            "put",
            "file",
            "trunk/file",
        ],
    );
    commit("copier", &["cp", "1", "trunk", "branches/release"]);
    commit("trunk-dev", &["put", "file", "trunk/new"]);
    commit("outsider", &["mkdir", "branches/unselected"]);
    commit("mover", &["mv", "branches/release", "branches/renamed"]);
    commit("late", &["put", "file", "branches/renamed/later"]);
    commit("removed", &["rm", "branches/renamed"]);

    let scan = |revision: u64, paths: &[&str]| {
        request(
            root,
            json!({"op":"scan_authors","url":base,"revision":revision,"paths":paths}),
        )
    };
    let users = |rows: &Value| {
        let mut names: Vec<_> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["svn_user"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
    };
    let renamed = scan(5, &["branches/renamed"]);
    assert_eq!(users(&renamed), ["alice", "copier", "mover"]);
    let combined = scan(5, &["trunk", "branches/renamed"]);
    assert_eq!(users(&combined), ["alice", "copier", "mover", "trunk-dev"]);
    assert_eq!(author(&combined, "alice")["commit_count"], 1);
    assert_eq!(
        users(&scan(6, &["branches/renamed"])),
        ["alice", "copier", "late", "mover"]
    );
    assert_eq!(users(&scan(5, &["trunk"])), ["alice", "trunk-dev"]);
    assert!(request(root, json!({"op":"authors"}))
        .as_array()
        .unwrap()
        .is_empty());

    save_alice(root, "shared@example.org");
    let identities = scan(5, &["trunk"]);
    let alice = author(&identities, "alice");
    assert_eq!(alice["git_name"], "Alice");
    assert_eq!(alice["git_email"], "shared@example.org");
    assert_eq!(alice["in_library"], true);
    let unmapped = author(&identities, "trunk-dev");
    assert_eq!(unmapped["git_email"], "trunk-dev@svn.local");
    assert_eq!(unmapped["in_library"], false);
    request(root, json!({"op":"delete_author","svn_user":"alice"}));
    let after_delete = scan(5, &["trunk"]);
    let alice = author(&after_delete, "alice");
    assert_eq!(alice["git_name"], "alice");
    assert_eq!(alice["git_email"], "alice@svn.local");
    assert_eq!(alice["in_library"], false);
}

#[test]
fn history_can_open_a_task_created_with_the_original_cli_config() {
    use svn_history_to_git_core::{config::TaskConfig, storage::Storage};
    let temp = tempfile::tempdir().unwrap();
    let config = TaskConfig::from_json(
        &json!({
            "svn_url":"https://svn.example.org/archive","name":"旧版任务",
            "workdir":temp.path().join("original")
        })
        .to_string(),
    )
    .unwrap();
    let original = Storage::open(temp.path().join("state"))
        .unwrap()
        .create_task(&config)
        .unwrap();
    let id = format!("legacy:{}", original.id);
    assert_eq!(request(temp.path(), json!({"op":"history"}))[0]["id"], id);
    let task = request(temp.path(), json!({"op":"get","id":id}));
    assert_eq!(task["legacy"]["config"]["svn_url"], config.svn_url);
    assert_eq!(task["legacy"]["name"], "旧版任务");
}

#[test]
fn desktop_migrates_a_pinned_project_and_recovers_a_rejected_push() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    save_alice(root, "shared@example.org");
    request(
        root,
        json!({"op":"save_authors","authors":[
            {"svn_user":"release-bot","git_name":"Release Bot","git_email":"release@example.org"}
        ]}),
    );
    let base = svn_repository(root, "source");
    let svn =
        |message: &str, operations: &[&str]| svn_commit(root, &base, "alice", message, operations);
    fs::write(root.join("file"), "first\n").unwrap();
    svn(
        "first",
        &[
            "mkdir",
            "ERP",
            "mkdir",
            "ERP/trunk",
            "mkdir",
            "ERP/branches",
            "mkdir",
            "ERP/tags",
            "put",
            "file",
            "ERP/trunk/readme.txt",
        ],
    );
    svn(
        "release",
        &[
            "cp",
            "1",
            "ERP/trunk",
            "ERP/branches/release",
            "cp",
            "1",
            "ERP/trunk",
            "ERP/tags/v1",
        ],
    );
    fs::write(root.join("file"), "pinned\n").unwrap();
    svn("second", &["put", "file", "ERP/trunk/readme.txt"]);
    let dirs = request(root, json!({"op":"browse","url":base}));
    assert_eq!(dirs["dirs"], json!(["ERP"]));
    let project = request(
        root,
        json!({"op":"inspect","url":format!("{base}/ERP"),"trunk":"trunk","branches":"branches","tags":"tags"}),
    );
    assert_eq!(project["branches"], json!(["branches/release"]));
    assert_eq!(project["tags"], json!(["tags/v1"]));
    assert_eq!(project["revision"], 3);
    fs::write(root.join("file"), "after confirmation").unwrap();
    svn("too late", &["put", "file", "ERP/trunk/readme.txt"]);

    invoke(
        root,
        "git",
        &["init", "--bare", "--initial-branch=default", "remote.git"],
    );
    let remote = root.join("remote.git");
    assert_eq!(
        request(root, json!({"op":"remote","remote":remote}))["empty"],
        true
    );
    let manifest = json!({"svn_url":project["url"],"revision":3,"uuid":project["uuid"],"remote":remote,
        "refs":[{"path":"trunk","kind":"trunk","name":"production"},{"path":"tags/v1","kind":"tag","name":"ship"}],
        "authors":[{"svn_user":"alice","git_name":"Alice","git_email":"one-off@example.org"}]});
    let hook = remote.join("hooks/pre-receive");
    fs::write(&hook, b"#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let failed = events(root, json!({"op":"run","manifest":manifest}));
    assert_eq!(failed.last().unwrap()["type"], "error", "{failed:?}");
    let id = &failed.iter().find(|e| e["stage"] == "created").unwrap()["message"];
    let saved = request(root, json!({"op":"get","id":id}));
    assert_eq!(saved["author_overrides"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        author(&saved["author_overrides"], "alice")["git_email"],
        "one-off@example.org"
    );
    assert_eq!(saved["verified"].as_array().unwrap().len(), 2, "{saved}");
    assert_eq!(saved["stage"], "failed");
    assert_eq!(saved["publication_started"], true);
    fs::remove_file(hook).unwrap();
    let job = request(root, json!({"op":"retry","id":id}));
    assert_eq!(job["stage"], "completed");
    assert_eq!(job["workdir"], saved["workdir"]);
    assert_eq!(job["commit_count"], 2);
    assert_eq!(job["verified"], saved["verified"]);
    invoke(
        root,
        "git",
        &[
            "clone",
            "--config",
            "core.autocrlf=false",
            "-q",
            "--branch",
            "production",
            "--",
            remote.to_str().unwrap(),
            "checkout",
        ],
    );
    let checkout = root.join("checkout");
    assert_eq!(
        fs::read_to_string(checkout.join("readme.txt")).unwrap(),
        "pinned\n"
    );
    assert_eq!(
        invoke(&checkout, "git", &["log", "--format=%s | %an <%ae>"]),
        "second | Alice <one-off@example.org>\nfirst | Alice <one-off@example.org>"
    );
    assert_eq!(
        invoke(&checkout, "git", &["show", "ship:readme.txt"]),
        "first"
    );
    assert_eq!(
        invoke(&remote, "git", &["for-each-ref", "--format=%(refname)"]),
        "refs/heads/production\nrefs/tags/ship"
    );
    let history = request(root, json!({"op":"history"}));
    let recent = &history[0];
    assert_eq!(recent["id"], *id);
    assert_eq!(recent["manifest"]["remote"], manifest["remote"]);
    assert_eq!(recent["manifest"]["refs"], manifest["refs"]);
    assert_eq!(recent["manifest"]["revision"], 3);
    assert_eq!(recent["commit_count"], 2);
    assert!(recent["updated_at"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(recent["manifest"].get("authors").is_none());
    assert_eq!(
        author(&request(root, json!({"op":"authors"})), "alice")["git_email"],
        "shared@example.org"
    );
    assert_eq!(
        events(root, json!({"op":"remote","remote":remote}))
            .last()
            .unwrap()["type"],
        "error"
    );

    // Import a second SVN history as a new branch in the same populated repository.
    let original = invoke(&remote, "git", &["rev-parse", "refs/heads/production"]);
    invoke(
        &remote,
        "git",
        &["update-ref", "refs/heads/main", &original],
    );
    let before = invoke(&remote, "git", &["show-ref"]);
    let head = invoke(&remote, "git", &["symbolic-ref", "HEAD"]);
    let check = |name: &str| json!({"op":"remote","remote":remote,"target_branch":name});
    let available = request(root, check("V0.X"));
    assert_eq!(
        available["existing_refs"],
        json!(["refs/heads/main", "refs/heads/production", "refs/tags/ship"])
    );
    for name in ["main", "main/child"] {
        let failure = events(root, check(name)).pop().unwrap();
        assert_eq!(failure["type"], "error", "{failure}");
        assert!(failure["message"]
            .as_str()
            .unwrap()
            .contains("refs/heads/main"));
    }
    assert_eq!(invoke(&remote, "git", &["show-ref"]), before);
    let second_source = svn_repository(root, "second-source");
    fs::write(root.join("file"), "second repository\n").unwrap();
    svn_commit(
        root,
        &second_source,
        "alice",
        "first release files",
        &[
            "mkdir",
            "trunk",
            "mkdir",
            "branches",
            "put",
            "file",
            "trunk/readme.txt",
        ],
    );
    svn_commit(
        root,
        &second_source,
        "alice",
        "copy release",
        &["cp", "1", "trunk", "branches/release"],
    );
    let release = request(
        root,
        json!({"op":"browse","url":format!("{second_source}/branches/release")}),
    );
    let second = json!({"svn_url":release["url"],"revision":release["revision"],
        "uuid":release["uuid"],"remote":remote,
        "refs":[{"path":"","kind":"trunk","name":"V0.X"}]});

    // A competing branch appears after preflight, so the job is saved before conversion.
    invoke(
        &remote,
        "git",
        &["update-ref", "refs/heads/V0.X", &original],
    );
    let failed = events(root, json!({"op":"run","manifest":second}));
    assert_eq!(failed.last().unwrap()["type"], "error", "{failed:?}");
    let second_id = &failed.iter().find(|e| e["stage"] == "created").unwrap()["message"];
    let saved = request(root, json!({"op":"get","id":second_id}));
    assert_eq!(saved["verified"], json!([]));
    assert_eq!(saved["author_overrides"], json!([]));
    assert_eq!(
        author(&saved["manifest"]["authors"], "alice")["git_email"],
        "shared@example.org"
    );

    save_alice(root, "edited@example.org");
    invoke(
        &remote,
        "git",
        &["update-ref", "-d", "refs/heads/V0.X", &original],
    );
    let second_job = request(root, json!({"op":"retry","id":second_id}));
    assert_eq!(second_job["stage"], "completed");
    assert_eq!(
        invoke(&remote, "git", &["show", "refs/heads/V0.X:readme.txt"]),
        "second repository"
    );
    assert_eq!(
        invoke(&remote, "git", &["log", "--format=%s | %an <%ae>", "refs/heads/V0.X"]),
        "copy release | Alice <shared@example.org>\nfirst release files | Alice <shared@example.org>"
    );
    assert_eq!(
        author(&request(root, json!({"op":"authors"})), "alice")["git_email"],
        "edited@example.org"
    );
    let after = invoke(&remote, "git", &["show-ref"]);
    assert_eq!(
        after
            .lines()
            .filter(|r| !r.ends_with(" refs/heads/V0.X"))
            .collect::<Vec<_>>()
            .join("\n"),
        before
    );
    assert_eq!(invoke(&remote, "git", &["symbolic-ref", "HEAD"]), head);
}
