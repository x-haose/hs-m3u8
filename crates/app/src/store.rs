//! 任务存储：一个 SQLite 文件。表结构的版本记在 `PRAGMA user_version`，打开时按版本迁移；读写函数接受调用方
//! 开好的事务，事务边界由服务层定。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, Transaction, params};

use crate::preference::StoredPreference;
use crate::{Error, OutputSpec, Status, Task, TaskId, TaskSpec};

/// 各版本的迁移：第 i 项把版本 i 升到 i + 1。已发布的迁移不再改动，新的表结构只追加一项。
const MIGRATIONS: [&str; 1] = ["CREATE TABLE task (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    url TEXT NOT NULL,
    preference TEXT NOT NULL,
    headers TEXT NOT NULL,
    output_kind TEXT NOT NULL CHECK (output_kind IN ('mp4', 'hls', 'both')),
    mp4_path TEXT,
    hls_path TEXT,
    live INTEGER NOT NULL CHECK (live IN (0, 1)),
    status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'failed', 'done')),
    error TEXT,
    created_at INTEGER NOT NULL
) STRICT;"];

/// 打开的任务存储。
pub struct Store {
    conn: Connection,
}

impl Store {
    /// 打开（不存在时新建）`path` 处的存储并迁移到当前版本；所在目录不存在时创建。Unix 上文件只有本人可读写：
    /// 里面明文存着请求头。版本比本应用认识的新时报 [`Error::Corrupt`]（用新版本的应用打开过）。
    pub fn open(path: &Path) -> Result<Store, Error> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io("创建", dir))?;
        }
        let conn = Connection::open(path)?;
        restrict_to_owner(path)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// 内存里的存储，用于测试。
    pub fn in_memory() -> Result<Store, Error> {
        let mut store = Store {
            conn: Connection::open_in_memory()?,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn transaction(&mut self) -> Result<Transaction<'_>, Error> {
        Ok(self.conn.transaction()?)
    }

    fn migrate(&mut self) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        let version = usize::try_from(version)
            .map_err(|_| Error::Corrupt(format!("表结构版本 {version} 不是非负数")))?;
        if version > MIGRATIONS.len() {
            return Err(Error::Corrupt(format!(
                "表结构版本 {version} 比本应用认识的 {} 新",
                MIGRATIONS.len()
            )));
        }
        for migration in &MIGRATIONS[version..] {
            tx.execute_batch(migration)?;
        }
        tx.pragma_update(None, "user_version", schema_version(MIGRATIONS.len()))?;
        tx.commit()?;
        Ok(())
    }
}

/// 新建任务，状态为运行中；返回分配的编号。
pub fn insert(tx: &Transaction<'_>, spec: &TaskSpec, created_at: i64) -> Result<TaskId, Error> {
    let (kind, mp4, hls) = output_columns(&spec.output)?;
    tx.execute(
        "INSERT INTO task (title, url, preference, headers, output_kind, mp4_path, hls_path, live, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running', ?9)",
        params![
            spec.title,
            spec.url.as_str(),
            to_json(&spec.preference),
            to_json(&spec.headers),
            kind,
            mp4,
            hls,
            spec.live,
            created_at
        ],
    )?;
    Ok(TaskId(tx.last_insert_rowid()))
}

/// 全部任务，按新建先后。
pub fn list(tx: &Transaction<'_>) -> Result<Vec<Task>, Error> {
    let mut statement = tx.prepare(
        "SELECT id, title, url, preference, headers, output_kind, mp4_path, hls_path, live, status, error, created_at
         FROM task ORDER BY id",
    )?;
    let rows = statement.query_map([], Row::read)?;
    rows.map(|row| row?.into_task()).collect()
}

/// 一个任务；没有时为 None。
pub fn get(tx: &Transaction<'_>, id: TaskId) -> Result<Option<Task>, Error> {
    tx.query_row(
        "SELECT id, title, url, preference, headers, output_kind, mp4_path, hls_path, live, status, error, created_at
         FROM task WHERE id = ?1",
        [id.0],
        Row::read,
    )
    .optional()?
    .map(Row::into_task)
    .transpose()
}

