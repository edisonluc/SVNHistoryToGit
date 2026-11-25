use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use svn_history_to_git_core::config::{EngineKind, LayoutMode, PushStrategy, TaskConfig};
use svn_history_to_git_core::credential::AskPassServer;
use svn_history_to_git_core::engine::fast_import::FastImportEngine;
use svn_history_to_git_core::engine::{EngineContext, MigrationEngine};
use svn_history_to_git_core::git_ops::{inspect_repo_stats_gix, sanitize_git_ref_name};
use svn_history_to_git_core::process::{decode_bytes, JobGuard, ProcessRegistry};
use svn_history_to_git_core::storage::{AuthorEntry, Storage};
use svn_history_to_git_core::svn::dump_parser::{DumpStreamParser, FileEntry, VersionedTreeIndex};
use svn_history_to_git_core::svn::inspector::{
    parse_authors_from_log_xml, parse_svn_info_xml, parse_svn_list_xml,
};

fn sample_config(workdir: &str) -> TaskConfig {
    TaskConfig {
        name: "demo-repo".into(),
        svn_url: "https://svn.example.com/repo".into(),
        svn_subpath: Some("services/auth".into()),
        svn_credential_id: None,
        svn_username: Some("alice".into()),
        trust_self_signed_cert: true,
        local_dump_file: None,
        git_remote_url: Some("https://git.example.com/team/auth.git".into()),
        git_credential_id: None,
        target_branch: Some("main".into()),
        subtree_prefix: None,
        push_remote: true,
        push_strategy: PushStrategy::TargetBranchSafeMerge,
        workdir: workdir.into(),
        authors_file: None,
        default_email_domain: Some("company.com".into()),
        start_revision: Some(1),
        end_revision: Some(100),
        layout_mode: LayoutMode::Standard,
        custom_layout: Default::default(),
        engine: EngineKind::FastImport,
        window_size: 250,
        turbo_prefetch: false,
        prefetch_concurrency: 2,
        convert_gitignore: true,
        scan_externals: true,
        repair_gbk_messages: true,
        verify_tree_consistency: true,
        optimize_git_repo: false,
        exclude_globs: vec!["*.bak".into(), "bin/**".into()],
        lfs_threshold_mb: Some(1),
    }
}

#[test]
fn test_config_validation_and_toml_json_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("repo").to_string_lossy().to_string();
    let cfg = sample_config(&workdir);
    assert!(cfg.validate().is_ok());
    assert_eq!(
        cfg.effective_svn_url(),
        "https://svn.example.com/repo/services/auth"
    );

    let toml_str = cfg.to_toml().unwrap();
    let parsed_toml = TaskConfig::from_toml(&toml_str).unwrap();
    assert_eq!(parsed_toml.name, "demo-repo");
    assert_eq!(parsed_toml.exclude_globs, vec!["*.bak", "bin/**"]);

    let json_str = cfg.to_json().unwrap();
    let parsed_json = TaskConfig::from_json(&json_str).unwrap();
    assert_eq!(parsed_json.window_size, 250);

    assert_eq!(sanitize_git_ref_name("release 1.0..beta"), "release_1.0_beta");
}

#[test]
fn test_gb18030_adaptive_decoding_and_job_guard() {
    let utf8 = "中文提交日志 OK".as_bytes();
    assert_eq!(decode_bytes(utf8), "中文提交日志 OK");

    let (gbk_bytes, _, _) = encoding_rs::GB18030.encode("拒绝访问：找不到路径");
    assert_eq!(decode_bytes(&gbk_bytes), "拒绝访问：找不到路径");

    let guard = JobGuard::new().unwrap();
    guard.terminate_all();
}

