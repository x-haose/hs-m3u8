//! 任务的进度与结果，以及失败原因里共用的类型：请求失败、分片取不到、直播刷新失败与停滞的原因、残留、合并出的流。

mod failure;
mod output;
mod progress;

use pyo3::prelude::*;

pub(crate) use self::failure::{
    FetchErrorKind, FetchFailure, HttpErrorKind, HttpFailure, Leftover, LeftoverKind, RefreshCause,
    RefreshReason, StallCause,
};
pub(crate) use self::output::{
    LiveEnd, LiveReport, MediaReport, MissReason, Missed, Mp4Output, NewSession, Output, Shape,
    SourceBreak, StreamKind, StreamReport,
};
pub(crate) use self::progress::{FileCount, Progress, Retrying, SegmentCounts, Stage, Writing};

pub(crate) fn register(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<HttpErrorKind>()?;
    module.add_class::<HttpFailure>()?;
    module.add_class::<FetchErrorKind>()?;
    module.add_class::<FetchFailure>()?;
    module.add_class::<SegmentCounts>()?;
    module.add_class::<Retrying>()?;
    module.add_class::<Writing>()?;
    module.add_class::<FileCount>()?;
    module.add_class::<RefreshReason>()?;
    module.add_class::<RefreshCause>()?;
    module.add_class::<StallCause>()?;
    module.add_class::<LeftoverKind>()?;
    module.add_class::<Leftover>()?;
    module.add_class::<StreamKind>()?;
    module.add_class::<Shape>()?;
    module.add_class::<Stage>()?;
    module.add_class::<Progress>()?;
    module.add_class::<StreamReport>()?;
    module.add_class::<MediaReport>()?;
    module.add_class::<Mp4Output>()?;
    module.add_class::<LiveEnd>()?;
    module.add_class::<SourceBreak>()?;
    module.add_class::<NewSession>()?;
    module.add_class::<MissReason>()?;
    module.add_class::<Missed>()?;
    module.add_class::<LiveReport>()?;
    module.add_class::<Output>()?;
    Ok(())
}
