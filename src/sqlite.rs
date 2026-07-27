use anyhow::{Context, Result, anyhow};
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_uchar, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

type Sqlite3 = c_void;
type Sqlite3Stmt = c_void;
type OpenV2 = unsafe extern "C" fn(*const c_char, *mut *mut Sqlite3, c_int, *const c_char) -> c_int;
type Close = unsafe extern "C" fn(*mut Sqlite3) -> c_int;
type PrepareV2 = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    c_int,
    *mut *mut Sqlite3Stmt,
    *mut *const c_char,
) -> c_int;
type Step = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type Finalize = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type ColumnCount = unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int;
type ColumnText = unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const c_uchar;
type Errmsg = unsafe extern "C" fn(*mut Sqlite3) -> *const c_char;
type BusyTimeout = unsafe extern "C" fn(*mut Sqlite3, c_int) -> c_int;
type Exec = unsafe extern "C" fn(
    *mut Sqlite3,
    *const c_char,
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x00000001;
const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
const SQLITE_OPEN_CREATE: c_int = 0x00000004;
const SQLITE_OPEN_URI: c_int = 0x00000040;
const SQLITE_OPEN_NOMUTEX: c_int = 0x00008000;

struct Api {
    _library: Library,
    open_v2: OpenV2,
    close: Close,
    prepare_v2: PrepareV2,
    step: Step,
    finalize: Finalize,
    column_count: ColumnCount,
    column_text: ColumnText,
    errmsg: Errmsg,
    busy_timeout: BusyTimeout,
    exec: Exec,
}

impl Api {
    fn load() -> Result<Self> {
        unsafe {
            let library = Library::new("winsqlite3.dll")
                .context("Windows system SQLite library winsqlite3.dll could not be loaded")?;
            let open_v2 = *library.get::<OpenV2>(b"sqlite3_open_v2\0")?;
            let close = *library.get::<Close>(b"sqlite3_close\0")?;
            let prepare_v2 = *library.get::<PrepareV2>(b"sqlite3_prepare_v2\0")?;
            let step = *library.get::<Step>(b"sqlite3_step\0")?;
            let finalize = *library.get::<Finalize>(b"sqlite3_finalize\0")?;
            let column_count = *library.get::<ColumnCount>(b"sqlite3_column_count\0")?;
            let column_text = *library.get::<ColumnText>(b"sqlite3_column_text\0")?;
            let errmsg = *library.get::<Errmsg>(b"sqlite3_errmsg\0")?;
            let busy_timeout = *library.get::<BusyTimeout>(b"sqlite3_busy_timeout\0")?;
            let exec = *library.get::<Exec>(b"sqlite3_exec\0")?;
            Ok(Self {
                _library: library,
                open_v2,
                close,
                prepare_v2,
                step,
                finalize,
                column_count,
                column_text,
                errmsg,
                busy_timeout,
                exec,
            })
        }
    }
}

pub struct Connection {
    api: Arc<Api>,
    db: *mut Sqlite3,
}

unsafe impl Send for Connection {}

impl Connection {
    pub fn open_readonly(path: &Path) -> Result<Self> {
        Self::open(
            path,
            SQLITE_OPEN_READONLY | SQLITE_OPEN_URI | SQLITE_OPEN_NOMUTEX,
        )
    }

    pub fn open_readwrite_create(path: &Path) -> Result<Self> {
        Self::open(
            path,
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_URI | SQLITE_OPEN_NOMUTEX,
        )
    }

    fn open(path: &Path, flags: c_int) -> Result<Self> {
        let api = Arc::new(Api::load()?);
        let normalized = path.to_string_lossy().replace('\\', "/");
        let mode = if flags & SQLITE_OPEN_READONLY != 0 {
            "?mode=ro"
        } else {
            ""
        };
        let uri = format!("file:/{}{}", normalized.trim_start_matches('/'), mode);
        let c_uri = CString::new(uri)?;
        let mut db = ptr::null_mut();
        let rc = unsafe { (api.open_v2)(c_uri.as_ptr(), &mut db, flags, ptr::null()) };
        if rc != SQLITE_OK || db.is_null() {
            let message = if db.is_null() {
                format!("SQLite open failed with status {rc}")
            } else {
                unsafe { CStr::from_ptr((api.errmsg)(db)) }
                    .to_string_lossy()
                    .into_owned()
            };
            if !db.is_null() {
                unsafe { (api.close)(db) };
            }
            return Err(anyhow!("{message}")).with_context(|| format!("open {}", path.display()));
        }
        unsafe { (api.busy_timeout)(db, 3000) };
        Ok(Self { api, db })
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql)?;
        let rc = unsafe {
            (self.api.exec)(
                self.db,
                c_sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            return Err(anyhow!(self.error_message())).context("execute SQLite batch");
        }
        Ok(())
    }

    pub fn query(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let c_sql = CString::new(sql)?;
        let mut statement = ptr::null_mut();
        let rc = unsafe {
            (self.api.prepare_v2)(self.db, c_sql.as_ptr(), -1, &mut statement, ptr::null_mut())
        };
        if rc != SQLITE_OK {
            return Err(anyhow!(self.error_message())).context("prepare SQLite query");
        }

        let result = (|| {
            let mut rows = Vec::new();
            loop {
                match unsafe { (self.api.step)(statement) } {
                    SQLITE_ROW => {
                        let count = unsafe { (self.api.column_count)(statement) };
                        let mut row = Vec::with_capacity(count as usize);
                        for index in 0..count {
                            let value = unsafe { (self.api.column_text)(statement, index) };
                            if value.is_null() {
                                row.push(None);
                            } else {
                                row.push(Some(
                                    unsafe { CStr::from_ptr(value.cast::<c_char>()) }
                                        .to_string_lossy()
                                        .into_owned(),
                                ));
                            }
                        }
                        rows.push(row);
                    }
                    SQLITE_DONE => break,
                    _ => return Err(anyhow!(self.error_message())).context("execute SQLite query"),
                }
            }
            Ok(rows)
        })();
        unsafe { (self.api.finalize)(statement) };
        result
    }

    fn error_message(&self) -> String {
        unsafe { CStr::from_ptr((self.api.errmsg)(self.db)) }
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe { (self.api.close)(self.db) };
        }
    }
}
