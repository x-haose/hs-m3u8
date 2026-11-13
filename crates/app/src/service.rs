//! 任务服务：持有核心的 [`Engine`] 与它的 tokio 运行时，按存储里的任务开始、暂停、继续、停止与放弃，把节流后的
//! 进度与结束状态经事件通道交给界面。

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hs_m3u8_core::hls::Preference;
use hs_m3u8_core::{
    CancellationToken, Engine, Job, JobControl, JobRequest, Leftover, Output, Probe, Progress,
    Source, Unsettled,
};
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::store::{self, Store};
use crate::{Error, Status, Task, TaskId, TaskSpec};

/// 同一任务两次进度事件之间至少隔多久：界面每秒最多重画十次进度。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// 交给界面的事件。
#[derive(Debug)]
pub enum Event {
    /// 运行中任务的进度快照；同一任务两次之间至少隔 100 毫秒
    Progress { id: TaskId, progress: Box<Progress> },
    /// 任务停下，已写进存储的状态；`output` 为写出的结果（状态为 [`Status::Done`] 时有）
    Ended {
        id: TaskId,
        status: Status,
        output: Option<Box<Output>>,
    },
    /// 任务停下了，但状态没能写进存储；存储里仍是运行中，下次打开应用时按暂停处理
    StoreFailed { id: TaskId, error: Error },
}

/// 一个正在运行的任务。
struct Running {
    control: JobControl,
    /// 停下（状态已写进存储、已从运行表里去掉）时变为 true
    finished: watch::Receiver<bool>,
}

pub struct Service {
    runtime: Runtime,
    engine: Engine,
    store: Arc<Mutex<Store>>,
    running: Arc<Mutex<HashMap<TaskId, Running>>>,
    events: mpsc::UnboundedSender<Event>,
}

impl Service {
    /// 用 `store` 建服务，同时最多 `max_requests` 个请求（见 [`Engine::new`]）；返回服务与事件的接收端。上次异常退出时
    /// 还标着运行中的任务改为暂停。
    pub fn new(
        mut store: Store,
        max_requests: NonZeroUsize,
    ) -> Result<(Service, mpsc::UnboundedReceiver<Event>), Error> {
        let tx = store.transaction()?;
        store::pause_interrupted(&tx)?;
        tx.commit()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("hs-m3u8-engine")
            .build()
            .map_err(|cause| Error::Io {
                action: "启动",
                path: "tokio 运行时".into(),
                cause,
            })?;
        let engine = {
            let _entered = runtime.enter();
            Engine::new(max_requests)
        };
        let (events, receiver) = mpsc::unbounded_channel();
        let service = Service {
            runtime,
            engine,
            store: Arc::new(Mutex::new(store)),
            running: Arc::new(Mutex::new(HashMap::new())),
            events,
        };
        Ok((service, receiver))
    }

    /// 全部任务，按新建先后。
    pub fn tasks(&self) -> Result<Vec<Task>, Error> {
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        store::list(&tx)
    }

    /// 探测来源；返回的句柄在任何执行器上都可等待，丢弃它不取消探测。
    pub fn probe(&self, source: Source) -> JoinHandle<Result<Probe, hs_m3u8_core::Error>> {
        let engine = self.engine.clone();
        self.runtime
            .spawn(async move { engine.probe(&source).await })
    }

    /// 新建任务并开始；`created_at` 为新建时间（Unix 秒）。参数不合要求或存储写不进时什么也不存、不开始。
    pub fn create(&self, spec: TaskSpec, created_at: i64) -> Result<TaskId, Error> {
        let request = request(&spec);
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        let id = store::insert(&tx, &spec, created_at)?;
        let job = self.launch(id, request)?;
        commit_or_cancel(tx, &job)?;
        drop(store);
        self.track(id, job);
        Ok(id)
    }

    /// 继续暂停或失败的任务：照新建时的请求再开始，任务目录里已完成的分片不重下。
    pub fn resume(&self, id: TaskId) -> Result<(), Error> {
        if lock(&self.running).contains_key(&id) {
            return Err(Error::Running(id));
        }
        let mut store = lock(&self.store);
        let tx = store.transaction()?;
        let task = store::get(&tx, id)?.ok_or(Error::NotFound(id))?;
        store::set_status(&tx, id, &Status::Running)?;
        let job = self.launch(id, request(&task.spec))?;
        commit_or_cancel(tx, &job)?;
        drop(store);
        self.track(id, job);
        Ok(())
    }

    /// 暂停：取消运行中的任务，保留任务目录；停下后状态为 [`Status::Paused`]。没在运行时什么也不做。
    pub fn pause(&self, id: TaskId) {
        if let Some(running) = lock(&self.running).get(&id) {
            running.control.cancel();
        }
    }

