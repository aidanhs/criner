use crate::{
    model,
    persistence::{self, TableAccess},
    Error, Result,
};
use bytesize::ByteSize;
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::io::AsyncWriteExt;

use crate::utils::timeout_after;
use async_trait::async_trait;
use futures::FutureExt;
use std::time::Duration;

struct ProcessingState {
    url: String,
    kind: &'static str,
    out_file: PathBuf,
    key: String,
}
pub struct Agent {
    asset_dir: PathBuf,
    client: reqwest::Client,
    results: persistence::TaskResultTable,
    channel: async_std::sync::Sender<super::cpubound::ExtractRequest>,
    state: Option<ProcessingState>,
    extraction_request: Option<super::cpubound::ExtractRequest>,
}

impl Agent {
    pub fn new(
        assets_dir: impl Into<PathBuf>,
        db: &persistence::Db,
        channel: async_std::sync::Sender<super::cpubound::ExtractRequest>,
    ) -> Result<Agent> {
        let client = reqwest::ClientBuilder::new().gzip(true).build()?;

        let results = db.open_results()?;
        Ok(Agent {
            asset_dir: assets_dir.into(),
            client,
            results,
            channel,
            state: None,
            extraction_request: None,
        })
    }
}

#[async_trait]
impl crate::engine::work::generic::Processor for Agent {
    type Item = DownloadRequest;

    fn set(
        &mut self,
        request: Self::Item,
        out_key: &mut String,
        progress: &mut prodash::tree::Item,
    ) -> Result<(model::Task, String)> {
        progress.init(None, None);
        match request {
            DownloadRequest {
                crate_name,
                crate_version,
                kind,
                url,
            } => {
                let dummy_task = default_persisted_download_task();
                let progress_message = format!("↓ {}:{}", crate_name, crate_version);

                dummy_task.fq_key(&crate_name, &crate_version, out_key);
                let task_result = model::TaskResult::Download {
                    kind: kind.to_owned(),
                    url: String::new(),
                    content_length: 0,
                    content_type: None,
                };
                let mut key = String::with_capacity(out_key.len() * 2);
                task_result.fq_key(&crate_name, &crate_version, &dummy_task, &mut key);
                let base_dir = crate_dir(&self.asset_dir, &crate_name);
                let out_file = download_file_path(
                    &base_dir,
                    &crate_version,
                    &dummy_task.process,
                    &dummy_task.version,
                    kind,
                );
                self.state = Some(ProcessingState {
                    url,
                    kind,
                    out_file,
                    key,
                });
                self.extraction_request = Some(super::cpubound::ExtractRequest {
                    download_task: dummy_task.clone(),
                    crate_name,
                    crate_version,
                });
                Ok((dummy_task, progress_message))
            }
        }
    }

    fn idle_message(&self) -> String {
        "↓ IDLE".into()
    }

    async fn process(
        &mut self,
        progress: &mut prodash::tree::Item,
    ) -> std::result::Result<(), (Error, String)> {
        let ProcessingState {
            url,
            kind,
            out_file,
            key,
        } = self.state.take().expect("initialized state");
        download_file_and_store_result(
            progress,
            &key,
            &self.results,
            &self.client,
            kind,
            &url,
            out_file,
        )
        .await
        .map_err(|err| (err, format!("Failed to download '{}'", url)))
    }

    async fn schedule_next(&mut self, progress: &mut prodash::tree::Item) -> Result<()> {
        let extract_request = self
            .extraction_request
            .take()
            .expect("this to be set when we are called");
        progress.blocked("schedule crate extraction", None);
        // Here we risk doing this work twice, but must of the time, we don't. And since it's fast,
        // we take the risk of duplicate work for keeping more precessors busy.
        // And yes, this send is blocking the source processor, but should not be an issue as CPU
        // processors are so fast - slow producer, fast consumer.
        self.channel.send(extract_request).await;
        Ok(())
    }
}

#[derive(Clone)]
pub struct DownloadRequest {
    pub crate_name: String,
    pub crate_version: String,
    pub kind: &'static str,
    pub url: String,
}

pub fn default_persisted_download_task() -> model::Task {
    const TASK_NAME: &str = "download";
    const TASK_VERSION: &str = "1.0.0";
    model::Task {
        stored_at: SystemTime::now(),
        process: TASK_NAME.into(),
        version: TASK_VERSION.into(),
        state: Default::default(),
    }
}

async fn download_file_and_store_result(
    progress: &mut prodash::tree::Item,
    key: &str,
    results: &persistence::TaskResultTable,
    client: &reqwest::Client,
    kind: &str,
    url: &str,
    out_file: PathBuf,
) -> Result<()> {
    progress.blocked("fetch HEAD", None);
    let mut res = timeout_after(
        Duration::from_secs(30),
        "fetching HEAD",
        client.get(url).send(),
    )
    .await??;
    let size: u32 = res
        .content_length()
        .ok_or(Error::InvalidHeader("expected content-length"))? as u32;
    progress.init(Some(size / 1024), Some("Kb"));
    progress.done(format!(
        "HEAD:{}: content-length = {}",
        url,
        ByteSize(size.into())
    ));

    let mut bytes_received = 0usize;
    tokio::fs::create_dir_all(&out_file.parent().expect("parent directory")).await?;
    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(out_file)
        .await?;

    while let Some(chunk) = timeout_after(
        Duration::from_secs(15),
        format!(
            "fetching {} of {}",
            ByteSize(bytes_received as u64),
            ByteSize(size.into())
        ),
        res.chunk().boxed(),
    )
    .await??
    {
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

    let task_result = model::TaskResult::Download {
        kind: kind.to_owned(),
        url: url.to_owned(),
        content_length: size,
        content_type: res
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|t| t.to_str().ok())
            .map(Into::into),
    };
    results.insert(progress, &key, &task_result)?;
    Ok(())
}

pub fn download_file_path(
    base_dir: &Path,
    crate_version: &str,
    process: &str,
    version: &str,
    kind: &str,
) -> PathBuf {
    base_dir.join(format!(
        "{crate_version}-{process}{sep}{version}.{kind}",
        process = process,
        sep = crate::persistence::KEY_SEP_CHAR,
        version = version,
        kind = kind,
        crate_version = crate_version
    ))
}

pub fn crate_dir(assets_dir: &Path, crate_name: &str) -> PathBuf {
    // we can safely assume ascii here - otherwise we panic
    let crate_path = match crate_name.len() {
        1 => Path::new("1").join(crate_name),
        2 => Path::new("2").join(crate_name),
        3 => Path::new("3").join(&crate_name[..1]),
        _ => Path::new(&crate_name[..2])
            .join(&crate_name[2..4])
            .join(crate_name),
    };
    assets_dir.join(crate_path)
}