#[test]
fn test_svn_xml_parsers() {
    let info_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
    <info><entry revision="42">
      <url>https://svn.local/repo/trunk</url>
      <repository><root>https://svn.local/repo</root><uuid>1234-5678</uuid></repository>
      <commit revision="41"><author>zhangsan</author><date>2026-01-01T00:00:00Z</date></commit>
    </entry></info>"#;
    let info = parse_svn_info_xml(info_xml).unwrap();
    assert_eq!(info.head_revision, 42);
    assert_eq!(info.last_changed_author, "zhangsan");
    assert_eq!(info.uuid, "1234-5678");

    let list_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
    <lists><list path="https://svn.local/repo">
      <entry kind="dir"><name>trunk</name><commit revision="10"><author>alice</author><date>2026-01-01T00:00:00Z</date></commit></entry>
      <entry kind="file"><name>README.md</name><size>128</size><commit revision="9"><author>bob</author><date>2026-01-01T00:00:00Z</date></commit></entry>
    </list></lists>"#;
    let entries = parse_svn_list_xml(list_xml, "").unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, "dir");
    assert_eq!(entries[0].name, "trunk");

    let log_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
    <log>
      <logentry revision="1"><author>alice</author></logentry>
      <logentry revision="2"><author>bob</author></logentry>
      <logentry revision="3"><author>alice</author></logentry>
    </log>"#;
    let authors = parse_authors_from_log_xml(log_xml, Some("acme.org")).unwrap();
    assert_eq!(authors[0].svn_user, "alice");
    assert_eq!(authors[0].commit_count, 2);
    assert_eq!(authors[0].git_email, "alice@acme.org");
}

#[test]
fn test_versioned_tree_index_copyfrom() {
    let mut idx = VersionedTreeIndex::new();
    idx.put_file(
        "trunk/src/main.rs".into(),
        FileEntry {
            mark_id: 10,
            mode: "100644",
        },
    );
    idx.commit_snapshot(1);

    let copied = idx.copy_from(1, "trunk", "branches/dev");
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].0, "branches/dev/src/main.rs");
    assert_eq!(copied[0].1.mark_id, 10);
}

#[tokio::test]
async fn test_sqlite_wal_storage_and_zstd_log_archive() {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(dir.path()).unwrap();

    let repo_dir = dir.path().join("work-repo").to_string_lossy().to_string();
    let cfg = sample_config(&repo_dir);
    let task = storage.create_task(&cfg).unwrap();
    assert_eq!(task.id, 1);

    storage
        .upsert_authors(&[AuthorEntry {
            svn_user: "alice".into(),
            git_name: "Alice Wang".into(),
            git_email: "alice@company.com".into(),
            commit_count: 15,
            updated_at: String::new(),
        }])
        .unwrap();
    let authors = storage.list_authors().unwrap();
    assert_eq!(authors.len(), 1);
    assert_eq!(authors[0].git_name, "Alice Wang");

    for i in 0..20 {
        storage.logs.emit(task.id, "info", &format!("log line {i}"));
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let lines = storage.logs.read_logs(task.id, 0, 50, None, Some("line 1")).unwrap();
    assert!(!lines.is_empty());

    let archived = storage.logs.archive_task_log(task.id).unwrap();
    assert!(archived.is_some());
    let after_archive = storage.logs.read_logs(task.id, 0, 50, None, None).unwrap();
    assert_eq!(after_archive.len(), 20);
}

#[tokio::test]
async fn test_askpass_server_roundtrip() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    let server = AskPassServer::start("devuser".into(), "s3cr3t!".into())
        .await
        .unwrap();
    let env = server.build_env(None);
    let addr = env.get("SVN_HISTORY_TO_GIT_ASKPASS_ADDR").unwrap();
    let token = env.get("SVN_HISTORY_TO_GIT_ASKPASS_TOKEN").unwrap();

    for (prompt, expected) in [
        ("Username for 'https://git.local':", "devuser"),
        ("Password for 'https://devuser@git.local':", "s3cr3t!"),
        ("Enter passphrase for key 'C:/Users/devuser/.ssh/id_ed25519':", "s3cr3t!"),
    ] {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(format!("{token} {prompt}\n").as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), expected, "{prompt}");
    }
}

