//! Commit, Data Change and Rollback Notification Callbacks
#![expect(non_camel_case_types)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::catch_unwind;
use std::sync::{Arc, Mutex};

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
    #[inline]
    pub fn commit_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        self.db.borrow_mut().commit_hook(hook)
    }

    /// Register a callback function to be invoked whenever
    /// a transaction is committed, with the ability to fail the commit
    /// with a specific error.
    ///
    /// The callback returns:
    ///
    /// - `Ok(false)` to allow the commit,
    /// - `Ok(true)` to rollback, exactly like a [`commit_hook`](Connection::commit_hook)
    ///   callback returning `true`,
    /// - `Err(e)` to rollback and make the operation that triggered the
    ///   commit (e.g. [`Transaction::commit`](crate::Transaction::commit),
    ///   [`execute`](Connection::execute) of an auto-commit write, or
    ///   iterating an `INSERT ... RETURNING` to completion) fail with `e`,
    ///   preserved as-is.
    ///
    /// A panic unwinding out of the callback is caught before it can cross
    /// the SQLite call boundary; the commit is rolled back and the triggering
    /// operation fails with [`Error::UnwindingPanic`](crate::Error::UnwindingPanic).
    ///
    /// This shares a single registration with [`commit_hook`](Connection::commit_hook):
    /// registering either one replaces the other, and passing `None` to
    /// either unregisters the current hook. A rejection does not unregister
    /// the callback.
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
    pub fn wal_hook<F>(&mut self, hook: Option<F>) -> Result<()>
    where
        F: FnMut(&Wal, c_int) -> Result<()> + Send + 'static,
    {
        unsafe extern "C" fn wal_hook_callback<F>(
            client_data: *mut c_void,
            db: *mut ffi::sqlite3,
            db_name: *const c_char,
            pages: c_int,
        ) -> c_int
        where
            F: FnMut(&Wal, c_int) -> Result<()>,
        {
            unsafe {
                let wal = Wal { db, db_name };
                catch_unwind(|| {
                    let hook_fn: *mut F = client_data.cast::<F>();
                    match (*hook_fn)(&wal, pages) {
                        Ok(()) => ffi::SQLITE_OK,
                        Err(e) => e
                            .sqlite_error()
                            .map_or(ffi::SQLITE_ERROR, |x| x.extended_code),
                    }
                })
                .unwrap_or_default()
            }
        }
        let x = hook.as_ref().map(|_| wal_hook_callback::<F> as _);
        let mut c = self.db.borrow_mut();
        c.set_clientdata(c"sqlite3_wal_hook", hook, |db, bh| unsafe {
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

/// Registration payload for [`Connection::try_commit_hook`]: the user
/// callback plus the side channel used to pass the rejection reason back to
/// the connection, since the SQLite commit-hook callback can only return an
/// integer.
struct TryCommitHook<F> {
    hook: F,
    error: Arc<Mutex<Option<Error>>>,
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
        unsafe extern "C" fn call_boxed_closure<F>(p_arg: *mut c_void) -> c_int
        where
            F: FnMut() -> bool,
        {
            unsafe {
                let r = catch_unwind(|| {
                    let boxed_hook: *mut F = p_arg.cast::<F>();
                    (*boxed_hook)()
                });
                c_int::from(r.unwrap_or_default())
            }
        }
        let x = hook.as_ref().map(|_| call_boxed_closure::<F> as _);
        self.set_clientdata(c"sqlite3_commit_hook", hook, |db, bh| unsafe {
            ffi::sqlite3_commit_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        // The plain hook cannot report a reason; drop the side channel a
        // replaced `try_commit_hook` may have shared with this connection.
        self.set_commit_hook_error_slot(None);
        Ok(())
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
        unsafe extern "C" fn call_boxed_closure<F>(p_arg: *mut c_void) -> c_int
        where
            F: FnMut() -> Result<bool>,
        {
            unsafe {
                let r = catch_unwind(|| {
                    let state: *mut TryCommitHook<F> = p_arg.cast::<TryCommitHook<F>>();
                    ((*state).hook)()
                });
                let err = match r {
                    Ok(Ok(rollback)) => return c_int::from(rollback),
                    Ok(Err(err)) => err,
                    // Do not let the panic cross the SQLite call boundary.
                    Err(_) => Error::UnwindingPanic,
                };
                let state: *mut TryCommitHook<F> = p_arg.cast::<TryCommitHook<F>>();
                *(*state).error.lock().unwrap() = Some(err);
                1
            }
        }
        let error = Arc::new(Mutex::new(None));
        let registration = hook.map(|hook| TryCommitHook {
            hook,
            error: Arc::clone(&error),
        });
        let x = registration.as_ref().map(|_| call_boxed_closure::<F> as _);
        self.set_clientdata(c"sqlite3_commit_hook", registration, |db, bh| unsafe {
            ffi::sqlite3_commit_hook(db, x, bh);
            ffi::SQLITE_OK
        })?;
        self.set_commit_hook_error_slot(x.map(|_| error));
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
    use crate::{Connection, Error, MAIN_DB, Result};
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn test_try_commit_hook_allow() -> Result<()> {
        let mut db = Connection::open_in_memory()?;

        static CALLED: AtomicBool = AtomicBool::new(false);
        db.try_commit_hook(Some(|| -> Result<bool> {
            CALLED.store(true, Ordering::Relaxed);
            Ok(false)
        }))?;
        db.execute_batch("BEGIN; CREATE TABLE foo (t TEXT); COMMIT;")?;
        assert!(CALLED.load(Ordering::Relaxed));
        assert_eq!(
            1,
            db.one_column::<i64, _>("SELECT COUNT(*) FROM sqlite_master WHERE name = 'foo'", [])?
        );
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_deny_like_commit_hook() -> Result<()> {
        fn denied_with_old_hook() -> Error {
            let mut db = Connection::open_in_memory().unwrap();
            db.execute_batch("CREATE TABLE foo (t TEXT)").unwrap();
            db.commit_hook(Some(|| true)).unwrap();
            db.execute_batch("BEGIN; INSERT INTO foo VALUES ('x'); COMMIT;")
                .unwrap_err()
        }
        fn denied_with_try_hook() -> Error {
            let mut db = Connection::open_in_memory().unwrap();
            db.execute_batch("CREATE TABLE foo (t TEXT)").unwrap();
            db.try_commit_hook(Some(|| -> Result<bool> { Ok(true) }))
                .unwrap();
            db.execute_batch("BEGIN; INSERT INTO foo VALUES ('x'); COMMIT;")
                .unwrap_err()
        }
        // `Ok(true)` rejects exactly like a `commit_hook` returning `true`
        assert_eq!(denied_with_old_hook(), denied_with_try_hook());
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_error_reason() -> Result<()> {
        #[derive(Debug)]
        struct Custom(&'static str);
        impl std::fmt::Display for Custom {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Custom {}

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        db.try_commit_hook(Some(|| -> Result<bool> {
            Err(Error::ToSqlConversionFailure(Box::new(Custom("denied"))))
        }))?;

        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        // the exact error is reported: variant, content and the concrete
        // type of the contained error object are preserved
        match &err {
            Error::ToSqlConversionFailure(inner) => {
                let custom = inner.downcast_ref::<Custom>().expect("concrete type");
                assert_eq!("denied", custom.0);
            }
            _ => panic!("expected the hook's error, got {err:?}"),
        }
        // the write was rolled back and the connection is usable
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_panic() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        db.try_commit_hook(Some(|| -> Result<bool> { panic!("boom") }))?;
        let err = db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_eq!(Error::UnwindingPanic, err);
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_transaction_commit() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            db.try_commit_hook(Some(move || -> Result<bool> {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::InvalidParameterName("nope".to_owned()))
            }))?;
        }
        let err = {
            let tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.commit().unwrap_err()
        };
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        // the check ran exactly once: dropping the transaction afterwards did
        // not run it again
        assert_eq!(1, calls.load(Ordering::SeqCst));
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_finish_commit() -> Result<()> {
        use crate::transaction::DropBehavior;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        db.try_commit_hook(Some(|| -> Result<bool> {
            Err(Error::InvalidParameterName("nope".to_owned()))
        }))?;
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_drop_commit_no_leak() -> Result<()> {
        use crate::transaction::DropBehavior;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            // reject only the first commit attempted
            db.try_commit_hook(Some(move || -> Result<bool> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::InvalidParameterName("nope".to_owned()))
                } else {
                    Ok(false)
                }
            }))?;
        }
        {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO foo VALUES (1)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            // the commit is attempted (and rejected) while dropping, where no
            // error can be returned
        }
        // the writes were still rolled back
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        // the swallowed error does not pop out of the next unrelated
        // operation, and the hook is still registered
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, calls.load(Ordering::SeqCst));
        assert_eq!(1, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_savepoint_release() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        db.try_commit_hook(Some(|| -> Result<bool> {
            Err(Error::InvalidParameterName("nope".to_owned()))
        }))?;
        let err = {
            let sp = db.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (1)", [])?;
            // releasing the outermost savepoint commits the transaction
            sp.commit().unwrap_err()
        };
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_nested_savepoint() -> Result<()> {
        use crate::transaction::DropBehavior;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            db.try_commit_hook(Some(move || -> Result<bool> {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Error::InvalidParameterName("nope".to_owned()))
            }))?;
        }
        let mut tx = db.transaction()?;
        tx.execute("INSERT INTO foo VALUES (1)", [])?;
        {
            let mut sp = tx.savepoint()?;
            sp.execute("INSERT INTO foo VALUES (2)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
            // releasing a nested savepoint must not run the check
        }
        assert_eq!(0, calls.load(Ordering::SeqCst));
        let err = tx.commit().unwrap_err();
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        assert_eq!(1, calls.load(Ordering::SeqCst));
        // the writes made in the released savepoint are rolled back too
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_execute_batch_stops() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            // reject only the second commit
            db.try_commit_hook(Some(move || -> Result<bool> {
                if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                    Err(Error::InvalidParameterName("nope".to_owned()))
                } else {
                    Ok(false)
                }
            }))?;
        }
        let err = db
            .execute_batch("INSERT INTO foo VALUES (1); INSERT INTO foo VALUES (2); INSERT INTO foo VALUES (3);")
            .unwrap_err();
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        // the batch stopped at the rejection: the third statement never ran,
        // but the first, already committed transaction stays
        assert_eq!(2, calls.load(Ordering::SeqCst));
        assert_eq!(1, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_returning() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        db.try_commit_hook(Some(|| -> Result<bool> {
            Err(Error::InvalidParameterName("nope".to_owned()))
        }))?;
        let mut stmt = db.prepare("INSERT INTO foo VALUES (1) RETURNING x")?;
        let mut rows = stmt.query([])?;
        // a returned row is not a committed row
        assert!(rows.next()?.is_some());
        let err = rows.next().unwrap_err();
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        drop(rows);
        drop(stmt);
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_statement_reuse() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            // reject only the first commit
            db.try_commit_hook(Some(move || -> Result<bool> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::InvalidParameterName("nope".to_owned()))
                } else {
                    Ok(false)
                }
            }))?;
        }
        let mut stmt = db.prepare("INSERT INTO foo VALUES (1)")?;
        let err = stmt.execute([]).unwrap_err();
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        // the failed statement can be executed again
        stmt.execute([])?;
        drop(stmt);
        assert_eq!(1, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[cfg(feature = "cache")]
    #[test]
    fn test_try_commit_hook_cached_statement() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            // reject only the first commit
            db.try_commit_hook(Some(move || -> Result<bool> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::InvalidParameterName("nope".to_owned()))
                } else {
                    Ok(false)
                }
            }))?;
        }
        {
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (1)")?;
            let err = stmt.execute([]).unwrap_err();
            assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        }
        {
            // the statement handed back to the cache carries no old error
            let mut stmt = db.prepare_cached("INSERT INTO foo VALUES (1)")?;
            stmt.execute([])?;
        }
        assert_eq!(1, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
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

        db1.try_commit_hook(Some(|| -> Result<bool> {
            Err(Error::InvalidParameterName("nope".to_owned()))
        }))?;
        let err = db1.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        assert_eq!(Error::InvalidParameterName("nope".to_owned()), err);
        // the other connection never saw the rolled-back write and is not
        // affected by the error
        assert_eq!(0, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        db2.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(1, db2.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_try_commit_hook_not_unregistered_by_failure() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = Arc::clone(&calls);
            // reject only the first commit
            db.try_commit_hook(Some(move || -> Result<bool> {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(Error::InvalidParameterName("nope".to_owned()))
                } else {
                    Ok(false)
                }
            }))?;
        }
        db.execute("INSERT INTO foo VALUES (1)", []).unwrap_err();
        // a failed check does not unregister the callback
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(2, calls.load(Ordering::SeqCst));
        assert_eq!(1, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_commit_hooks_share_registration() -> Result<()> {
        use std::sync::atomic::AtomicUsize;

        static OLD_CALLS: AtomicUsize = AtomicUsize::new(0);
        static NEW_CALLS: AtomicUsize = AtomicUsize::new(0);

        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;

        // the newer registration replaces the older one, either way
        db.commit_hook(Some(|| {
            OLD_CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }))?;
        db.try_commit_hook(Some(|| -> Result<bool> {
            NEW_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }))?;
        db.execute("INSERT INTO foo VALUES (1)", [])?;
        assert_eq!(0, OLD_CALLS.load(Ordering::SeqCst));
        assert_eq!(1, NEW_CALLS.load(Ordering::SeqCst));

        db.commit_hook(Some(|| {
            OLD_CALLS.fetch_add(1, Ordering::SeqCst);
            false
        }))?;
        db.execute("INSERT INTO foo VALUES (2)", [])?;
        assert_eq!(1, OLD_CALLS.load(Ordering::SeqCst));
        assert_eq!(1, NEW_CALLS.load(Ordering::SeqCst));

        // either entry point unregisters the current hook
        db.try_commit_hook(None::<fn() -> Result<bool>>)?;
        db.execute("INSERT INTO foo VALUES (3)", [])?;
        assert_eq!(1, OLD_CALLS.load(Ordering::SeqCst));
        assert_eq!(1, NEW_CALLS.load(Ordering::SeqCst));

        db.try_commit_hook(Some(|| -> Result<bool> {
            NEW_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }))?;
        db.commit_hook(None::<fn() -> bool>)?;
        db.execute("INSERT INTO foo VALUES (4)", [])?;
        assert_eq!(1, OLD_CALLS.load(Ordering::SeqCst));
        assert_eq!(1, NEW_CALLS.load(Ordering::SeqCst));

        assert_eq!(4, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_commit_hooks_dropped_exactly_once() -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;

        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let mut db = Connection::open_in_memory()?;

        {
            let counter = DropCounter(Arc::clone(&drops));
            db.try_commit_hook(Some(move || -> Result<bool> {
                let _ = &counter;
                Ok(false)
            }))?;
        }
        // replaced by the other entry point: released exactly once
        {
            let counter = DropCounter(Arc::clone(&drops));
            db.commit_hook(Some(move || {
                let _ = &counter;
                false
            }))?;
            assert_eq!(1, drops.load(Ordering::SeqCst));
        }
        // unregistered through either entry point: released exactly once
        db.try_commit_hook(None::<fn() -> Result<bool>>)?;
        assert_eq!(2, drops.load(Ordering::SeqCst));

        // released exactly once when the connection is closed
        {
            let counter = DropCounter(Arc::clone(&drops));
            db.try_commit_hook(Some(move || -> Result<bool> {
                let _ = &counter;
                Ok(false)
            }))?;
        }
        assert_eq!(2, drops.load(Ordering::SeqCst));
        drop(db);
        assert_eq!(3, drops.load(Ordering::SeqCst));
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
}
