use crate::error::{Error, FormatDeadline, Result};
use dia_semver::Semver;
use futures::task::SpawnExt;
use futures::{
    future::{self, Either},
    task::Spawn,
};
use futures_timer::Delay;
use std::{
    convert::TryFrom,
    future::Future,
    time::{Duration, SystemTime},
};

pub fn parse_semver(version: &str) -> Semver {
    use std::str::FromStr;
    Semver::from_str(&version)
        .or_else(|_| {
            Semver::from_str(
                &version[..version
                    .find('-')
                    .or_else(|| version.find('+'))
                    .expect("some prerelease version")],
            )
        })
        .expect("semver parsing to work if violating prerelease versions are stripped")
}

pub async fn wait_with_progress(
    duration_s: u32,
    mut progress: prodash::tree::Item,
    deadline: Option<SystemTime>,
    time: Option<time::Time>,
) -> Result<()> {
    progress.init(Some(duration_s), Some("s"));
    if let Some(time) = time {
        progress.set_name(format!(
            "{} scheduled at {}",
            progress.name().unwrap_or_else(|| "un-named".into()),
            time.format("%R")
        ));
    }
    for s in 1..=duration_s {
        Delay::new(Duration::from_secs(1)).await;
        check(deadline)?;
        progress.set(s);
    }
    Ok(())
}

fn duration_until(time: Option<time::Time>) -> Duration {
    time.map(|t| {
        let now = time::OffsetDateTime::now_local();
        let desired = now.date().with_time(t).assume_offset(now.offset());
        if desired > now {
            desired - now
        } else {
            desired
                .date()
                .next_day()
                .with_time(t)
                .assume_offset(now.offset())
                - now
        }
    })
    .and_then(|d| Duration::try_from(d).ok())
    .unwrap_or_default()
}

pub async fn repeat_daily_at<MakeFut, MakeProgress, Fut, T>(
    time: Option<time::Time>,
    mut make_progress: MakeProgress,
    deadline: Option<SystemTime>,
    mut make_future: MakeFut,
) -> Result<()>
where
    Fut: Future<Output = Result<T>>,
    MakeFut: FnMut() -> Fut,
    MakeProgress: FnMut() -> prodash::tree::Item,
{
    let mut iteration = 0;
    loop {
        iteration += 1;
        wait_with_progress(
            duration_until(time).as_secs() as u32,
            make_progress(),
            deadline,
            time,
        )
        .await?;
        if let Err(err) = make_future().await {
            make_progress().fail(format!(
                "{} : ignored by repeat_daily_at('{:?}',…) iteration {}",
                err, time, iteration
            ))
        }
    }
}

pub async fn repeat_every_s<MakeFut, MakeProgress, Fut, T>(
    interval_s: u32,
    mut make_progress: MakeProgress,
    deadline: Option<SystemTime>,
    at_most: Option<usize>,
    mut make_future: MakeFut,
) -> Result<()>
where
    Fut: Future<Output = Result<T>>,
    MakeFut: FnMut() -> Fut,
    MakeProgress: FnMut() -> prodash::tree::Item,
{
    let max_iterations = at_most.unwrap_or(std::usize::MAX);
    let mut iteration = 0;
    loop {
        if iteration == max_iterations {
            return Ok(());
        }
        iteration += 1;
        if let Err(err) = make_future().await {
            make_progress().fail(format!(
                "{} : ignored by repeat_every({}s,…) iteration {}",
                err, interval_s, iteration
            ))
        }
        if iteration == max_iterations {
            return Ok(());
        }
        wait_with_progress(interval_s, make_progress(), deadline, None).await?;
    }
}

pub fn check(deadline: Option<SystemTime>) -> Result<()> {
    deadline
        .map(|d| {
            if SystemTime::now() >= d {
                Err(Error::DeadlineExceeded(FormatDeadline(d)))
            } else {
                Ok(())
            }
        })
        .unwrap_or(Ok(()))
}

pub async fn timeout_after<F, T>(duration: Duration, msg: impl Into<String>, f: F) -> Result<T>
where
    F: Future<Output = T> + Unpin,
{
    let selector = future::select(Delay::new(duration), f);
    match selector.await {
        Either::Left((_, _f)) => Err(Error::Timeout(duration, msg.into())),
        Either::Right((r, _delay)) => Ok(r),
    }
}

pub async fn enforce<F, T>(deadline: Option<SystemTime>, f: F) -> Result<T>
where
    F: Future<Output = T> + Unpin,
{
    match deadline {
        Some(d) => {
            let selector = future::select(
                Delay::new(d.duration_since(SystemTime::now()).unwrap_or_default()),
                f,
            );
            match selector.await {
                Either::Left((_, _f)) => Err(Error::DeadlineExceeded(FormatDeadline(d))),
                Either::Right((r, _delay)) => Ok(r),
            }
        }
        None => Ok(f.await),
    }
}

pub async fn enforce_blocking<F, T>(deadline: Option<SystemTime>, f: F, s: impl Spawn) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    enforce(deadline, s.spawn_with_handle(async { f() })?).await
}