    /// 停止录制：把已列出的分片下完后写出（见 [`JobControl::stop`]）。没在运行时什么也不做。
    pub fn stop(&self, id: TaskId) {
        if let Some(running) = lock(&self.running).get(&id) {
            running.control.stop();
        }
    }

    /// 放弃任务：在运行就先取消并等它停下，再删掉任务目录（见 [`Engine::discard`]）与存储里的记录；返回留下的东西。
    /// 返回的句柄在任何执行器上都可等待。
    pub fn discard(&self, id: TaskId) -> JoinHandle<Result<Vec<Leftover>, Error>> {
        let engine = self.engine.clone();
        let store = self.store.clone();
        let finished = lock(&self.running).get(&id).map(|running| {
            running.control.cancel();
            running.finished.clone()
        });
        self.runtime.spawn(async move {
            if let Some(mut finished) = finished {
                finished
                    .wait_for(|done| *done)
                    .await
                    .expect("运行表里的任务停下时才丢弃发送端，那时已置为 true");
            }
            let task = {
                let mut store = lock(&store);
                let tx = store.transaction()?;
                store::get(&tx, id)?.ok_or(Error::NotFound(id))?
            };
            let work_dir = task.spec.output.options().resolved_work_dir()?;
            let leftovers = engine
                .discard(&work_dir, Unsettled::Fail, &CancellationToken::new())
                .await?;
            let mut store = lock(&store);
            let tx = store.transaction()?;
            store::delete(&tx, id)?;
            tx.commit()?;
            Ok(leftovers)
        })
    }

    /// 在运行时里开始任务；`start` 的参数错误照原样报出。
    fn launch(&self, id: TaskId, request: JobRequest) -> Result<Job, Error> {
        if lock(&self.running).contains_key(&id) {
            return Err(Error::Running(id));
        }
        let _entered = self.runtime.enter();
        Ok(self.engine.start(request)?)
    }

    /// 登记运行中的任务，转发它的进度，等它停下后把状态写进存储并发出 [`Event::Ended`]。
    fn track(&self, id: TaskId, job: Job) {
        let control = job.control();
        let (finished_tx, finished) = watch::channel(false);
        lock(&self.running).insert(
            id,
            Running {
                control: control.clone(),
                finished,
            },
        );
        let events = self.events.clone();
        let mut progress = control.progress();
        let forwarding = self.runtime.spawn(async move {
            while progress.changed().await.is_ok() {
                let snapshot = progress.borrow_and_update().clone();
                if events
                    .send(Event::Progress {
                        id,
                        progress: Box::new(snapshot),
                    })
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(PROGRESS_INTERVAL).await;
            }
        });
        let events = self.events.clone();
        let store = self.store.clone();
        let running = self.running.clone();
        self.runtime.spawn(async move {
            let result = job.wait().await;
            forwarding.abort();
            let (status, output) = match result {
                Ok(output) => (Status::Done, Some(Box::new(output))),
                Err(hs_m3u8_core::Error::Cancelled) => (Status::Paused, None),
                Err(error) => (Status::Failed(error.to_string()), None),
            };
            let saved = save_status(&store, id, &status);
            lock(&running).remove(&id);
            finished_tx.send_replace(true);
            let event = match saved {
                Ok(()) => Event::Ended { id, status, output },
                Err(error) => Event::StoreFailed { id, error },
            };
            // 接收端随界面关闭而丢弃时没有谁要这个事件；状态已写进存储
            let _closed = events.send(event);
        });
    }
}

/// 提交开始任务的事务；提交失败时取消已开始的任务，存储里也没有它。
fn commit_or_cancel(tx: rusqlite::Transaction<'_>, job: &Job) -> Result<(), Error> {
    tx.commit().map_err(|error| {
        job.control().cancel();
        Error::Store(error)
    })
}

fn save_status(store: &Mutex<Store>, id: TaskId, status: &Status) -> Result<(), Error> {
    let mut store = lock(store);
    let tx = store.transaction()?;
    store::set_status(&tx, id, status)?;
    tx.commit()?;
    Ok(())
}

/// 照任务的内容建请求：不是直播时不开录制，来源是直播就报 [`hs_m3u8_core::Error::LiveNotEnabled`]。
fn request(spec: &TaskSpec) -> JobRequest {
    let mut source = Source::new(spec.url.clone());
    source.preference = Preference::from(&spec.preference);
    source.http.headers.clone_from(&spec.headers);
    let mut request = JobRequest::new(source, spec.output.options());
    if !spec.live {
        request.live = None;
    }
    request
}

/// 加锁；持锁期间只做内存与 SQLite 的读写，不会 panic，锁中毒即不变量被破坏。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("持锁期间不会 panic")
}
