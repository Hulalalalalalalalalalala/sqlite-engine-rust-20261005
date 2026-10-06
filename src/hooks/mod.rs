//! Commit, Data Change and Rollback Notification Callbacks
#![expect(non_camel_case_types)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::ffi;

use crate::{Connection, Error, InnerConnection, Result, error::decode_result_raw};

#[cfg(feature = "preupdate_hook")]
pub use preupdate_hook::*;

#[cfg(feature = "preupdate_hook")]
mod preupdate_hook;

/// Action Codes
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
#[non_exhaustive]
pub enum Action {
    /// Unsupported / unexpected action
    UNKNOWN = -1,
    /// DELETE command
    SQLITE_DELETE = ffi::SQLITE_DELETE,
    /// INSERT command
    SQLITE_INSERT = ffi::SQLITE_INSERT,
    /// UPDATE command
    SQLITE_UPDATE = ffi::SQLITE_UPDATE,
}

impl From<i32> for Action {
    #[inline]
    fn from(code: i32) -> Self {
        match code {
            ffi::SQLITE_DELETE => Self::SQLITE_DELETE,
            ffi::SQLITE_INSERT => Self::SQLITE_INSERT,
            ffi::SQLITE_UPDATE => Self::SQLITE_UPDATE,
            _ => Self::UNKNOWN,
        }
    }
}

/// The context received by an authorizer hook.
///
/// See <https://sqlite.org/c3ref/set_authorizer.html> for more info.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthContext<'c> {
    /// The action to be authorized.
    pub action: AuthAction<'c>,

    /// The database name, if applicable.
    pub database_name: Option<&'c str>,

    /// The inner-most trigger or view responsible for the access attempt.
    /// `None` if the access attempt was made by top-level SQL code.
    pub accessor: Option<&'c str>,
}

/// Actions and arguments found within a statement during
/// preparation.
///
/// See <https://sqlite.org/c3ref/c_alter_table.html> for more info.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum AuthAction<'c> {
    /// This variant is not normally produced by SQLite. You may encounter it
    // if you're using a different version than what's supported by this library.
    Unknown {
        /// The unknown authorization action code.
        code: i32,
        /// The third arg to the authorizer callback.
        arg1: Option<&'c str>,
        /// The fourth arg to the authorizer callback.
        arg2: Option<&'c str>,
    },
    CreateIndex {
        index_name: &'c str,
        table_name: &'c str,
    },
    CreateTable {
        table_name: &'c str,
    },
    CreateTempIndex {
        index_name: &'c str,
        table_name: &'c str,
    },
    CreateTempTable {
        table_name: &'c str,
    },
    CreateTempTrigger {
        trigger_name: &'c str,
        table_name: &'c str,
    },
    CreateTempView {
        view_name: &'c str,
    },
    CreateTrigger {
        trigger_name: &'c str,
        table_name: &'c str,
    },
    CreateView {
        view_name: &'c str,
    },
    Delete {
        table_name: &'c str,
    },
    DropIndex {
        index_name: &'c str,
        table_name: &'c str,
    },
    DropTable {
        table_name: &'c str,
    },
    DropTempIndex {
        index_name: &'c str,
        table_name: &'c str,
    },
    DropTempTable {
        table_name: &'c str,
    },
    DropTempTrigger {
        trigger_name: &'c str,
        table_name: &'c str,
    },
    DropTempView {
        view_name: &'c str,
    },
    DropTrigger {
        trigger_name: &'c str,
        table_name: &'c str,
    },
    DropView {
        view_name: &'c str,
    },
    Insert {
        table_name: &'c str,
    },
    Pragma {
        pragma_name: &'c str,
        /// The pragma value, if present (e.g., `PRAGMA name = value;`).
        pragma_value: Option<&'c str>,
    },
    Read {
        table_name: &'c str,
        column_name: &'c str,
    },
    Select,
    Transaction {
        operation: TransactionOperation,
    },
    Update {
        table_name: &'c str,
        column_name: &'c str,
    },
    Attach {
        filename: &'c str,
    },
    Detach {
        database_name: &'c str,
    },
    AlterTable {
        database_name: &'c str,
        table_name: &'c str,
    },
    Reindex {
        index_name: &'c str,
    },
    Analyze {
        table_name: &'c str,
    },
    CreateVtable {
        table_name: &'c str,
        module_name: &'c str,
    },
    DropVtable {
        table_name: &'c str,
        module_name: &'c str,
    },
    Function {
        function_name: &'c str,
    },
    Savepoint {
        operation: TransactionOperation,
        savepoint_name: &'c str,
    },
    Recursive,
}

