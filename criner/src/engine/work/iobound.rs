use crate::{
    error::{Error, Result},
    model, persistence,
    persistence::TreeAccess,
};
use bytesize::ByteSize;
use std::{path::Path, path::PathBuf, time::SystemTime};
use tokio::io::AsyncWriteExt;

pub struct DownloadRequest {
    pub crate_name: String,
    pub crate_version: String,
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
    let results = db.results();
    let client = reqwest::ClientBuilder::new()
        .connect_timeout(std::time::Duration::from_secs(120))
        .gzip(true)
        .build()?;

    while let Some(DownloadRequest {
        crate_name,
        crate_version,
        kind,
        url,
    }) = r.recv().await
    {
        progress.set_name(format!("↓ {}:{}", crate_name, crate_version));
        progress.init(None, None);
        let mut kt = (crate_name.as_str(), crate_version.as_str(), dummy);
        key.clear();

        persistence::TasksTree::key_to_buf(&kt, &mut key);
        dummy = kt.2;

        let mut task = tasks.update(&key, |t| {
            t.process = dummy.process.clone();
            t.version = dummy.version.clone();
            t.state = t.state.merged(&model::TaskState::InProgress(None));
        })?;

        progress.blocked(None);
        let res: Result<()> = async {
            {
                let mut res = client.get(&url).send().await?;
                let size: u32 = res
                    .content_length()
                    .ok_or(Error::InvalidHeader("expected content-length"))?
                    as u32;
                progress.init(Some(size / 1024), Some("Kb"));
                progress.blocked(None);
                progress.done(format!(
                    "HEAD:{}: content-length = {}",
                    url,
                    ByteSize(size.into())
                ));
                let mut bytes_received = 0;
                let base_dir = crate_version_dir(&assets_dir, &crate_name, &crate_version);
                tokio::fs::create_dir_all(&base_dir).await?;
                let out_file = download_file_path(
                    dummy.process.as_ref(),
                    dummy.version.as_ref(),
                    kind,
                    &base_dir,
                );
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
                progress.done(format!(
                    "GET:{}: body-size = {}",
                    url,
                    ByteSize(bytes_received as u64)
                ));

                {
                    let insert_item = (
                        crate_name.as_str(),
                        crate_version.as_str(),
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
                    results.insert(&insert_item)?;
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
        progress.set_name("↓ IDLE");
        progress.init(None, None);
    }
    Ok(())
}

pub fn download_file_path(process: &str, version: &str, kind: &str, base_dir: &Path) -> PathBuf {
    base_dir.join(format!(
        "{process}{sep}{version}.{kind}",
        process = process,
        sep = crate::persistence::KEY_SEP_CHAR,
        version = version,
        kind = kind
    ))
}

pub fn crate_version_dir(assets_dir: &Path, crate_name: &str, crate_version: &str) -> PathBuf {
    assets_dir.join(crate_name).join(crate_version)
}
