use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use svn2git_core::guided::{jobs, publish, source, Manifest};
use svn2git_core::storage::{AuthorEntry, Storage};

mod authors;
mod credentials;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Browse {
        url: String,
        svn_credential_id: Option<String>,
    },
    Inspect {
        url: String,
        trunk: String,
        branches: String,
        tags: String,
        #[serde(default)]
        auto_branch_subdir: bool,
        svn_credential_id: Option<String>,
    },
    Remote {
        remote: String,
        target_branch: Option<String>,
        git_credential_id: Option<String>,
    },
    Credential {
        #[serde(flatten)]
        profile: credentials::SaveCredential,
    },
    DeleteCredential {
        id: String,
    },
    Authors,
    SaveAuthors {
        authors: Vec<AuthorEntry>,
    },
    DeleteAuthor {
        svn_user: String,
    },
    ScanAuthors {
        url: String,
        revision: u64,
        paths: Vec<String>,
        svn_credential_id: Option<String>,
    },
    Credentials,
    Doctor,
    History,
    Get {
        id: String,
    },
    Run {
        manifest: Manifest,
        workdir: Option<PathBuf>,
    },
    Retry {
        id: String,
        svn_credential_id: Option<String>,
        git_credential_id: Option<String>,
    },
}

fn emit(value: Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    out.flush()
}

pub async fn serve(dir: &Path) -> Result<()> {
    let result = async {
        let request: Request =
            serde_json::from_reader(io::stdin().lock()).context("无效的桌面请求")?;
        dispatch(&Storage::open(dir)?, request).await
    }
    .await;
    match result {
        Ok(data) => emit(json!({"type":"result","data":data}))?,
        Err(err) => {
            emit(json!({"type":"error","message":format!("{err:#}")}))?;
            std::process::exit(1);
        }
    }
    Ok(())
}

async fn dispatch(storage: &Storage, request: Request) -> Result<Value> {
    let progress = |stage: &str, message: &str| {
        let _ = emit(json!({"type":"progress","stage":stage,"message":message}));
    };
    match request {
        Request::Browse {
            url,
            svn_credential_id,
        } => {
            let secret = jobs::credential(storage, svn_credential_id.as_deref(), true)?;
            Ok(json!(source::browse(&url, secret.as_ref()).await?))
        }
        Request::Inspect {
            url,
            trunk,
            branches,
            tags,
            auto_branch_subdir,
            svn_credential_id,
        } => {
            let secret = jobs::credential(storage, svn_credential_id.as_deref(), true)?;
            Ok(json!(
                source::inspect_layout(
                    &url,
                    &trunk,
                    &branches,
                    &tags,
                    auto_branch_subdir,
                    secret.as_ref()
                )
                .await?
            ))
        }
        Request::Remote {
            remote,
            target_branch,
            git_credential_id,
        } => {
            let (env, _askpass) = jobs::git_auth(storage, git_credential_id.as_deref()).await?;
            if let Some(branch) = target_branch {
                let refs = publish::check_targets(&remote, &[format!("refs/heads/{branch}")], &env)
                    .await?;
                return Ok(json!({"empty":refs.is_empty(),"existing_refs":refs}));
            }
            publish::check_empty(&remote, &env).await?;
            Ok(json!({"empty":true}))
        }
        Request::Credential { profile } => credentials::save(storage, profile),
        Request::DeleteCredential { id } => credentials::remove(storage, &id),
        Request::Authors => Ok(json!(storage.list_authors()?)),
        Request::SaveAuthors { authors } => authors::save(storage, &authors),
        Request::DeleteAuthor { svn_user } => authors::remove(storage, &svn_user),
        Request::ScanAuthors {
            url,
            revision,
            paths,
            svn_credential_id,
        } => {
            authors::scan(
                storage,
                &url,
                revision,
                &paths,
                svn_credential_id.as_deref(),
            )
            .await
        }
        Request::Credentials => Ok(json!(storage.list_credentials()?)),
        Request::Doctor => Ok(json!(
            svn2git_core::doctor::run_doctor(&svn2git_core::process::ProcessRegistry::new()).await
        )),
        Request::History => {
            let mut rows: Vec<_> = storage.guided_history_with_times()?.into_iter().map(|(job, updated_at)| json!({
                "id":job.id, "title": job.manifest.svn_url, "stage":job.stage, "message":job.message,
                "web_url":job.manifest.web_url, "updated_at":updated_at, "commit_count":job.commit_count,
                "manifest":{"svn_url":job.manifest.svn_url,"remote":job.manifest.remote,
                    "revision":job.manifest.revision,"refs":job.manifest.refs}
            })).collect();
            rows.extend(storage.list_tasks()?.iter().map(|task| json!({
                "id":format!("legacy:{}", task.id),"title":task.name,"stage":task.status.as_str(),
                "updated_at":task.updated_at,
                "message": task.error.as_deref().unwrap_or(&task.stage)
            })));
            rows.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
            Ok(json!(rows))
        }
        Request::Get { id } => {
            if let Some(number) = id.strip_prefix("legacy:") {
                let task = storage
                    .get_task(number.parse()?)?
                    .context("找不到旧版任务")?;
                return Ok(
                    json!({"id":id,"stage":task.status.as_str(),"message":task.stage,"legacy":task}),
                );
            }
            let mut data = json!(storage.guided_task(&id)?);
            data["logs"] = json!(storage.guided_events(&id)?);
            Ok(data)
        }
        Request::Run { manifest, workdir } => {
            let job = jobs::create(
                storage,
                manifest,
                workdir.as_deref().filter(|p| !p.as_os_str().is_empty()),
            )?;
            progress("created", &job.id);
            Ok(json!(
                jobs::execute(storage, &job.id, None, None, &progress).await?
            ))
        }
        Request::Retry {
            id,
            svn_credential_id,
            git_credential_id,
        } => Ok(json!(
            jobs::execute(
                storage,
                &id,
                svn_credential_id,
                git_credential_id,
                &progress
            )
            .await?
        )),
    }
}