impl<'c> AuthAction<'c> {
    fn from_raw(code: i32, arg1: Option<&'c str>, arg2: Option<&'c str>) -> Self {
        match (code, arg1, arg2) {
            (ffi::SQLITE_CREATE_INDEX, Some(index_name), Some(table_name)) => Self::CreateIndex {
                index_name,
                table_name,
            },
            (ffi::SQLITE_CREATE_TABLE, Some(table_name), _) => Self::CreateTable { table_name },
            (ffi::SQLITE_CREATE_TEMP_INDEX, Some(index_name), Some(table_name)) => {
                Self::CreateTempIndex {
                    index_name,
                    table_name,
                }
            }
            (ffi::SQLITE_CREATE_TEMP_TABLE, Some(table_name), _) => {
                Self::CreateTempTable { table_name }
            }
            (ffi::SQLITE_CREATE_TEMP_TRIGGER, Some(trigger_name), Some(table_name)) => {
                Self::CreateTempTrigger {
                    trigger_name,
                    table_name,
                }
            }
            (ffi::SQLITE_CREATE_TEMP_VIEW, Some(view_name), _) => {
                Self::CreateTempView { view_name }
            }
            (ffi::SQLITE_CREATE_TRIGGER, Some(trigger_name), Some(table_name)) => {
                Self::CreateTrigger {
                    trigger_name,
                    table_name,
                }
            }
            (ffi::SQLITE_CREATE_VIEW, Some(view_name), _) => Self::CreateView { view_name },
            (ffi::SQLITE_DELETE, Some(table_name), None) => Self::Delete { table_name },
            (ffi::SQLITE_DROP_INDEX, Some(index_name), Some(table_name)) => Self::DropIndex {
                index_name,
                table_name,
            },
            (ffi::SQLITE_DROP_TABLE, Some(table_name), _) => Self::DropTable { table_name },
            (ffi::SQLITE_DROP_TEMP_INDEX, Some(index_name), Some(table_name)) => {
                Self::DropTempIndex {
                    index_name,
                    table_name,
                }
            }
            (ffi::SQLITE_DROP_TEMP_TABLE, Some(table_name), _) => {
                Self::DropTempTable { table_name }
            }
            (ffi::SQLITE_DROP_TEMP_TRIGGER, Some(trigger_name), Some(table_name)) => {
                Self::DropTempTrigger {
                    trigger_name,
                    table_name,
                }
            }
            (ffi::SQLITE_DROP_TEMP_VIEW, Some(view_name), _) => Self::DropTempView { view_name },
            (ffi::SQLITE_DROP_TRIGGER, Some(trigger_name), Some(table_name)) => Self::DropTrigger {
                trigger_name,
                table_name,
            },
            (ffi::SQLITE_DROP_VIEW, Some(view_name), _) => Self::DropView { view_name },
            (ffi::SQLITE_INSERT, Some(table_name), _) => Self::Insert { table_name },
            (ffi::SQLITE_PRAGMA, Some(pragma_name), pragma_value) => Self::Pragma {
                pragma_name,
                pragma_value,
            },
            (ffi::SQLITE_READ, Some(table_name), Some(column_name)) => Self::Read {
                table_name,
                column_name,
            },
            (ffi::SQLITE_SELECT, ..) => Self::Select,
            (ffi::SQLITE_TRANSACTION, Some(operation_str), _) => Self::Transaction {
                operation: TransactionOperation::from_str(operation_str),
            },
            (ffi::SQLITE_UPDATE, Some(table_name), Some(column_name)) => Self::Update {
                table_name,
                column_name,
            },
            (ffi::SQLITE_ATTACH, Some(filename), _) => Self::Attach { filename },
            (ffi::SQLITE_DETACH, Some(database_name), _) => Self::Detach { database_name },
            (ffi::SQLITE_ALTER_TABLE, Some(database_name), Some(table_name)) => Self::AlterTable {
                database_name,
                table_name,
            },
            (ffi::SQLITE_REINDEX, Some(index_name), _) => Self::Reindex { index_name },
            (ffi::SQLITE_ANALYZE, Some(table_name), _) => Self::Analyze { table_name },
            (ffi::SQLITE_CREATE_VTABLE, Some(table_name), Some(module_name)) => {
                Self::CreateVtable {
                    table_name,
                    module_name,
                }
            }
            (ffi::SQLITE_DROP_VTABLE, Some(table_name), Some(module_name)) => Self::DropVtable {
                table_name,
                module_name,
            },
            (ffi::SQLITE_FUNCTION, _, Some(function_name)) => Self::Function { function_name },
            (ffi::SQLITE_SAVEPOINT, Some(operation_str), Some(savepoint_name)) => Self::Savepoint {
                operation: TransactionOperation::from_str(operation_str),
                savepoint_name,
            },
            (ffi::SQLITE_RECURSIVE, ..) => Self::Recursive,
            (code, arg1, arg2) => Self::Unknown { code, arg1, arg2 },
        }
    }
}

/// A transaction operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum TransactionOperation {
    Unknown,
    Begin,
    Release,
    Rollback,
}

impl TransactionOperation {
    fn from_str(op_str: &str) -> Self {
        match op_str {
            "BEGIN" => Self::Begin,
            "RELEASE" => Self::Release,
            "ROLLBACK" => Self::Rollback,
            _ => Self::Unknown,
        }
    }
}

/// [`authorizer`](Connection::authorizer) return code
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Authorization {
    /// Authorize the action.
    Allow,
    /// Don't allow access, but don't trigger an error either.
    Ignore,
    /// Trigger an error.
    Deny,
}

impl Authorization {
    fn into_raw(self) -> c_int {
        match self {
            Self::Allow => ffi::SQLITE_OK,
            Self::Ignore => ffi::SQLITE_IGNORE,
            Self::Deny => ffi::SQLITE_DENY,
        }
    }
}

impl Connection {
    /// Register a callback function to be invoked whenever
    /// a transaction is committed.
    ///
    /// The callback returns `true` to rollback.
    ///
    /// This hook shares its registration with
    /// [`try_commit_hook`](Connection::try_commit_hook): the most recently
    /// registered hook wins, and unregistering either one removes the
    /// currently registered hook.
    #[inline]
    pub fn commit_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        self.db.borrow_mut().commit_hook(hook)
    }

    /// Register a callback function to be invoked whenever
    /// a transaction is committed, with the ability to report
    /// why a commit was rejected.
    ///
    /// The callback returns `Ok(false)` to let the commit proceed, `Ok(true)`
    /// to roll it back (producing the same error as a
    /// [`commit_hook`](Connection::commit_hook) rejection), or `Err(e)` to
    /// roll it back and have the call that triggered the commit fail with
    /// `e`. If the callback panics, the commit is rolled back and the
    /// triggering call fails with [`Error::UnwindingPanic`].
    ///
    /// This hook shares its registration with
    /// [`commit_hook`](Connection::commit_hook): the most recently registered
    /// hook wins, and unregistering either one removes the currently
    /// registered hook. A rejected commit does not unregister the hook.
    #[inline]
    pub fn try_commit_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() -> Result<bool> + Send + 'static,
    {
        self.db.borrow_mut().try_commit_hook(hook)
    }

    /// Register a callback function to be invoked whenever
    /// a transaction is rolled back.
    #[inline]
    pub fn rollback_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() + Send + 'static,
    {
        self.db.borrow_mut().rollback_hook(hook)
    }

    /// Register a callback function to be invoked whenever
    /// a row is updated, inserted or deleted in a rowid table.
    ///
    /// The callback parameters are:
    ///
    /// - the type of database update (`SQLITE_INSERT`, `SQLITE_UPDATE` or
    ///   `SQLITE_DELETE`),
    /// - the name of the database ("main", "temp", ...),
    /// - the name of the table that is updated,
    /// - the ROWID of the row that is updated.
    #[inline]
    pub fn update_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut(Action, &str, &str, i64) + Send + 'static,
    {
        self.db.borrow_mut().update_hook(hook)
    }

    /// Register a callback that is invoked each time data is committed to a database in wal mode.
    ///
    /// A single database handle may have at most a single write-ahead log callback registered at one time.
    /// Calling `wal_hook` replaces any previously registered write-ahead log callback.
    /// Note that the `sqlite3_wal_autocheckpoint()` interface and the `wal_autocheckpoint` pragma
    /// both invoke `sqlite3_wal_hook()` and will overwrite any prior `sqlite3_wal_hook()` settings.
    ///
    /// The notification is delivered after the commit has completed, so the
    /// write it reports is already durable: returning `Err(e)` does not roll
    /// it back, but the database call that triggered the notification fails
    /// with `e`. If the callback panics, that call fails with
    /// [`Error::UnwindingPanic`]. A failed notification does not unregister
    /// the hook.
    pub fn wal_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut(&Wal, c_int) -> Result<()> + Send + 'static,
    {
        let state = hook.map(|hook| WalHookState {
            hook: Box::new(hook),
            error: None,
        });
        let x = state.as_ref().map(|_| wal_hook_callback as _);
        let mut c = self.db.borrow_mut();
        c.set_clientdata(c"sqlite3_wal_hook", state, |db, bh| unsafe {
            ffi::sqlite3_wal_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        Ok(())
    }

    /// Register a query progress callback.
    ///
    /// The parameter `num_ops` is the approximate number of virtual machine
    /// instructions that are evaluated between successive invocations of the
    /// `handler`. If `num_ops` is less than one then the progress handler
    /// is disabled.
    ///
    /// If the progress callback returns `true`, the operation is interrupted.
    pub fn progress_handler<F>(&mut self, num_ops: c_int, handler: Option<F>) -> Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        self.db.borrow_mut().progress_handler(num_ops, handler)
    }

    /// Register an authorizer callback that's invoked
    /// as a statement is being prepared.
    #[inline]
    pub fn authorizer<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
    {
        self.db.borrow_mut().authorizer(hook)
    }
}

