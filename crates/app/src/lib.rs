//! 桌面应用的业务层：任务的存储、生命周期与节流后的进度，建在 hs-m3u8-core 之上，不依赖界面框架。

mod error;
mod preference;
mod service;
mod store;
mod task;

pub use error::Error;
pub use preference::StoredPreference;
pub use service::{Event, Service};
pub use store::Store;
pub use task::{OutputSpec, Status, Task, TaskId, TaskSpec};