/// 改状态；没有这个任务时报 [`Error::NotFound`]。
pub fn set_status(tx: &Transaction<'_>, id: TaskId, status: &Status) -> Result<(), Error> {
    let (name, error) = match status {
        Status::Running => ("running", None),
        Status::Paused => ("paused", None),
        Status::Failed(message) => ("failed", Some(message.as_str())),
        Status::Done => ("done", None),
    };
    let changed = tx.execute(
        "UPDATE task SET status = ?2, error = ?3 WHERE id = ?1",
        params![id.0, name, error],
    )?;
    if changed == 0 {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 把还标着运行中的任务改为暂停：应用上次异常退出时没来得及改；返回改了几个。
pub fn pause_interrupted(tx: &Transaction<'_>) -> Result<usize, Error> {
    Ok(tx.execute(
        "UPDATE task SET status = 'paused' WHERE status = 'running'",
        [],
    )?)
}

/// 删除任务；没有时报 [`Error::NotFound`]。
pub fn delete(tx: &Transaction<'_>, id: TaskId) -> Result<(), Error> {
    if tx.execute("DELETE FROM task WHERE id = ?1", [id.0])? == 0 {
        return Err(Error::NotFound(id));
    }
    Ok(())
}

/// 表里的一行，原样读出、未校验。
struct Row {
    id: i64,
    title: String,
    url: String,
    preference: String,
    headers: String,
    output_kind: String,
    mp4_path: Option<String>,
    hls_path: Option<String>,
    live: bool,
    status: String,
    error: Option<String>,
    created_at: i64,
}

impl Row {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
        Ok(Row {
            id: row.get(0)?,
            title: row.get(1)?,
            url: row.get(2)?,
            preference: row.get(3)?,
            headers: row.get(4)?,
            output_kind: row.get(5)?,
            mp4_path: row.get(6)?,
            hls_path: row.get(7)?,
            live: row.get(8)?,
            status: row.get(9)?,
            error: row.get(10)?,
            created_at: row.get(11)?,
        })
    }

    fn into_task(self) -> Result<Task, Error> {
        let id = TaskId(self.id);
        let corrupt = |what: &str| Error::Corrupt(format!("任务 {} 的{what}", self.id));
        let url = self.url.parse().map_err(|_| corrupt("地址"))?;
        let preference: StoredPreference =
            serde_json::from_str(&self.preference).map_err(|_| corrupt("选轨偏好"))?;
        let headers: Vec<(String, String)> =
            serde_json::from_str(&self.headers).map_err(|_| corrupt("请求头"))?;
        let output = match (self.output_kind.as_str(), self.mp4_path, self.hls_path) {
            ("mp4", Some(mp4), None) => OutputSpec::Mp4(mp4.into()),
            ("hls", None, Some(hls)) => OutputSpec::Hls(hls.into()),
            ("both", Some(mp4), Some(hls)) => OutputSpec::Both {
                mp4: mp4.into(),
                hls: hls.into(),
            },
            _ => return Err(corrupt("输出")),
        };
        let status = match (self.status.as_str(), self.error) {
            ("running", None) => Status::Running,
            ("paused", None) => Status::Paused,
            ("failed", Some(message)) => Status::Failed(message),
            ("done", None) => Status::Done,
            _ => return Err(corrupt("状态")),
        };
        Ok(Task {
            id,
            spec: TaskSpec {
                title: self.title,
                url,
                preference,
                headers,
                output,
                live: self.live,
            },
            status,
            created_at: self.created_at,
        })
    }
}

/// 输出在表里的三列：种类与两个路径。路径须是 UTF-8（核心对输出路径的要求相同）。
fn output_columns(
    output: &OutputSpec,
) -> Result<(&'static str, Option<String>, Option<String>), Error> {
    let text = |path: &PathBuf| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::InvalidInput(format!("输出路径不是 UTF-8：{}", path.display())))
    };
    Ok(match output {
        OutputSpec::Mp4(mp4) => ("mp4", Some(text(mp4)?), None),
        OutputSpec::Hls(hls) => ("hls", None, Some(text(hls)?)),
        OutputSpec::Both { mp4, hls } => ("both", Some(text(mp4)?), Some(text(hls)?)),
    })
}