/// Checkpoint mode
#[derive(Clone, Copy)]
#[repr(i32)]
#[non_exhaustive]
pub enum CheckpointMode {
    /// Do as much as possible w/o blocking
    PASSIVE = ffi::SQLITE_CHECKPOINT_PASSIVE,
    /// Wait for writers, then checkpoint
    FULL = ffi::SQLITE_CHECKPOINT_FULL,
    /// Like FULL but wait for readers
    RESTART = ffi::SQLITE_CHECKPOINT_RESTART,
    /// Like RESTART but also truncate WAL
    TRUNCATE = ffi::SQLITE_CHECKPOINT_TRUNCATE,
    /// Do no work at all
    #[cfg(feature = "modern_sqlite")] // 3.51.0
    NOOP = -1, //ffi::SQLITE_CHECKPOINT_NOOP,
}

/// Write-Ahead Log
pub struct Wal {
    db: *mut ffi::sqlite3,
    db_name: *const c_char,
}

impl Wal {
    /// Checkpoint a database
    pub fn checkpoint(&self) -> Result<()> {
        unsafe { decode_result_raw(self.db, ffi::sqlite3_wal_checkpoint(self.db, self.db_name)) }
    }

    /// Checkpoint a database
    pub fn checkpoint_v2(&self, mode: CheckpointMode) -> Result<(c_int, c_int)> {
        let mut n_log = 0;
        let mut n_ckpt = 0;
        unsafe {
            decode_result_raw(
                self.db,
                ffi::sqlite3_wal_checkpoint_v2(
                    self.db,
                    self.db_name,
                    mode as c_int,
                    &raw mut n_log,
                    &raw mut n_ckpt,
                ),
            )?;
        };
        Ok((n_log, n_ckpt))
    }

    /// Name of the database that was written to
    #[must_use]
    pub fn name(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.db_name) }
    }
}

/// The commit hook registered on a connection.
///
/// Both `commit_hook` and `try_commit_hook` share this single registration:
/// the most recent call to either entry point replaces the previous hook,
/// and unregistering either one removes whichever hook is current.
enum CommitHook {
    Simple(Box<dyn FnMut() -> bool + Send>),
    Fallible(Box<dyn FnMut() -> Result<bool> + Send>),
}

struct CommitHookState {
    hook: CommitHook,
    /// The reason the fallible hook rejected the most recent commit attempt,
    /// to be reported by the call that observes SQLite's
    /// `SQLITE_CONSTRAINT_COMMITHOOK` error. Reset on every hook invocation,
    /// so a stale reason can never be attributed to a later failure.
    error: Option<Error>,
}

unsafe extern "C" fn commit_hook_callback(p_arg: *mut c_void) -> c_int {
    unsafe {
        let state: *mut CommitHookState = p_arg.cast();
        match &mut (*state).hook {
            CommitHook::Simple(hook) => {
                let r = catch_unwind(AssertUnwindSafe(hook));
                c_int::from(r.unwrap_or_default())
            }
            CommitHook::Fallible(hook) => {
                let r = catch_unwind(AssertUnwindSafe(hook));
                let (rollback, error) = match r {
                    Ok(Ok(rollback)) => (rollback, None),
                    Ok(Err(err)) => (true, Some(err)),
                    Err(_) => (true, Some(Error::UnwindingPanic)),
                };
                (*state).error = error;
                c_int::from(rollback)
            }
        }
    }
}

/// If `code` reports that the commit hook on `db` rejected a commit
/// (`SQLITE_CONSTRAINT_COMMITHOOK`) and the currently registered fallible
/// hook provided a reason, take and return it. The reason is consumed, so it
/// is reported at most once, by the first call that observes the rejection.
pub(crate) unsafe fn take_try_commit_hook_error(
    db: *mut ffi::sqlite3,
    code: c_int,
) -> Option<Error> {
    if db.is_null()
        || (code != ffi::SQLITE_CONSTRAINT_COMMITHOOK
            && ((code & 0xff) != ffi::SQLITE_CONSTRAINT
                || unsafe { ffi::sqlite3_extended_errcode(db) }
                    != ffi::SQLITE_CONSTRAINT_COMMITHOOK))
    {
        return None;
    }
    let state = unsafe { ffi::sqlite3_get_clientdata(db, c"sqlite3_commit_hook".as_ptr()) }
        .cast::<CommitHookState>();
    if state.is_null() {
        return None;
    }
    unsafe { (*state).error.take() }
}

type WalHook = Box<dyn FnMut(&Wal, c_int) -> Result<()> + Send>;

/// The wal hook registered on a connection.
struct WalHookState {
    hook: WalHook,
    /// The reason the hook rejected the most recent notification, to be
    /// reported by the call that observes SQLite's generic `SQLITE_ERROR`
    /// (the notification happens after the commit, so SQLite cannot
    /// attribute the failure more precisely). Reset on every hook
    /// invocation, so a stale reason can never be attributed to a later
    /// failure.
    error: Option<Error>,
}

unsafe extern "C" fn wal_hook_callback(
    client_data: *mut c_void,
    db: *mut ffi::sqlite3,
    db_name: *const c_char,
    pages: c_int,
) -> c_int {
    unsafe {
        let state: *mut WalHookState = client_data.cast();
        let wal = Wal { db, db_name };
        (*state).error = None;
        let r = catch_unwind(AssertUnwindSafe(|| ((*state).hook)(&wal, pages)));
        let error = match r {
            Ok(Ok(())) => return ffi::SQLITE_OK,
            Ok(Err(err)) => err,
            Err(_) => Error::UnwindingPanic,
        };
        (*state).error = Some(error);
        ffi::SQLITE_ERROR
    }
}

