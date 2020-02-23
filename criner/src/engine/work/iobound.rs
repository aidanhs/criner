use crate::{
    error::{Error, Result},
    model, persistence,
    persistence::TreeAccess,
};
use std::{path::PathBuf, time::SystemTime};
use tokio::io::AsyncWriteExt;

pub struct DownloadRequest {
    pub name: String,
    pub semver: String,
    pub kind: &'static str,
    pub url: String,
}

pub fn default_persisted_download_task() -> model::Task<'static> {
    const TASK_NAME: &str = "download";
    const TASK_VERSION: &str = "1.0.0";
    model::Task {
        stored_at: SystemTime::now(),
        process: TASK_NAME.into(),
        version: TASK_VERSION.into(),
        state: Default::default(),
    }
}

pub async fn processor(
    db: persistence::Db,
    mut progress: prodash::tree::Item,
    r: async_std::sync::Receiver<DownloadRequest>,
    assets_dir: PathBuf,
) -> Result<()> {
    let mut dummy = default_persisted_download_task();
    let mut key = Vec::with_capacity(32);
    let tasks = db.tasks();

    while let Some(DownloadRequest {
        name,
        semver,
        kind,
        url,
    }) = r.recv().await
    {
        progress.set_name(format!("↓ {}:{}", name, semver));
        progress.init(None, None);
        let mut kt = (name.as_str(), semver.as_str(), dummy);
        key.clear();

        persistence::TasksTree::key_to_buf(&kt, &mut key);
        dummy = kt.2;

        let mut task = tasks.update(&key, |_| ())?;
        task.process = dummy.process.clone();
        task.version = dummy.version.clone();

        progress.blocked(None);
        let res: Result<()> = async {
            {
                let mut res = reqwest::get(&url).await?;
                let size: u32 = res
                    .content_length()
                    .ok_or(Error::InvalidHeader("expected content-length"))?
                    as u32;
                progress.init(Some(size / 1024), Some("Kb"));
                progress.blocked(None);
                progress.done(format!("HEAD:{}: content-size = {}", url, size));
                let mut bytes_received = 0;
                let base_dir = assets_dir.join(&name).join(&semver);
                tokio::fs::create_dir_all(&base_dir).await?;
                let out_file = base_dir.join(format!(
                    "{process}{sep}{version}.{kind}",
                    process = dummy.process,
                    sep = crate::persistence::KEY_SEP,
                    version = dummy.version,
                    kind = kind
                ));
                let mut out = tokio::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(out_file)
                    .await?;
                while let Some(chunk) = res.chunk().await? {
                    out.write(&chunk).await?;
                    // body_buf.extend(chunk);
                    bytes_received += chunk.len();
                    progress.set((bytes_received / 1024) as u32);
                }
                progress.done(format!("GET:{}: body-size = {}", url, bytes_received));

                {
                    key.clear();
                    let insert_item = (
                        name.as_str(),
                        semver.as_str(),
                        &task,
                        model::TaskResult::Download {
                            kind: kind.into(),
                            url: url.as_str().into(),
                            content_length: size,
                            content_type: res
                                .headers()
                                .get(http::header::CONTENT_TYPE)
                                .and_then(|t| t.to_str().ok())
                                .map(Into::into),
                        },
                    );
                    persistence::TaskResultTree::key_to_buf(&insert_item, &mut key);
                }
                Ok(())
            }
        }
        .await;

        task.state = match res {
            Ok(_) => model::TaskState::Complete,
            Err(err) => {
                progress.fail(format!("Failed to download '{}': {}", url, err));
                model::TaskState::AttemptsWithFailure(vec![err.to_string()])
            }
        };
        kt.2 = task;
        tasks.upsert(&kt)?;
        progress.set_name("↓ idle");
        progress.init(None, None);
    }
    progress.done("Shutting down…");
    Ok(())
}