/// 表结构版本在 `user_version` 里的写法。
fn schema_version(version: usize) -> i64 {
    i64::try_from(version).expect("迁移的项数远小于 i64 的上限")
}

fn to_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("只含字符串、整数、数组与枚举，序列化不会失败")
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_path_buf();
    move |cause| Error::Io {
        action,
        path,
        cause,
    }
}

/// 存储文件只有本人可读写。Windows 上用户目录本就只有本人能访问，不另设。
fn restrict_to_owner(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(io("设置权限", path))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use hs_m3u8_core::hls::Preference;

    use super::*;

    fn spec(output: OutputSpec) -> TaskSpec {
        TaskSpec {
            title: "主题演讲".into(),
            url: "https://media.example.com/keynote/master.m3u8?token=1"
                .parse()
                .unwrap(),
            preference: StoredPreference::try_from(&Preference::default()).unwrap(),
            headers: vec![("Referer".into(), "https://example.com/".into())],
            output,
            live: true,
        }
    }

    #[test]
    fn tasks_round_trip_and_change_status() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let a = insert(&tx, &spec(OutputSpec::Mp4("/m/a.mp4".into())), 100).unwrap();
        let b = insert(
            &tx,
            &spec(OutputSpec::Both {
                mp4: "/m/b.mp4".into(),
                hls: "/m/b".into(),
            }),
            200,
        )
        .unwrap();
        set_status(&tx, a, &Status::Failed("403".into())).unwrap();
        set_status(&tx, b, &Status::Done).unwrap();
        tx.commit().unwrap();

        let tx = store.transaction().unwrap();
        let tasks = list(&tx).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].spec, spec(OutputSpec::Mp4("/m/a.mp4".into())));
        assert_eq!(tasks[0].status, Status::Failed("403".into()));
        assert_eq!(tasks[1].status, Status::Done);
        assert_eq!(get(&tx, b).unwrap().unwrap().created_at, 200);
        delete(&tx, a).unwrap();
        assert!(matches!(delete(&tx, a), Err(Error::NotFound(_))));
        assert!(matches!(
            set_status(&tx, a, &Status::Done),
            Err(Error::NotFound(_))
        ));
        assert_eq!(get(&tx, a).unwrap(), None);
    }

    #[test]
    fn interrupted_tasks_become_paused() {
        let mut store = Store::in_memory().unwrap();
        let tx = store.transaction().unwrap();
        let running = insert(&tx, &spec(OutputSpec::Hls("/m/c".into())), 1).unwrap();
        let done = insert(&tx, &spec(OutputSpec::Hls("/m/d".into())), 2).unwrap();
        set_status(&tx, done, &Status::Done).unwrap();
        assert_eq!(pause_interrupted(&tx).unwrap(), 1);
        assert_eq!(get(&tx, running).unwrap().unwrap().status, Status::Paused);
        assert_eq!(get(&tx, done).unwrap().unwrap().status, Status::Done);
    }

    #[test]
    fn a_store_from_a_newer_version_is_refused_and_rows_are_checked() {
        let dir = std::env::temp_dir().join(format!("hs-m3u8-app-store-{}", std::process::id()));
        let path = dir.join("tasks.sqlite");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut store = Store::open(&path).unwrap();
            let tx = store.transaction().unwrap();
            tx.execute(
                "INSERT INTO task (title, url, preference, headers, output_kind, mp4_path, live, status, created_at)
                 VALUES ('x', 'https://a/b.m3u8', '{}', '[]', 'mp4', '/m/x.mp4', 0, 'done', 1)",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
            let tx = store.transaction().unwrap();
            assert!(matches!(list(&tx), Err(Error::Corrupt(_))));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", schema_version(MIGRATIONS.len() + 1))
            .unwrap();
        assert!(matches!(Store::open(&path), Err(Error::Corrupt(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