#[tokio::test]
async fn test_fast_import_engine_end_to_end_synthetic_dump() {
    // Verify git is available
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let dump_path = dir.path().join("synthetic.dump");
    let workdir = dir.path().join("migrated-git");

    let hello_content = b"fn main() { println!(\"Hello SVNHistoryToGit\"); }\n";
    let ignored_bak = b"temporary backup";
    let large_blob = vec![b'X'; 128 * 1024]; // 128 KB

    let mut dump = Vec::new();
    let push_str = |buf: &mut Vec<u8>, s: &str| buf.extend_from_slice(s.as_bytes());

    push_str(&mut dump, "SVN-fs-dump-format-version: 2\n\n");
    push_str(&mut dump, "Revision-number: 0\nProp-content-length: 10\nContent-length: 10\n\nPROPS-END\n\n");

    // Revision 1: add trunk/src/main.rs, trunk/temp.bak (excluded), trunk/big.bin (LFS)
    let r1_props = "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n2026-01-15T10:00:00.000000Z\nK 7\nsvn:log\nV 21\nInitial commit 中文\nPROPS-END\n";
    push_str(
        &mut dump,
        &format!(
            "Revision-number: 1\nProp-content-length: {}\nContent-length: {}\n\n{r1_props}\n",
            r1_props.len(),
            r1_props.len()
        ),
    );
    push_str(&mut dump, "Node-path: trunk\nNode-kind: dir\nNode-action: add\nProp-content-length: 39\nContent-length: 39\n\nK 10\nsvn:ignore\nV 7\ntarget/\nPROPS-END\n\n");
    push_str(
        &mut dump,
        &format!(
            "Node-path: trunk/src/main.rs\nNode-kind: file\nNode-action: add\nText-content-length: {}\nContent-length: {}\n\n",
            hello_content.len(),
            hello_content.len()
        ),
    );
    dump.extend_from_slice(hello_content);
    push_str(&mut dump, "\n\n");
    push_str(
        &mut dump,
        &format!(
            "Node-path: trunk/temp.bak\nNode-kind: file\nNode-action: add\nText-content-length: {}\nContent-length: {}\n\n",
            ignored_bak.len(),
            ignored_bak.len()
        ),
    );
    dump.extend_from_slice(ignored_bak);
    push_str(&mut dump, "\n\n");
    push_str(
        &mut dump,
        &format!(
            "Node-path: trunk/big.bin\nNode-kind: file\nNode-action: add\nText-content-length: {}\nContent-length: {}\n\n",
            large_blob.len(),
            large_blob.len()
        ),
    );
    dump.extend_from_slice(&large_blob);
    push_str(&mut dump, "\n\n");

    // Revision 2: copy trunk -> branches/release-1 and trunk -> tags/v1.0
    let r2_props = "K 10\nsvn:author\nV 3\nbob\nK 8\nsvn:date\nV 27\n2026-01-16T12:00:00.000000Z\nK 7\nsvn:log\nV 12\nRelease v1.0\nPROPS-END\n";
    push_str(
        &mut dump,
        &format!(
            "Revision-number: 2\nProp-content-length: {}\nContent-length: {}\n\n{r2_props}\n",
            r2_props.len(),
            r2_props.len()
        ),
    );
    push_str(&mut dump, "Node-path: branches/release-1\nNode-kind: dir\nNode-action: add\nNode-copyfrom-rev: 1\nNode-copyfrom-path: trunk\n\n");
    push_str(&mut dump, "Node-path: tags/v1.0\nNode-kind: dir\nNode-action: add\nNode-copyfrom-rev: 1\nNode-copyfrom-path: trunk\n\n");

    fs::write(&dump_path, &dump).unwrap();

    // Quick parser sanity check
    let mut parser = DumpStreamParser::new(Cursor::new(&dump));
    assert_eq!(parser.next_revision().unwrap().unwrap().revision, 0);
    assert_eq!(parser.next_revision().unwrap().unwrap().revision, 1);
    assert_eq!(parser.next_revision().unwrap().unwrap().revision, 2);

    let mut cfg = sample_config(&workdir.to_string_lossy());
    cfg.local_dump_file = Some(dump_path.to_string_lossy().to_string());
    cfg.svn_subpath = None;
    cfg.push_remote = false;
    cfg.git_remote_url = None;
    cfg.lfs_threshold_mb = None; // test normal blob first; exclude *.bak

    let mut authors = HashMap::new();
    authors.insert("alice".into(), ("Alice".into(), "alice@acme.com".into()));
    authors.insert("bob".into(), ("Bob".into(), "bob@acme.com".into()));

    let ctx = EngineContext {
        task_id: 1,
        config: cfg,
        workdir: workdir.clone(),
        authors_map: authors,
        authors_file: PathBuf::new(),
        svn_secret: None,
        svn_auth_env: HashMap::new(),
        svn_config_dir: None,
        start_rev: 1,
        end_rev: 2,
        registry: ProcessRegistry::new(),
        on_line: None,
        on_progress: None,
        on_checkpoint: None,
    };

    let outcome = FastImportEngine.run(&ctx).await.unwrap();
    assert_eq!(outcome.last_synced_rev, 2);
    assert_eq!(outcome.excluded_files_count, 1);
    assert!(outcome.gitignore_generated);

    let stats = inspect_repo_stats_gix(&workdir).unwrap();
    assert!(stats.branches.contains(&"main".to_string()));
    assert!(stats.branches.contains(&"release-1".to_string()));
    assert!(stats.tags.contains(&"v1.0".to_string()));
    assert!(workdir.join("src/main.rs").exists());
    assert!(!workdir.join("temp.bak").exists());
    assert!(workdir.join(".gitignore").exists());
}

