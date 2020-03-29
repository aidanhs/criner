use crate::error::{Error, FormatDeadline, Result};
use dia_semver::Semver;
use futures::task::SpawnExt;
use futures::{
    future::{self, Either},
    task::Spawn,
};
use futures_timer::Delay;
use std::{
    convert::TryInto,
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
            time.format("%R %p")
        ));
    }
    for s in 1..=duration_s {
        Delay::new(Duration::from_secs(1)).await;
        check(deadline)?;
        progress.set(s);
    }
    Ok(())
}

fn desired_launch_at(time: Option<time::Time>) -> time::OffsetDateTime {
    let time = time.unwrap_or_else(|| time::OffsetDateTime::now_local().time());
    let now = time::OffsetDateTime::now_local();
    let mut desired = now.date().with_time(time).assume_offset(now.offset());
    if desired < now {
        desired = desired
            .date()
            .next_day()
            .with_time(time)
            .assume_offset(now.offset());
    }
    desired
}

fn duration_until(time: Option<time::Time>) -> Duration {
    let desired = desired_launch_at(time);
    (desired - time::OffsetDateTime::now_local())
        .try_into()
        .unwrap_or(Duration::from_secs(1))
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
    let time = desired_launch_at(time).time();
    loop {
        iteration += 1;
        if let Err(err) = make_future().await {
            make_progress().fail(format!(
                "{} : ignored by repeat_daily_at('{:?}',…) iteration {}",
                err, time, iteration
            ))
        }
        wait_with_progress(
            duration_until(Some(time)).as_secs() as u32,
            make_progress(),
            deadline,
            Some(time),
        )
        .await?;
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

/// Use this if `f()` might block forever, due to code that doesn't implement timeouts like libgit2 fetch does as it has no timeout
/// on 'recv' bytes.
/// Even though the calling thread or future won't be blocked, the spawned thread calling the future will be blocked forever.
/// However, this is better than blocking a futures-threadpool thread, which quickly freezes the whole program as there are
/// not too many of these.
///
/// This approach eventually fails as we would accumulate more and more threads, but this will also give use additional
/// days of runtime for little effort. On a Chinese network, outside of data centers, one can probably restart criner on
/// a weekly basis or so, which is can easily be automated.
pub async fn enforce_threaded<F, T>(deadline: SystemTime, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let res = f();
        tx.send(res).ok() // if this fails, we will timeout. Can't enforce Debug to be implemented
    });
    let selector = future::select(
        Delay::new(
            deadline
                .duration_since(SystemTime::now())
                .unwrap_or_default(),
        ),
        rx,
    );
    match selector.await {
        Either::Left((_, _rx)) => Err(Error::DeadlineExceeded(FormatDeadline(deadline))),
        Either::Right((Ok(res), _delay)) => Ok(res),
        Either::Right((Err(err), _delay)) => Err(Error::Message(format!("{}", err))),
    }
}

pub async fn enforce_blocking<F, T>(deadline: Option<SystemTime>, f: F, s: impl Spawn) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    enforce(deadline, s.spawn_with_handle(async { f() })?).await
}