/// If `code` is the generic `SQLITE_ERROR` that SQLite produces when the wal
/// hook on `db` failed and the currently registered hook provided a reason,
/// take and return it. The reason is consumed, so it is reported at most
/// once, by the call that triggered the notification.
pub(crate) unsafe fn take_wal_hook_error(db: *mut ffi::sqlite3, code: c_int) -> Option<Error> {
    if db.is_null() || code != ffi::SQLITE_ERROR {
        return None;
    }
    let state = unsafe { ffi::sqlite3_get_clientdata(db, c"sqlite3_wal_hook".as_ptr()) }
        .cast::<WalHookState>();
    if state.is_null() {
        return None;
    }
    unsafe { (*state).error.take() }
}

impl InnerConnection {
    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.commit_hook(Some(|| {
    ///             called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///             true
    ///         }));
    ///     }
    ///     assert!(db
    ///         .execute_batch(
    ///             "BEGIN;
    ///         CREATE TABLE foo (t TEXT);
    ///         COMMIT;",
    ///         )
    ///         .is_err());
    ///     Ok(())
    /// }
    /// ```
    fn commit_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        self.set_commit_hook(hook.map(|hook| CommitHook::Simple(Box::new(hook))))
    }

    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.try_commit_hook(Some(|| {
    ///             called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///             Ok(true)
    ///         }));
    ///     }
    ///     assert!(db
    ///         .execute_batch(
    ///             "BEGIN;
    ///         CREATE TABLE foo (t TEXT);
    ///         COMMIT;",
    ///         )
    ///         .is_err());
    ///     Ok(())
    /// }
    /// ```
    fn try_commit_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() -> Result<bool> + Send + 'static,
    {
        self.set_commit_hook(hook.map(|hook| CommitHook::Fallible(Box::new(hook))))
    }

    fn set_commit_hook(&mut self, hook: Option<CommitHook>) -> Result<()> {
        let state = hook.map(|hook| CommitHookState { hook, error: None });
        let x = state.as_ref().map(|_| commit_hook_callback as _);
        self.set_clientdata(c"sqlite3_commit_hook", state, |db, bh| unsafe {
            ffi::sqlite3_commit_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        Ok(())
    }

    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.rollback_hook(Some(|| {
    ///             called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///         }));
    ///     }
    ///     assert!(db
    ///         .execute_batch(
    ///             "BEGIN;
    ///         CREATE TABLE foo (t TEXT);
    ///         ROLLBACK;",
    ///         )
    ///         .is_err());
    ///     Ok(())
    /// }
    /// ```
    fn rollback_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() + Send + 'static,
    {
        unsafe extern "C" fn call_boxed_closure<F>(p_arg: *mut c_void)
        where
            F: FnMut(),
        {
            unsafe {
                drop(catch_unwind(|| {
                    let boxed_hook: *mut F = p_arg.cast::<F>();
                    (*boxed_hook)();
                }));
            }
        }

        let x = hook.as_ref().map(|_| call_boxed_closure::<F> as _);
        self.set_clientdata(c"sqlite3_rollback_hook", hook, |db, bh| unsafe {
            ffi::sqlite3_rollback_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        Ok(())
    }

    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.update_hook(Some(|_, _: &str, _: &str, _| {
    ///             called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///         }));
    ///     }
    ///     db.execute_batch("CREATE TABLE foo AS SELECT 1 AS bar;")
    /// }
    /// ```
    fn update_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut(Action, &str, &str, i64) + Send + 'static,
    {
        unsafe extern "C" fn call_boxed_closure<F>(
            p_arg: *mut c_void,
            action_code: c_int,
            p_db_name: *const c_char,
            p_table_name: *const c_char,
            row_id: i64,
        ) where
            F: FnMut(Action, &str, &str, i64),
        {
            let action = Action::from(action_code);
            unsafe {
                drop(catch_unwind(|| {
                    let boxed_hook: *mut F = p_arg.cast::<F>();
                    (*boxed_hook)(
                        action,
                        expect_utf8(p_db_name, "database name"),
                        expect_utf8(p_table_name, "table name"),
                        row_id,
                    );
                }));
            }
        }

        let x = hook.as_ref().map(|_| call_boxed_closure::<F> as _);
        self.set_clientdata(c"sqlite3_update_hook", hook, |db, bh| unsafe {
            ffi::sqlite3_update_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        Ok(())
    }

    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.progress_handler(
    ///             1,
    ///             Some(|| {
    ///                 called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///                 true
    ///             }),
    ///         );
    ///     }
    ///     assert!(db
    ///         .execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")
    ///         .is_err());
    ///     Ok(())
    /// }
    /// ```
    fn progress_handler<F>(&mut self, num_ops: c_int, handler: Option<F>) -> Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        unsafe extern "C" fn call_boxed_closure<F>(p_arg: *mut c_void) -> c_int
        where
            F: FnMut() -> bool,
        {
            unsafe {
                let r = catch_unwind(|| {
                    let boxed_handler: *mut F = p_arg.cast::<F>();
                    (*boxed_handler)()
                });
                c_int::from(r.unwrap_or_default())
            }
        }

        let x = handler.as_ref().map(|_| call_boxed_closure::<F> as _);
        self.set_clientdata(c"sqlite3_progress_handler", handler, |db, bh| unsafe {
            ffi::sqlite3_progress_handler(db, num_ops, x, bh);
            ffi::SQLITE_OK
        })?;
        Ok(())
    }

    /// ```compile_fail
    /// use rusqlite::{Connection, Result};
    /// fn main() -> Result<()> {
    ///     let mut db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.authorizer(Some(|_: rusqlite::hooks::AuthContext<'_>| {
    ///             called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///             rusqlite::hooks::Authorization::Deny
    ///         }));
    ///     }
    ///     assert!(db
    ///         .execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")
    ///         .is_err());
    ///     Ok(())
    /// }
    /// ```
    fn authorizer<'c, F>(&'c mut self, authorizer: Option<F>) -> Result<()>
    where
        F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
    {
        unsafe extern "C" fn call_boxed_closure<'c, F>(
            p_arg: *mut c_void,
            action_code: c_int,
            param1: *const c_char,
            param2: *const c_char,
            db_name: *const c_char,
            trigger_or_view_name: *const c_char,
        ) -> c_int
        where
            F: FnMut(AuthContext<'c>) -> Authorization + Send + 'static,
        {
            unsafe {
                catch_unwind(|| {
                    let action = AuthAction::from_raw(
                        action_code,
                        expect_optional_utf8(param1, "authorizer param 1"),
                        expect_optional_utf8(param2, "authorizer param 2"),
                    );
                    let auth_ctx = AuthContext {
                        action,
                        database_name: expect_optional_utf8(db_name, "database name"),
                        accessor: expect_optional_utf8(
                            trigger_or_view_name,
                            "accessor (inner-most trigger or view)",
                        ),
                    };
                    let boxed_hook: *mut F = p_arg.cast::<F>();
                    (*boxed_hook)(auth_ctx)
                })
                .map_or_else(|_| ffi::SQLITE_ERROR, Authorization::into_raw)
            }
        }

        let x_auth = authorizer
            .as_ref()
            .map(|_| call_boxed_closure::<'c, F> as _);
        self.set_clientdata(c"sqlite3_set_authorizer", authorizer, |db, bh| unsafe {
            ffi::sqlite3_set_authorizer(db, x_auth, bh)
        })?;
        Ok(())
    }
}

unsafe fn expect_utf8<'a>(p_str: *const c_char, description: &'static str) -> &'a str {
    unsafe {
        expect_optional_utf8(p_str, description)
            .unwrap_or_else(|| panic!("received empty {description}"))
    }
}

unsafe fn expect_optional_utf8<'a>(
    p_str: *const c_char,
    description: &'static str,
) -> Option<&'a str> {
    if p_str.is_null() {
        return None;
    }
    unsafe {
        CStr::from_ptr(p_str)
            .to_str()
            .unwrap_or_else(|_| panic!("received non-utf8 string as {description}"))
            .into()
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{Action, Wal};
    use crate::{Connection, DropBehavior, Error, MAIN_DB, Result};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn test_commit_hook() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.commit_hook(Some(|| {
            CALLED.store(true, Ordering::Relaxed);
            false
        }))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_fn_commit_hook() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        fn hook() -> bool {
            true
        }

        db.commit_hook(Some(hook))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")
            .unwrap_err();
        Ok(())
    }

    #[test]
    fn test_rollback_hook() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.rollback_hook(Some(|| {
            CALLED.store(true, Ordering::Relaxed);
        }))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); ROLLBACK;")?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_update_hook() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.update_hook(Some(|action, db: &str, tbl: &str, row_id| {
            assert_eq!(Action::SQLITE_INSERT, action);
            assert_eq!("main", db);
            assert_eq!("foo", tbl);
            assert_eq!(1, row_id);
            CALLED.store(true, Ordering::Relaxed);
        }))?;
        db.execute_batch("CREATE TABLE foo (t TEXT)")?;
        db.execute_batch("INSERT INTO foo VALUES ('lisa')")?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_progress_handler() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.progress_handler(
            1,
            Some(|| {
                CALLED.store(true, Ordering::Relaxed);
                false
            }),
        )?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn test_progress_handler_interrupt() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        fn handler() -> bool {
            true
        }

        db.progress_handler(1, Some(handler))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")
            .unwrap_err();
        Ok(())
    }

    #[test]
    fn test_authorizer() -> Result<()> {
        use super::{AuthAction, AuthContext, Authorization};

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (public TEXT, private TEXT)")?;

        let authorizer = move |ctx: AuthContext<'_>| match ctx.action {
            AuthAction::Read {
                column_name: "private",
                ..
            } => Authorization::Ignore,
            AuthAction::DropTable { .. } => Authorization::Deny,
            AuthAction::Pragma { .. } => panic!("shouldn't be called"),
            _ => Authorization::Allow,
        };

        db.authorizer(Some(authorizer))?;
        db.execute_batch(
            "BEGIN TRANSACTION; INSERT INTO foo VALUES ('pub txt', 'priv txt'); COMMIT;",
        )?;
        db.query_row_and_then("SELECT * FROM foo", [], |row| -> Result<()> {
            assert_eq!(row.get::<_, String>("public")?, "pub txt");
            assert!(row.get::<_, Option<String>>("private")?.is_none());
            Ok(())
        })?;
        db.execute_batch("DROP TABLE foo").unwrap_err();

        db.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
        db.execute_batch("PRAGMA user_version=1")?; // Disallowed by first authorizer, but it's now removed.

        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn wal_hook() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("wal-hook.db3");

        let mut db = Connection::open(&path)?;
        let journal_mode: String =
            db.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
        assert_eq!(journal_mode, "wal");

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.wal_hook(Some(|wal: &'_ Wal, pages| {
            assert_eq!(wal.name(), MAIN_DB);
            assert!(pages > 0);
            CALLED.swap(true, Ordering::Relaxed);
            wal.checkpoint()
        }))?;
        db.execute_batch("CREATE TABLE x(c);")?;
        assert!(CALLED.load(Ordering::Relaxed));

        db.wal_hook(Some(|wal: &'_ Wal, pages| {
            assert!(pages > 0);
            let (log, ckpt) = wal.checkpoint_v2(super::CheckpointMode::TRUNCATE)?;
            assert_eq!(log, 0);
            assert_eq!(ckpt, 0);
            Ok(())
        }))?;
        db.execute_batch("CREATE TABLE y(c);")?;

        db.wal_hook(None::<fn(&Wal, std::ffi::c_int) -> Result<()>>)
    }

    fn wal_foo_table() -> (tempfile::TempDir, Connection) {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("wal-hook-test.db3");
        let db = Connection::open(&path).unwrap();
        let journal_mode: String = db
            .pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        db.execute_batch("CREATE TABLE foo (x INTEGER)").unwrap();
        (temp_dir, db)
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_error_propagates() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;

        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        // the notification happens after the commit: the write persists and
        // the connection is back in autocommit mode
        assert!(db.is_autocommit());
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_panic() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        db.wal_hook(Some(|_: &Wal, _| -> Result<()> { panic!("boom") }))?;

        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_eq!(Error::UnwindingPanic, err);
        assert!(db.is_autocommit());
        assert_eq!(1, foo_count(&db)?);

        // the hook is still registered; removing it restores normal operation
        db.wal_hook(None::<fn(&Wal, std::ffi::c_int) -> Result<()>>)?;
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_transaction_commit() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.wal_hook(Some(move |_: &Wal, _| {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let err = {
            let tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.commit().unwrap_err()
        };
        assert_custom_err(&err);
        // the notification ran exactly once: dropping the transaction
        // afterwards did not commit or notify again
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        // the commit had already happened when the notification fired
        assert_eq!(1, foo_count(&db)?);
        // the connection can start a new transaction
        db.transaction()?.commit()
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_finish_commit() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.wal_hook(Some(move |_: &Wal, _| {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        assert_custom_err(&err);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_savepoint_release() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;
        // a savepoint started in autocommit mode commits on release
        let err = {
            let mut sp = db.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (1)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
            sp.finish().unwrap_err()
        };
        assert_custom_err(&err);
        assert!(db.is_autocommit());
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_nested_savepoint() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.wal_hook(Some(move |_: &Wal, _| {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let mut tx = db.transaction()?;
        tx.execute("INSERT INTO foo VALUES (1)", [])?;
        {
            let sp = tx.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (2)", [])?;
            // releasing a nested savepoint must not fire the notification
            sp.commit()?;
        }
        assert_eq!(0, calls.load(Ordering::SeqCst));
        let err = tx.commit().unwrap_err();
        assert_custom_err(&err);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        // the whole transaction, including the savepoint's writes, persisted
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_execute_batch() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let commits = Arc::new(AtomicUsize::new(0));
        let commits2 = Arc::clone(&commits);
        db.wal_hook(Some(move |_: &Wal, _| {
            // allow the first autocommit statement, fail the second
            if commits2.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(custom_err())
            }
        }))?;
        let err = db
            .execute_batch(
                "INSERT INTO foo VALUES (1);
                 INSERT INTO foo VALUES (2);
                 INSERT INTO foo VALUES (3);",
            )
            .unwrap_err();
        assert_custom_err(&err);
        // the batch stopped after the failed notification; both statements
        // that ran had already committed and stay
        assert_eq!(2, foo_count(&db)?);
        assert_eq!(2, db.one_column::<i32, _>("SELECT MAX(x) FROM foo", [])?);
        assert!(db.is_autocommit());
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_returning() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;
        {
            let mut stmt = db.prepare("INSERT INTO foo VALUES (1) RETURNING x")?;
            let mut rows = stmt.query([])?;
            {
                // a row already returned is not a delivered notification
                let row = rows.next()?.expect("expected one row");
                assert_eq!(1, row.get::<_, i32>(0)?);
            }
            // iterating to the commit reports the notification failure
            let err = rows.next().unwrap_err();
            assert_custom_err(&err);
        }
        assert_eq!(1, foo_count(&db)?);
        assert!(db.is_autocommit());
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_drop_mid_iteration() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.wal_hook(Some(move |_: &Wal, _| {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(())
            }
        }))?;
        {
            let mut stmt = db.prepare("INSERT INTO foo VALUES (1) RETURNING x")?;
            let mut rows = stmt.query([])?;
            assert!(rows.next()?.is_some());
            // drop the iterator and statement without stepping to completion:
            // the commit happens during teardown, with no caller to report to
        }
        // the write was committed anyway and the connection is usable
        assert!(db.is_autocommit());
        // no error is left behind to surface in unrelated operations
        reject.store(false, Ordering::SeqCst);
        assert_eq!(1, foo_count(&db)?);
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_statement_reuse() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.wal_hook(Some(move |_: &Wal, _| {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(())
            }
        }))?;
        let mut stmt = db.prepare("INSERT INTO foo VALUES (?1)")?;
        let err = stmt.execute([1]).unwrap_err();
        assert_custom_err(&err);
        // the failed statement can be executed again
        reject.store(false, Ordering::SeqCst);
        stmt.execute([2])?;
        drop(stmt);
        // both writes persisted
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg(feature = "cache")]
    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_cached_statement() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.wal_hook(Some(move |_: &Wal, _| {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(())
            }
        }))?;
        {
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (?1)")?;
            let err = stmt.execute([1]).unwrap_err();
            assert_custom_err(&err);
            // returned to the cache while the hook still fails
        }
        reject.store(false, Ordering::SeqCst);
        {
            // the cached statement does not carry the old error
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (?1)")?;
            stmt.execute([2])?;
        }
        assert_eq!(2, foo_count(&db)?);
        assert_eq!(2, db.one_column::<i32, _>("SELECT MAX(x) FROM foo", [])?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_not_unregistered_by_failure() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let reject = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let reject = Arc::clone(&reject);
            let calls = Arc::clone(&calls);
            db.wal_hook(Some(move |_: &Wal, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                if reject.load(Ordering::SeqCst) {
                    Err(custom_err())
                } else {
                    Ok(())
                }
            }))?;
        }
        db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        reject.store(false, Ordering::SeqCst);
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, calls.load(Ordering::SeqCst));
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_consecutive_errors_are_distinct() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.wal_hook(Some(move |_: &Wal, _| {
            let n = calls2.fetch_add(1, Ordering::SeqCst);
            Err(Error::ToSqlConversionFailure(
                format!("wal error {n}").into(),
            ))
        }))?;
        let e1 = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        let e2 = db.execute("INSERT INTO foo VALUES (2)", []).unwrap_err();
        // each call gets the error of its own notification, exactly once
        assert!(e1.to_string().ends_with("wal error 0"));
        assert!(e2.to_string().ends_with("wal error 1"));
        assert_eq!(2, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_replace_unregister_override() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();

        // replacing the hook: only the new one runs
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;
        db.wal_hook(Some(|_: &Wal, _| Ok(())))?;
        db.execute("INSERT INTO foo VALUES (1)", [])?;

        // unregistering: no hook runs
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;
        db.wal_hook(None::<fn(&Wal, std::ffi::c_int) -> Result<()>>)?;
        db.execute("INSERT INTO foo VALUES (2)", [])?;

        // wal_autocheckpoint overwrites the registered hook
        db.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;
        db.pragma_update(None, "wal_autocheckpoint", 1000)?;
        db.execute("INSERT INTO foo VALUES (3)", [])?;

        assert_eq!(3, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_error_does_not_mask_sql_errors() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        db.execute_batch("CREATE UNIQUE INDEX ux ON foo(x)")?;
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.wal_hook(Some(move |_: &Wal, _| {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(())
            }
        }))?;
        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        reject.store(false, Ordering::SeqCst);
        // a genuine SQL error is reported as itself, not as a stale hook error
        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        match &err {
            Error::SqliteFailure(ffi_err, _) => {
                assert_eq!(crate::ErrorCode::ConstraintViolation, ffi_err.code);
            }
            other => panic!("expected SqliteFailure, got {other:?}"),
        }
        // a generic SQLITE_ERROR is not rewritten either
        db.execute_batch("BEGIN")?;
        let err = db.execute_batch("BEGIN").unwrap_err();
        match &err {
            Error::SqliteFailure(ffi_err, _) => {
                assert_eq!(crate::ErrorCode::Unknown, ffi_err.code);
            }
            other => panic!("expected SqliteFailure, got {other:?}"),
        }
        db.execute_batch("ROLLBACK")?;
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_commit_hook_rejection_still_rolls_back() -> Result<()> {
        let (_dir, mut db) = wal_foo_table();
        let wal_calls = Arc::new(AtomicUsize::new(0));
        let wal_calls2 = Arc::clone(&wal_calls);
        db.wal_hook(Some(move |_: &Wal, _| {
            wal_calls2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))?;
        db.try_commit_hook(Some(|| Err(custom_err())))?;
        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        // the commit hook rejected before the commit, so the write was
        // rolled back and no wal notification fired
        assert_eq!(0, wal_calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_wal_hook_other_connection() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("wal-hook-other.db3");

        let mut db1 = Connection::open(&path)?;
        let journal_mode: String =
            db1.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
        assert_eq!(journal_mode, "wal");
        db1.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let db2 = Connection::open(&path)?;

        db1.wal_hook(Some(|_: &Wal, _| Err(custom_err())))?;

        // failed notification on an autocommit write
        let err = db1.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        // the other connection sees the committed write
        assert_eq!(1, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);

        // failed notification on a transaction commit
        let err = {
            let tx = db1.transaction()?;
            tx.execute("INSERT INTO foo VALUES (2)", [])?;
            tx.commit().unwrap_err()
        };
        assert_custom_err(&err);
        assert!(db1.is_autocommit());
        assert_eq!(2, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);

        // the other connection is unaffected by db1's hook error
        db2.execute("INSERT INTO foo VALUES (3)", [])?;
        assert_eq!(3, db1.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_non_owning_hooks_cleanup() -> Result<()> {
        let mut conn = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        CALLED.store(false, Ordering::Relaxed);
        conn.commit_hook(Some(|| {
            CALLED.store(true, Ordering::Relaxed);
            false
        }))?;

        let non_owning_conn = unsafe { Connection::from_handle(conn.handle()) }?;
        drop(non_owning_conn);

        conn.execute_batch("CREATE TABLE test(value)")?;
        assert!(CALLED.load(Ordering::Relaxed));
        Ok(())
    }

    #[derive(Debug)]
    struct CommitRejected(&'static str);

    impl std::fmt::Display for CommitRejected {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for CommitRejected {}

    fn custom_err() -> Error {
        Error::ToSqlConversionFailure(Box::new(CommitRejected("commit rejected by hook")))
    }

    fn assert_custom_err(err: &Error) {
        match err {
            Error::ToSqlConversionFailure(e) => {
                let ce = e
                    .downcast_ref::<CommitRejected>()
                    .expect("inner error type must be preserved");
                assert_eq!("commit rejected by hook", ce.0);
            }
            other => panic!("expected the hook's error, got {other:?}"),
        }
    }

    fn foo_table() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        Ok(db)
    }

    fn foo_count(db: &Connection) -> Result<i32> {
        db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])
    }

    #[test]
    fn test_try_commit_hook_allow() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.try_commit_hook(Some(move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")?;
        assert_eq!(1, calls.load(Ordering::SeqCst));
        // an autocommit write is a commit too
        db.execute("INSERT INTO foo VALUES ('lisa')", [])?;
        assert_eq!(2, calls.load(Ordering::SeqCst));
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_true_matches_commit_hook() -> Result<()> {
        let mut db1 = foo_table()?;
        db1.commit_hook(Some(|| true))?;
        let mut db2 = foo_table()?;
        db2.try_commit_hook(Some(|| Ok(true)))?;

        let e1 = db1.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        let e2 = db2.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        // Ok(true) rejects exactly like the existing commit_hook
        assert_eq!(e1, e2);
        match &e2 {
            Error::SqliteFailure(ffi_err, _) => {
                assert_eq!(crate::ErrorCode::ConstraintViolation, ffi_err.code);
                assert_eq!(
                    crate::ffi::SQLITE_CONSTRAINT_COMMITHOOK,
                    ffi_err.extended_code
                );
            }
            other => panic!("expected SqliteFailure, got {other:?}"),
        }
        assert!(db2.is_autocommit());
        assert_eq!(0, foo_count(&db2)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_error_propagates() -> Result<()> {
        let mut db = foo_table()?;
        db.try_commit_hook(Some(|| Err(custom_err())))?;

        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        // the write was rolled back and the connection is usable again
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_panic() -> Result<()> {
        let mut db = foo_table()?;
        db.try_commit_hook(Some(|| -> Result<bool> { panic!("boom") }))?;

        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_eq!(Error::UnwindingPanic, err);
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);

        // the hook is still registered; removing it restores normal operation
        db.try_commit_hook(None::<fn() -> Result<bool>>)?;
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_not_unregistered_by_failure() -> Result<()> {
        let mut db = foo_table()?;
        let reject = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let reject = Arc::clone(&reject);
            let calls = Arc::clone(&calls);
            db.try_commit_hook(Some(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                if reject.load(Ordering::SeqCst) {
                    Err(custom_err())
                } else {
                    Ok(false)
                }
            }))?;
        }
        db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        reject.store(false, Ordering::SeqCst);
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, calls.load(Ordering::SeqCst));
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_execute_batch() -> Result<()> {
        let mut db = foo_table()?;
        let commits = Arc::new(AtomicUsize::new(0));
        let commits2 = Arc::clone(&commits);
        db.try_commit_hook(Some(move || {
            // allow the first autocommit statement, reject the second
            if commits2.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(false)
            } else {
                Err(custom_err())
            }
        }))?;
        let err = db
            .execute_batch(
                "INSERT INTO foo VALUES (1);
                 INSERT INTO foo VALUES (2);
                 INSERT INTO foo VALUES (3);",
            )
            .unwrap_err();
        assert_custom_err(&err);
        // the batch stopped at the rejected statement; the first statement's
        // own transaction was already committed and stays
        assert_eq!(1, foo_count(&db)?);
        assert_eq!(1, db.one_column::<i32, _>("SELECT MIN(x) FROM foo", [])?);
        assert!(db.is_autocommit());
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_transaction_commit() -> Result<()> {
        let mut db = foo_table()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.try_commit_hook(Some(move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let err = {
            let tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.commit().unwrap_err()
        };
        assert_custom_err(&err);
        // the check ran exactly once: dropping the transaction afterwards
        // did not commit or check again
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);
        // the connection can start a new transaction
        db.transaction()?.commit()
    }

    #[test]
    fn test_try_commit_hook_finish_commit() -> Result<()> {
        let mut db = foo_table()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.try_commit_hook(Some(move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        assert_custom_err(&err);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_savepoint_release() -> Result<()> {
        let mut db = foo_table()?;
        db.try_commit_hook(Some(|| Err(custom_err())))?;
        // a savepoint started in autocommit mode commits on release
        let err = {
            let mut sp = db.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (1)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
            sp.finish().unwrap_err()
        };
        assert_custom_err(&err);
        assert!(db.is_autocommit());
        assert_eq!(0, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_nested_savepoint() -> Result<()> {
        let mut db = foo_table()?;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        db.try_commit_hook(Some(move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            Err(custom_err())
        }))?;
        let mut tx = db.transaction()?;
        tx.execute("INSERT INTO foo VALUES (1)", [])?;
        {
            let sp = tx.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (2)", [])?;
            // releasing a nested savepoint must not run the check
            sp.commit()?;
        }
        assert_eq!(0, calls.load(Ordering::SeqCst));
        let err = tx.commit().unwrap_err();
        assert_custom_err(&err);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        // the released savepoint's writes are rolled back with the transaction
        assert_eq!(0, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_returning() -> Result<()> {
        let mut db = foo_table()?;
        db.try_commit_hook(Some(|| Err(custom_err())))?;
        {
            let mut stmt = db.prepare("INSERT INTO foo VALUES (1) RETURNING x")?;
            let mut rows = stmt.query([])?;
            {
                // a row already returned is not a committed write
                let row = rows.next()?.expect("expected one row");
                assert_eq!(1, row.get::<_, i32>(0)?);
            }
            // iterating to the end reports the check failure
            let err = rows.next().unwrap_err();
            assert_custom_err(&err);
        }
        assert_eq!(0, foo_count(&db)?);
        assert!(db.is_autocommit());
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_drop_mid_iteration() -> Result<()> {
        let mut db = foo_table()?;
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.try_commit_hook(Some(move || {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(false)
            }
        }))?;
        {
            let mut stmt = db.prepare("INSERT INTO foo VALUES (1) RETURNING x")?;
            let mut rows = stmt.query([])?;
            assert!(rows.next()?.is_some());
            // drop the iterator and statement without stepping to completion:
            // the commit attempted during teardown is rejected silently
        }
        // the write was rolled back...
        assert_eq!(0, foo_count(&db)?);
        assert!(db.is_autocommit());
        // ...and the swallowed error does not surface in unrelated operations
        reject.store(false, Ordering::SeqCst);
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_statement_reuse() -> Result<()> {
        let mut db = foo_table()?;
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.try_commit_hook(Some(move || {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(false)
            }
        }))?;
        let mut stmt = db.prepare("INSERT INTO foo VALUES (?1)")?;
        let err = stmt.execute([1]).unwrap_err();
        assert_custom_err(&err);
        // the failed statement can be executed again
        reject.store(false, Ordering::SeqCst);
        stmt.execute([1])?;
        drop(stmt);
        assert_eq!(1, foo_count(&db)?);
        Ok(())
    }

    #[cfg(feature = "cache")]
    #[test]
    fn test_try_commit_hook_cached_statement() -> Result<()> {
        let mut db = foo_table()?;
        let reject = Arc::new(AtomicBool::new(true));
        let reject2 = Arc::clone(&reject);
        db.try_commit_hook(Some(move || {
            if reject2.load(Ordering::SeqCst) {
                Err(custom_err())
            } else {
                Ok(false)
            }
        }))?;
        {
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (?1)")?;
            let err = stmt.execute([1]).unwrap_err();
            assert_custom_err(&err);
            // returned to the cache while the hook still rejects
        }
        reject.store(false, Ordering::SeqCst);
        {
            // the cached statement does not carry the old error
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (?1)")?;
            stmt.execute([2])?;
        }
        assert_eq!(1, foo_count(&db)?);
        assert_eq!(2, db.one_column::<i32, _>("SELECT MIN(x) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_commit_hooks_share_registration() -> Result<()> {
        let mut db = foo_table()?;

        // try_commit_hook replaces commit_hook
        db.commit_hook(Some(|| true))?;
        db.try_commit_hook(Some(|| Ok(false)))?;
        db.execute("INSERT INTO foo VALUES (1)", [])?;

        // commit_hook replaces try_commit_hook
        db.try_commit_hook(Some(|| Err(custom_err())))?;
        db.commit_hook(Some(|| false))?;
        db.execute("INSERT INTO foo VALUES (2)", [])?;

        // unregistering via commit_hook removes a try_commit_hook
        db.try_commit_hook(Some(|| Err(custom_err())))?;
        db.commit_hook(None::<fn() -> bool>)?;
        db.execute("INSERT INTO foo VALUES (3)", [])?;

        // unregistering via try_commit_hook removes a commit_hook
        db.commit_hook(Some(|| true))?;
        db.try_commit_hook(None::<fn() -> Result<bool>>)?;
        db.execute("INSERT INTO foo VALUES (4)", [])?;

        assert_eq!(4, foo_count(&db)?);
        Ok(())
    }

    #[test]
    fn test_commit_hook_callback_freed_exactly_once() -> Result<()> {
        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let mut db = Connection::open_in_memory()?;

        // replaced by another try_commit_hook
        db.try_commit_hook(Some({
            let counter = DropCounter(Arc::clone(&drops));
            move || -> Result<bool> {
                let _ = &counter;
                Ok(false)
            }
        }))?;
        assert_eq!(0, drops.load(Ordering::SeqCst));
        db.try_commit_hook(Some({
            let counter = DropCounter(Arc::clone(&drops));
            move || -> Result<bool> {
                let _ = &counter;
                Ok(false)
            }
        }))?;
        assert_eq!(1, drops.load(Ordering::SeqCst));

        // replaced by a commit_hook
        db.commit_hook(Some({
            let counter = DropCounter(Arc::clone(&drops));
            move || {
                let _ = &counter;
                false
            }
        }))?;
        assert_eq!(2, drops.load(Ordering::SeqCst));

        // unregistered via try_commit_hook
        db.try_commit_hook(None::<fn() -> Result<bool>>)?;
        assert_eq!(3, drops.load(Ordering::SeqCst));

        // freed when the connection is closed
        db.try_commit_hook(Some({
            let counter = DropCounter(Arc::clone(&drops));
            move || -> Result<bool> {
                let _ = &counter;
                Ok(false)
            }
        }))?;
        assert_eq!(3, drops.load(Ordering::SeqCst));
        drop(db);
        assert_eq!(4, drops.load(Ordering::SeqCst));
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_try_commit_hook_other_connection() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("try-commit-hook.db3");

        let mut db1 = Connection::open(&path)?;
        db1.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let db2 = Connection::open(&path)?;

        db1.try_commit_hook(Some(|| Err(custom_err())))?;

        // rejected autocommit write
        let err = db1.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_custom_err(&err);
        // the other connection never saw the rolled-back write
        assert_eq!(0, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);

        // rejected transaction commit
        let err = {
            let tx = db1.transaction()?;
            tx.execute("INSERT INTO foo VALUES (2)", [])?;
            tx.commit().unwrap_err()
        };
        assert_custom_err(&err);
        assert!(db1.is_autocommit());
        assert_eq!(0, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);

        // the other connection is unaffected by db1's hook error
        db2.execute("INSERT INTO foo VALUES (3)", [])?;
        assert_eq!(1, db1.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }
}