#[tokio::test]
async fn test_fast_import_lfs_large_file_conversion() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let dump_path = dir.path().join("lfs.dump");
    let workdir = dir.path().join("lfs-git");

    let big_payload = vec![0xABu8; 1024 * 1024 + 128]; // > 1 MB
    let r1_props = "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n2026-02-01T10:00:00.000000Z\nK 7\nsvn:log\nV 13\nAdd big asset\nPROPS-END\n";

    let mut dump = Vec::new();
    dump.extend_from_slice(b"SVN-fs-dump-format-version: 2\n\n");
    dump.extend_from_slice(
        format!(
            "Revision-number: 1\nProp-content-length: {}\nContent-length: {}\n\n{r1_props}\n",
            r1_props.len(),
            r1_props.len()
        )
        .as_bytes(),
    );
    dump.extend_from_slice(
        format!(
            "Node-path: assets/model.bin\nNode-kind: file\nNode-action: add\nText-content-length: {}\nContent-length: {}\n\n",
            big_payload.len(),
            big_payload.len()
        )
        .as_bytes(),
    );
    dump.extend_from_slice(&big_payload);
    dump.extend_from_slice(b"\n\n");
    fs::write(&dump_path, &dump).unwrap();

    let mut cfg = sample_config(&workdir.to_string_lossy());
    cfg.local_dump_file = Some(dump_path.to_string_lossy().to_string());
    cfg.layout_mode = LayoutMode::Root;
    cfg.svn_subpath = None;
    cfg.push_remote = false;
    cfg.git_remote_url = None;
    cfg.lfs_threshold_mb = Some(1);

    let ctx = EngineContext {
        task_id: 2,
        config: cfg,
        workdir: workdir.clone(),
        authors_map: HashMap::new(),
        authors_file: PathBuf::new(),
        svn_secret: None,
        svn_auth_env: HashMap::new(),
        svn_config_dir: None,
        start_rev: 1,
        end_rev: 1,
        registry: ProcessRegistry::new(),
        on_line: None,
        on_progress: None,
        on_checkpoint: None,
    };

    let outcome = FastImportEngine.run(&ctx).await.unwrap();
    assert_eq!(outcome.lfs_converted.len(), 1);
    assert_eq!(outcome.lfs_converted[0].path, "assets/model.bin");

    // Verify raw Git blob stored in ODB is a valid Git LFS pointer (even if git-lfs smudge hydrates working tree)
    let repo = gix::open(&workdir).unwrap();
    let head = repo.head_commit().unwrap();
    let tree = head.tree().unwrap();
    let entry = tree.lookup_entry_by_path("assets/model.bin").unwrap().unwrap();
    let blob = repo.find_object(entry.oid()).unwrap();
    let pointer_text = std::str::from_utf8(&blob.data).unwrap();
    assert!(pointer_text.starts_with("version https://git-lfs.github.com/spec/v1\n"));
    let oid = &outcome.lfs_converted[0].oid;
    assert!(workdir
        .join(".git/lfs/objects")
        .join(&oid[0..2])
        .join(&oid[2..4])
        .join(oid)
        .exists());
    let attrs = fs::read_to_string(workdir.join(".gitattributes")).unwrap();
    assert!(attrs.contains("*.bin filter=lfs"));
}
