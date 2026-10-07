//! Create or redefine SQL functions.
//!
//! # Example
//!
//! Adding a `regexp` function to a connection in which compiled regular
//! expressions are cached in a `HashMap`. For an alternative implementation
//! that uses SQLite's [Function Auxiliary Data](https://www.sqlite.org/c3ref/get_auxdata.html) interface
//! to avoid recompiling regular expressions, see the unit tests for this
//! module.
//!
//! ```rust
//! use regex::Regex;
//! use rusqlite::functions::FunctionFlags;
//! use rusqlite::{Connection, Error, Result};
//! use std::sync::Arc;
//! type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
//!
//! fn add_regexp_function(db: &Connection) -> Result<()> {
//!     db.create_scalar_function(
//!         "regexp",
//!         2,
//!         FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
//!         move |ctx| {
//!             assert_eq!(ctx.len(), 2, "called with unexpected number of arguments");
//!             let regexp: Arc<Regex> = ctx.get_or_create_aux(0, |vr| -> Result<_, BoxError> {
//!                 Ok(Regex::new(vr.as_str()?)?)
//!             })?;
//!             let is_match = {
//!                 let text = ctx
//!                     .get_raw(1)
//!                     .as_str()
//!                     .map_err(|e| Error::UserFunctionError(e.into()))?;
//!
//!                 regexp.is_match(text)
//!             };
//!
//!             Ok(is_match)
//!         },
//!     )
//! }
//!
//! fn main() -> Result<()> {
//!     let db = Connection::open_in_memory()?;
//!     add_regexp_function(&db)?;
//!
//!     let is_match: bool =
//!         db.query_row("SELECT regexp('[aeiou]*', 'aaaaeeeiii')", [], |row| {
//!             row.get(0)
//!         })?;
//!
//!     assert!(is_match);
//!     Ok(())
//! }
//! ```
use std::any::Any;
use std::ffi::{CStr, c_int, c_uint, c_void};
use std::marker::PhantomData;
use std::ops::Deref;
use std::panic::{RefUnwindSafe, UnwindSafe, catch_unwind};
use std::ptr;
use std::slice;
use std::sync::Arc;

use crate::ffi::{self, sqlite3_context, sqlite3_value};

use crate::context::set_result;
use crate::types::{FromSql, FromSqlError, ToSql, ToSqlOutput, ValueRef};
use crate::util::free_boxed_value;
use crate::{Connection, Error, InnerConnection, Name, Result, str_to_cstring};

unsafe fn report_error(ctx: *mut sqlite3_context, err: &Error) {
    unsafe {
        if let Error::SqliteFailure(ref err, ref s) = *err {
            ffi::sqlite3_result_error_code(ctx, err.extended_code);
            if let Some(Ok(cstr)) = s.as_ref().map(|s| str_to_cstring(s)) {
                ffi::sqlite3_result_error(ctx, cstr.as_ptr(), -1);
            }
        } else {
            ffi::sqlite3_result_error_code(ctx, ffi::SQLITE_CONSTRAINT_FUNCTION);
            if let Ok(cstr) = str_to_cstring(&err.to_string()) {
                ffi::sqlite3_result_error(ctx, cstr.as_ptr(), -1);
            }
        }
    }
}

/// Name of the per-connection slot (see [`InnerConnection::set_clientdata`])
/// holding the original error reported by the most recent failure of a
/// scalar function registered with
/// [`Connection::try_create_scalar_function`]. The error cannot cross the
/// SQLite boundary itself, so the callback reports a generic SQLite error to
/// the engine and stashes the original one here; the first rusqlite call
/// that observes the SQLite error takes and returns the original instead.
const SCALAR_FN_ERROR_SLOT: &CStr = c"rusqlite_scalar_function_error";

/// The original error a scalar function registered with
/// [`Connection::try_create_scalar_function`] reported most recently, waiting
/// to be delivered to the database call that observes the failure.
struct ScalarFnErrorState {
    error: Option<Error>,
}

/// Stash the original error of a failing scalar function on the connection
/// `ctx` belongs to, to be picked up by [`take_scalar_function_error`] when
/// the database call that triggered the failure decodes SQLite's error.
unsafe fn stash_scalar_fn_error(ctx: *mut sqlite3_context, error: Error) {
    unsafe {
        let db = ffi::sqlite3_context_db_handle(ctx);
        if db.is_null() {
            return;
        }
        let state = ffi::sqlite3_get_clientdata(db, SCALAR_FN_ERROR_SLOT.as_ptr())
            .cast::<ScalarFnErrorState>();
        if state.is_null() {
            return;
        }
        (*state).error = Some(error);
    }
}

/// If a scalar function registered with
/// [`Connection::try_create_scalar_function`] on `db` reported an error that
/// has not been delivered yet, take and return it. The error is consumed, so
/// it is reported at most once, by the call that triggered the failure; a
/// failure that is never observed (e.g., the statement is finalized during
/// teardown) is discarded rather than attributed to a later, unrelated call.
pub(crate) unsafe fn take_scalar_function_error(
    db: *mut ffi::sqlite3,
    _code: c_int,
) -> Option<Error> {
    if db.is_null() {
        return None;
    }
    let state = unsafe { ffi::sqlite3_get_clientdata(db, SCALAR_FN_ERROR_SLOT.as_ptr()) }
        .cast::<ScalarFnErrorState>();
    if state.is_null() {
        return None;
    }
    unsafe { (*state).error.take() }
}

/// Context is a wrapper for the SQLite function
/// evaluation context.
pub struct Context<'a> {
    ctx: *mut sqlite3_context,
    args: &'a [*mut sqlite3_value],
}

impl Context<'_> {
    /// Returns the number of arguments to the function.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    /// Returns `true` when there is no argument.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.args.is_empty()
    }

    /// Returns the `idx`th argument as a `T`.
    ///
    /// # Failure
    ///
    /// Will panic if `idx` is greater than or equal to
    /// [`self.len()`](Context::len).
    ///
    /// Will return Err if the underlying SQLite type cannot be converted to a
    /// `T`.
    pub fn get<T: FromSql>(&self, idx: usize) -> Result<T> {
        let arg = self.args[idx];
        let value = unsafe { ValueRef::from_value(arg) };
        FromSql::column_result(value).map_err(|err| match err {
            FromSqlError::InvalidType => {
                Error::InvalidFunctionParameterType(idx, value.data_type())
            }
            FromSqlError::OutOfRange(i) => Error::IntegralValueOutOfRange(idx, i),
            FromSqlError::Utf8Error(err) => Error::Utf8Error(idx, err),
            FromSqlError::Other(err) => {
                Error::FromSqlConversionFailure(idx, value.data_type(), err)
            }
            FromSqlError::InvalidBlobSize { .. } => {
                Error::FromSqlConversionFailure(idx, value.data_type(), Box::new(err))
            }
        })
    }

    /// Return raw pointer at `idx`
    /// # Safety
    /// This function is unsafe because it uses raw pointer and cast
    #[cfg(feature = "pointer")]
    #[must_use]
    pub unsafe fn get_pointer<T: 'static>(
        &self,
        idx: usize,
        ptr_type: &'static std::ffi::CStr,
    ) -> Option<&T> {
        let arg = self.args[idx];
        debug_assert_eq!(unsafe { ffi::sqlite3_value_type(arg) }, ffi::SQLITE_NULL);
        unsafe {
            ffi::sqlite3_value_pointer(arg, ptr_type.as_ptr())
                .cast::<T>()
                .as_ref()
        }
    }

    /// Returns the `idx`th argument as a `ValueRef`.
    ///
    /// # Failure
    ///
    /// Will panic if `idx` is greater than or equal to
    /// [`self.len()`](Context::len).
    #[inline]
    #[must_use]
    pub fn get_raw(&self, idx: usize) -> ValueRef<'_> {
        let arg = self.args[idx];
        unsafe { ValueRef::from_value(arg) }
    }

    /// Returns the `idx`th argument as a `SqlFnArg`.
    /// To be used when the SQL function result is one of its arguments.
    #[inline]
    #[must_use]
    pub fn get_arg(&self, idx: usize) -> SqlFnArg {
        assert!(idx < self.len());
        SqlFnArg { idx }
    }

    /// Returns the subtype of `idx`th argument.
    ///
    /// # Failure
    ///
    /// Will panic if `idx` is greater than or equal to
    /// [`self.len()`](Context::len).
    #[must_use]
    pub fn get_subtype(&self, idx: usize) -> c_uint {
        let arg = self.args[idx];
        unsafe { ffi::sqlite3_value_subtype(arg) }
    }

    /// Fetch or insert the auxiliary data associated with a particular
    /// parameter. This is intended to be an easier-to-use way of fetching it
    /// compared to calling [`get_aux`](Context::get_aux) and
    /// [`set_aux`](Context::set_aux) separately.
    ///
    /// See `https://www.sqlite.org/c3ref/get_auxdata.html` for a discussion of
    /// this feature, or the unit tests of this module for an example.
    ///
    /// # Failure
    ///
    /// Will panic if `arg` is greater than or equal to
    /// [`self.len()`](Context::len).
    pub fn get_or_create_aux<T, E, F>(&self, arg: c_int, func: F) -> Result<Arc<T>>
    where
        T: Send + Sync + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
        F: FnOnce(ValueRef<'_>) -> Result<T, E>,
    {
        if let Some(v) = self.get_aux(arg)? {
            Ok(v)
        } else {
            let vr = self.get_raw(arg as usize);
            self.set_aux(
                arg,
                func(vr).map_err(|e| Error::UserFunctionError(e.into()))?,
            )
        }
    }

    /// Sets the auxiliary data associated with a particular parameter. See
    /// `https://www.sqlite.org/c3ref/get_auxdata.html` for a discussion of
    /// this feature, or the unit tests of this module for an example.
    ///
    /// # Failure
    ///
    /// Will panic if `arg` is greater than or equal to
    /// [`self.len()`](Context::len).
    pub fn set_aux<T: Send + Sync + 'static>(&self, arg: c_int, value: T) -> Result<Arc<T>> {
        assert!(arg < self.len() as i32);
        let orig: Arc<T> = Arc::new(value);
        let inner: AuxInner = orig.clone();
        let outer = Box::new(inner);
        let raw: *mut AuxInner = Box::into_raw(outer);
        unsafe {
            ffi::sqlite3_set_auxdata(
                self.ctx,
                arg,
                raw.cast(),
                Some(free_boxed_value::<AuxInner>),
            );
        };
        Ok(orig)
    }

    /// Gets the auxiliary data that was associated with a given parameter via
    /// [`set_aux`](Context::set_aux). Returns `Ok(None)` if no data has been
    /// associated, and Ok(Some(v)) if it has. Returns an error if the
    /// requested type does not match.
    ///
    /// # Failure
    ///
    /// Will panic if `arg` is greater than or equal to
    /// [`self.len()`](Context::len).
    pub fn get_aux<T: Send + Sync + 'static>(&self, arg: c_int) -> Result<Option<Arc<T>>> {
        assert!(arg < self.len() as i32);
        let p = unsafe { ffi::sqlite3_get_auxdata(self.ctx, arg) as *const AuxInner };
        if p.is_null() {
            Ok(None)
        } else {
            let v: AuxInner = AuxInner::clone(unsafe { &*p });
            v.downcast::<T>()
                .map(Some)
                .map_err(|_| Error::GetAuxWrongType)
        }
    }

    /// Get the db connection handle via [sqlite3_context_db_handle](https://www.sqlite.org/c3ref/context_db_handle.html)
    ///
    /// # Safety
    ///
    /// This function is marked unsafe because there is a potential for other
    /// references to the connection to be sent across threads, [see this comment](https://github.com/rusqlite/rusqlite/issues/643#issuecomment-640181213).
    pub unsafe fn get_connection(&self) -> Result<ConnectionRef<'_>> {
        unsafe {
            let handle = ffi::sqlite3_context_db_handle(self.ctx);
            Ok(ConnectionRef {
                conn: Connection::from_handle(handle)?,
                phantom: PhantomData,
            })
        }
    }
}

/// A reference to a connection handle with a lifetime bound to something.
pub struct ConnectionRef<'ctx> {
    // comes from Connection::from_handle(sqlite3_context_db_handle(...))
    // and is non-owning
    conn: Connection,
    phantom: PhantomData<&'ctx Context<'ctx>>,
}

impl Deref for ConnectionRef<'_> {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

type AuxInner = Arc<dyn Any + Send + Sync + 'static>;

/// Subtype of an SQL function
pub type SubType = Option<c_uint>;

/// Result of an SQL function
pub trait SqlFnOutput {
    /// Converts Rust value to SQLite value with an optional subtype
    fn to_sql(&self) -> Result<(ToSqlOutput<'_>, SubType)>;
}

impl<T: ToSql> SqlFnOutput for T {
    #[inline]
    fn to_sql(&self) -> Result<(ToSqlOutput<'_>, SubType)> {
        ToSql::to_sql(self).map(|o| (o, None))
    }
}

impl<T: ToSql> SqlFnOutput for (T, SubType) {
    fn to_sql(&self) -> Result<(ToSqlOutput<'_>, SubType)> {
        ToSql::to_sql(&self.0).map(|o| (o, self.1))
    }
}

/// n-th arg of an SQL scalar function
pub struct SqlFnArg {
    idx: usize,
}
impl ToSql for SqlFnArg {
    fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Arg(self.idx))
    }
}

unsafe fn sql_result<T: SqlFnOutput>(
    ctx: *mut sqlite3_context,
    args: &[*mut sqlite3_value],
    r: Result<T>,
) {
    unsafe {
        if let Err(err) = _sql_result(ctx, args, r) {
            report_error(ctx, &err);
        }
    }
}

unsafe fn _sql_result<T: SqlFnOutput>(
    ctx: *mut sqlite3_context,
    args: &[*mut sqlite3_value],
    r: Result<T>,
) -> Result<()> {
    let r = r?;
    let (value, sub_type) = r.to_sql()?;
    unsafe {
        set_result(ctx, args, value)?;
        if let Some(sub_type) = sub_type {
            ffi::sqlite3_result_subtype(ctx, sub_type);
        }
    }
    Ok(())
}

/// Aggregate is the callback interface for user-defined
/// aggregate function.
///
/// `A` is the type of the aggregation context and `T` is the type of the final
/// result. Implementations should be stateless.
pub trait Aggregate<A, T>
where
    A: RefUnwindSafe + UnwindSafe,
    T: SqlFnOutput,
{
    /// Initializes the aggregation context. Will be called prior to the first
    /// call to [`step()`](Aggregate::step) to set up the context for an
    /// invocation of the function. (Note: `init()` will not be called if
    /// there are no rows.)
    fn init(&self, ctx: &mut Context<'_>) -> Result<A>;

    /// "step" function called once for each row in an aggregate group. May be
    /// called 0 times if there are no rows.
    fn step(&self, ctx: &mut Context<'_>, acc: &mut A) -> Result<()>;

    /// Computes and returns the final result. Will be called exactly once for
    /// each invocation of the function. If [`step()`](Aggregate::step) was
    /// called at least once, will be given `Some(A)` (the same `A` as was
    /// created by [`init`](Aggregate::init) and given to
    /// [`step`](Aggregate::step)); if [`step()`](Aggregate::step) was not
    /// called (because the function is running against 0 rows), will be
    /// given `None`.
    ///
    /// The passed context will have no arguments.
    fn finalize(&self, ctx: &mut Context<'_>, acc: Option<A>) -> Result<T>;
}

/// `WindowAggregate` is the callback interface for
/// user-defined aggregate window function.
#[cfg(feature = "window")]
pub trait WindowAggregate<A, T>: Aggregate<A, T>
where
    A: RefUnwindSafe + UnwindSafe,
    T: SqlFnOutput,
{
    /// Returns the current value of the aggregate. Unlike xFinal, the
    /// implementation should not delete any context.
    fn value(&self, acc: Option<&mut A>) -> Result<T>;

    /// Removes a row from the current window.
    fn inverse(&self, ctx: &mut Context<'_>, acc: &mut A) -> Result<()>;
}

bitflags::bitflags! {
    /// Function Flags.
    /// See [sqlite3_create_function](https://sqlite.org/c3ref/create_function.html)
    /// and [Function Flags](https://sqlite.org/c3ref/c_deterministic.html) for details.
    #[derive(Clone, Copy, Debug)]
    #[repr(C)]
    pub struct FunctionFlags: c_int {
        /// Specifies UTF-8 as the text encoding this SQL function prefers for its parameters.
        const SQLITE_UTF8     = ffi::SQLITE_UTF8;
        /// Specifies UTF-16 using little-endian byte order as the text encoding this SQL function prefers for its parameters.
        const SQLITE_UTF16LE  = ffi::SQLITE_UTF16LE;
        /// Specifies UTF-16 using big-endian byte order as the text encoding this SQL function prefers for its parameters.
        const SQLITE_UTF16BE  = ffi::SQLITE_UTF16BE;
        /// Specifies UTF-16 using native byte order as the text encoding this SQL function prefers for its parameters.
        const SQLITE_UTF16    = ffi::SQLITE_UTF16;
        /// Means that the function always gives the same output when the input parameters are the same.
        const SQLITE_DETERMINISTIC = ffi::SQLITE_DETERMINISTIC; // 3.8.3
        /// Means that the function may only be invoked from top-level SQL.
        const SQLITE_DIRECTONLY    = 0x0000_0008_0000; // 3.30.0
        /// Indicates to SQLite that a function may call `sqlite3_value_subtype()` to inspect the subtypes of its arguments.
        const SQLITE_SUBTYPE       = 0x0000_0010_0000; // 3.30.0
        /// Means that the function is unlikely to cause problems even if misused.
        const SQLITE_INNOCUOUS     = 0x0000_0020_0000; // 3.31.0
        /// Indicates to SQLite that a function might call `sqlite3_result_subtype()` to cause a subtype to be associated with its result.
        const SQLITE_RESULT_SUBTYPE     = 0x0000_0100_0000; // 3.45.0
        /// Indicates that the function is an aggregate that internally orders the values provided to the first argument.
        const SQLITE_SELFORDER1 = 0x0000_0200_0000; // 3.47.0
    }
}

impl Default for FunctionFlags {
    #[inline]
    fn default() -> Self {
        Self::SQLITE_UTF8
    }
}

impl Connection {
    /// Attach a user-defined scalar function to
    /// this database connection.
    ///
    /// `fn_name` is the name the function will be accessible from SQL.
    /// `n_arg` is the number of arguments to the function. Use `-1` for a
    /// variable number. If the function always returns the same value
    /// given the same input, `deterministic` should be `true`.
    ///
    /// The function will remain available until the connection is closed or
    /// until it is explicitly removed via
    /// [`remove_function`](Connection::remove_function).
    ///
    /// # Example
    ///
    /// ```rust
    /// # use rusqlite::{Connection, Result};
    /// # use rusqlite::functions::FunctionFlags;
    /// fn scalar_function_example(db: Connection) -> Result<()> {
    ///     db.create_scalar_function(
    ///         "halve",
    ///         1,
    ///         FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
    ///         |ctx| {
    ///             let value = ctx.get::<f64>(0)?;
    ///             Ok(value / 2f64)
    ///         },
    ///     )?;
    ///
    ///     let six_halved: f64 = db.query_row("SELECT halve(6)", [], |r| r.get(0))?;
    ///     assert_eq!(six_halved, 3f64);
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return Err if the function could not be attached to the connection.
    #[inline]
    pub fn create_scalar_function<F, N: Name, T>(
        &self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        x_func: F,
    ) -> Result<()>
    where
        F: Fn(&Context<'_>) -> Result<T> + Send + 'static,
        T: SqlFnOutput,
    {
        self.db
            .borrow_mut()
            .create_scalar_function(fn_name, n_arg, flags, x_func)
    }

    /// Attach a user-defined scalar function to this database connection,
    /// preserving the original error when the callback fails.
    ///
    /// This is a variant of
    /// [`create_scalar_function`](Connection::create_scalar_function): the
    /// function is registered under the same rules (name, number of
    /// arguments, flags, replacement and removal via
    /// [`remove_function`](Connection::remove_function) all behave the same,
    /// and a registration made through one entry point replaces or removes
    /// one made through the other), and normal return values, including
    /// subtypes, are propagated the same way.
    ///
    /// The difference is in error reporting. With
    /// [`create_scalar_function`](Connection::create_scalar_function), a
    /// callback failure is flattened into a SQLite error and the caller only
    /// sees [`Error::SqliteFailure`]. With this entry point, when the
    /// callback returns `Err(e)` — or when converting the returned value to
    /// a SQL value fails with `e` — the database call that triggered the
    /// function (a query, an [`execute`](Connection::execute), an
    /// [`execute_batch`](Connection::execute_batch), a row fetch, ...) fails
    /// with `e` itself, so the caller can match on the exact [`Error`] and,
    /// for [`Error::UserFunctionError`], recover the concrete error object
    /// and its attached data. If the callback or the value conversion
    /// panics, the triggering call fails with [`Error::UnwindingPanic`] and
    /// the process keeps running.
    ///
    /// The error is delivered to the call that triggered the failure only:
    /// it is reported at most once, is not leaked into unrelated statements
    /// or other connections, and does not mask genuine SQLite errors
    /// afterwards. A failure does not unregister the function, does not
    /// commit or roll back any surrounding transaction, and does not prevent
    /// the statement or the connection from being reused.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use rusqlite::{Connection, Error, Result};
    /// # use rusqlite::functions::FunctionFlags;
    /// fn fallible_scalar_function_example(db: Connection) -> Result<()> {
    ///     db.try_create_scalar_function(
    ///         "halve",
    ///         1,
    ///         FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
    ///         |ctx| {
    ///             let value = ctx.get::<f64>(0)?;
    ///             if value < 0f64 {
    ///                 return Err(Error::InvalidParameter("negative value".to_owned()));
    ///             }
    ///             Ok(value / 2f64)
    ///         },
    ///     )?;
    ///
    ///     let six_halved: f64 = db.query_row("SELECT halve(6)", [], |r| r.get(0))?;
    ///     assert_eq!(six_halved, 3f64);
    ///
    ///     let err = db
    ///         .query_row::<f64, _, _>("SELECT halve(-6)", [], |r| r.get(0))
    ///         .unwrap_err();
    ///     assert!(matches!(err, Error::InvalidParameter(_)), "{err:?}");
    ///     Ok(())
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return Err if the function could not be attached to the connection.
    #[inline]
    pub fn try_create_scalar_function<F, N: Name, T>(
        &self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        x_func: F,
    ) -> Result<()>
    where
        F: Fn(&Context<'_>) -> Result<T> + Send + 'static,
        T: SqlFnOutput,
    {
        self.db
            .borrow_mut()
            .try_create_scalar_function(fn_name, n_arg, flags, x_func)
    }

    /// Attach a user-defined scalar function to this database connection,
    /// preserving the original error when the callback fails.
    ///
    /// This is an alias of
    /// [`try_create_scalar_function`](Connection::try_create_scalar_function);
    /// see its documentation for the exact semantics.
    ///
    /// # Failure
    ///
    /// Will return Err if the function could not be attached to the connection.
    #[inline]
    pub fn create_scalar_function_with_error<F, N: Name, T>(
        &self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        x_func: F,
    ) -> Result<()>
    where
        F: Fn(&Context<'_>) -> Result<T> + Send + 'static,
        T: SqlFnOutput,
    {
        self.try_create_scalar_function(fn_name, n_arg, flags, x_func)
    }

    /// Attach a user-defined aggregate function to this
    /// database connection.
    ///
    /// # Failure
    ///
    /// Will return Err if the function could not be attached to the connection.
    #[inline]
    pub fn create_aggregate_function<A, D, N: Name, T>(
        &self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        aggr: D,
    ) -> Result<()>
    where
        A: RefUnwindSafe + UnwindSafe + Send,
        D: Aggregate<A, T> + Send + 'static,
        T: SqlFnOutput,
    {
        self.db
            .borrow_mut()
            .create_aggregate_function(fn_name, n_arg, flags, aggr)
    }

    /// Attach a user-defined aggregate window function to
    /// this database connection.
    ///
    /// See `https://sqlite.org/windowfunctions.html#udfwinfunc` for more
    /// information.
    #[cfg(feature = "window")]
    #[inline]
    pub fn create_window_function<A, N: Name, W, T>(
        &self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        aggr: W,
    ) -> Result<()>
    where
        A: RefUnwindSafe + UnwindSafe + Send,
        W: WindowAggregate<A, T> + Send + 'static,
        T: SqlFnOutput,
    {
        self.db
            .borrow_mut()
            .create_window_function(fn_name, n_arg, flags, aggr)
    }

    /// Removes a user-defined function from this
    /// database connection.
    ///
    /// `fn_name` and `n_arg` should match the name and number of arguments
    /// given to [`create_scalar_function`](Connection::create_scalar_function)
    /// or [`create_aggregate_function`](Connection::create_aggregate_function).
    ///
    /// # Failure
    ///
    /// Will return Err if the function could not be removed.
    #[inline]
    pub fn remove_function<N: Name>(&self, fn_name: N, n_arg: c_int) -> Result<()> {
        self.db.borrow_mut().remove_function(fn_name, n_arg)
    }
}

impl InnerConnection {
    /// ```compile_fail
    /// use rusqlite::{functions::FunctionFlags, Connection, Result};
    /// fn main() -> Result<()> {
    ///     let db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.create_scalar_function(
    ///             "test",
    ///             0,
    ///             FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
    ///             |_| {
    ///                 called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///                 Ok(true)
    ///             },
    ///         );
    ///     }
    ///     let result: Result<bool> = db.query_row("SELECT test()", [], |r| r.get(0));
    ///     assert!(result?);
    ///     Ok(())
    /// }
    /// ```
    fn create_scalar_function<F, N: Name, T>(
        &mut self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        x_func: F,
    ) -> Result<()>
    where
        F: Fn(&Context<'_>) -> Result<T> + Send + 'static,
        T: SqlFnOutput,
    {
        unsafe extern "C" fn call_boxed_closure<F, T>(
            ctx: *mut sqlite3_context,
            argc: c_int,
            argv: *mut *mut sqlite3_value,
        ) where
            F: Fn(&Context<'_>) -> Result<T>,
            T: SqlFnOutput,
        {
            unsafe {
                let args = slice::from_raw_parts(argv, argc as usize);
                let r = catch_unwind(|| {
                    let boxed_f: *const F = ffi::sqlite3_user_data(ctx).cast::<F>();
                    assert!(!boxed_f.is_null(), "Internal error - null function pointer");
                    let ctx = Context { ctx, args };
                    (*boxed_f)(&ctx)
                });
                let Ok(r) = r else {
                    report_error(ctx, &Error::UnwindingPanic);
                    return;
                };
                sql_result(ctx, args, r);
            }
        }

        let boxed_f: *mut F = Box::into_raw(Box::new(x_func));
        let c_name = fn_name.as_cstr()?;
        let r = unsafe {
            ffi::sqlite3_create_function_v2(
                self.db(),
                c_name.as_ptr(),
                n_arg,
                flags.bits(),
                boxed_f.cast::<c_void>(),
                Some(call_boxed_closure::<F, T>),
                None,
                None,
                Some(free_boxed_value::<F>),
            )
        };
        self.decode_result(r)
    }

    /// ```compile_fail
    /// use rusqlite::{functions::FunctionFlags, Connection, Result};
    /// fn main() -> Result<()> {
    ///     let db = Connection::open_in_memory()?;
    ///     {
    ///         let mut called = std::sync::atomic::AtomicBool::new(false);
    ///         db.try_create_scalar_function(
    ///             "test",
    ///             0,
    ///             FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
    ///             |_| {
    ///                 called.store(true, std::sync::atomic::Ordering::Relaxed);
    ///                 Ok(true)
    ///             },
    ///         );
    ///     }
    ///     let result: Result<bool> = db.query_row("SELECT test()", [], |r| r.get(0));
    ///     assert!(result?);
    ///     Ok(())
    /// }
    /// ```
    fn try_create_scalar_function<F, N: Name, T>(
        &mut self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        x_func: F,
    ) -> Result<()>
    where
        F: Fn(&Context<'_>) -> Result<T> + Send + 'static,
        T: SqlFnOutput,
    {
        unsafe extern "C" fn call_boxed_closure_with_error<F, T>(
            ctx: *mut sqlite3_context,
            argc: c_int,
            argv: *mut *mut sqlite3_value,
        ) where
            F: Fn(&Context<'_>) -> Result<T>,
            T: SqlFnOutput,
        {
            unsafe {
                let args = slice::from_raw_parts(argv, argc as usize);
                // The callback, the conversion of its return value to a SQL
                // value and the delivery of that value are all covered: a
                // failure of any of them reports the original error, and an
                // unwinding panic in any of them reports
                // `Error::UnwindingPanic`, without crossing the FFI boundary.
                let r = catch_unwind(|| -> Result<()> {
                    let boxed_f: *const F = ffi::sqlite3_user_data(ctx).cast::<F>();
                    assert!(
                        !boxed_f.is_null(),
                        "Internal error - null function pointer"
                    );
                    let f_ctx = Context { ctx, args };
                    let t = (*boxed_f)(&f_ctx)?;
                    let (value, sub_type) = t.to_sql()?;
                    set_result(ctx, args, value)?;
                    if let Some(sub_type) = sub_type {
                        ffi::sqlite3_result_subtype(ctx, sub_type);
                    }
                    Ok(())
                });
                let r = match r {
                    Ok(r) => r,
                    Err(_) => Err(Error::UnwindingPanic),
                };
                if let Err(err) = r {
                    // Report to SQLite exactly like `create_scalar_function`
                    // does, so the engine-level behavior (statement abort,
                    // transaction handling, error code and message) is
                    // unchanged, and additionally stash the original error
                    // for the call that will observe the failure.
                    report_error(ctx, &err);
                    stash_scalar_fn_error(ctx, err);
                }
            }
        }

        self.ensure_scalar_fn_error_slot()?;
        let boxed_f: *mut F = Box::into_raw(Box::new(x_func));
        let c_name = fn_name.as_cstr()?;
        let r = unsafe {
            ffi::sqlite3_create_function_v2(
                self.db(),
                c_name.as_ptr(),
                n_arg,
                flags.bits(),
                boxed_f.cast::<c_void>(),
                Some(call_boxed_closure_with_error::<F, T>),
                None,
                None,
                Some(free_boxed_value::<F>),
            )
        };
        self.decode_result(r)
    }

    /// Create the per-connection slot used to hand the original error of a
    /// failing `try_create_scalar_function` callback to the triggering call,
    /// unless it already exists.
    fn ensure_scalar_fn_error_slot(&mut self) -> Result<()> {
        let existing =
            unsafe { self.get_clientdata::<ScalarFnErrorState, _>(SCALAR_FN_ERROR_SLOT) }?;
        if existing.is_null() {
            self.set_clientdata(
                SCALAR_FN_ERROR_SLOT,
                Some(ScalarFnErrorState { error: None }),
                |_, _| ffi::SQLITE_OK,
            )?;
        }
        Ok(())
    }

    fn create_aggregate_function<A, D, N: Name, T>(
        &mut self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        aggr: D,
    ) -> Result<()>
    where
        A: RefUnwindSafe + UnwindSafe + Send,
        D: Aggregate<A, T> + Send + 'static,
        T: SqlFnOutput,
    {
        let boxed_aggr: *mut D = Box::into_raw(Box::new(aggr));
        let c_name = fn_name.as_cstr()?;
        let r = unsafe {
            ffi::sqlite3_create_function_v2(
                self.db(),
                c_name.as_ptr(),
                n_arg,
                flags.bits(),
                boxed_aggr.cast::<c_void>(),
                None,
                Some(call_boxed_step::<A, D, T>),
                Some(call_boxed_final::<A, D, T>),
                Some(free_boxed_value::<D>),
            )
        };
        self.decode_result(r)
    }

    #[cfg(feature = "window")]
    fn create_window_function<A, N: Name, W, T>(
        &mut self,
        fn_name: N,
        n_arg: c_int,
        flags: FunctionFlags,
        aggr: W,
    ) -> Result<()>
    where
        A: RefUnwindSafe + UnwindSafe + Send,
        W: WindowAggregate<A, T> + Send + 'static,
        T: SqlFnOutput,
    {
        let boxed_aggr: *mut W = Box::into_raw(Box::new(aggr));
        let c_name = fn_name.as_cstr()?;
        let r = unsafe {
            ffi::sqlite3_create_window_function(
                self.db(),
                c_name.as_ptr(),
                n_arg,
                flags.bits(),
                boxed_aggr.cast::<c_void>(),
                Some(call_boxed_step::<A, W, T>),
                Some(call_boxed_final::<A, W, T>),
                Some(call_boxed_value::<A, W, T>),
                Some(call_boxed_inverse::<A, W, T>),
                Some(free_boxed_value::<W>),
            )
        };
        self.decode_result(r)
    }

    fn remove_function<N: Name>(&mut self, fn_name: N, n_arg: c_int) -> Result<()> {
        let c_name = fn_name.as_cstr()?;
        let r = unsafe {
            ffi::sqlite3_create_function_v2(
                self.db(),
                c_name.as_ptr(),
                n_arg,
                ffi::SQLITE_UTF8,
                ptr::null_mut(),
                None,
                None,
                None,
                None,
            )
        };
        self.decode_result(r)
    }
}

unsafe fn aggregate_context<A>(ctx: *mut sqlite3_context, bytes: usize) -> Option<*mut *mut A> {
    let pac = unsafe { ffi::sqlite3_aggregate_context(ctx, bytes as c_int).cast::<*mut A>() };
    if pac.is_null() {
        return None;
    }
    Some(pac)
}

unsafe extern "C" fn call_boxed_step<A, D, T>(
    ctx: *mut sqlite3_context,
    argc: c_int,
    argv: *mut *mut sqlite3_value,
) where
    A: RefUnwindSafe + UnwindSafe,
    D: Aggregate<A, T>,
    T: SqlFnOutput,
{
    unsafe {
        let Some(pac) = aggregate_context::<A>(ctx, size_of::<*mut A>()) else {
            ffi::sqlite3_result_error_nomem(ctx);
            return;
        };

        let r = catch_unwind(|| {
            let boxed_aggr: *mut D = ffi::sqlite3_user_data(ctx).cast::<D>();
            assert!(
                !boxed_aggr.is_null(),
                "Internal error - null aggregate pointer"
            );
            let mut ctx = Context {
                ctx,
                args: slice::from_raw_parts(argv, argc as usize),
            };

            if (*pac).is_null() {
                *pac = Box::into_raw(Box::new((*boxed_aggr).init(&mut ctx)?));
            }

            (*boxed_aggr).step(&mut ctx, &mut **pac)
        });
        let Ok(r) = r else {
            report_error(ctx, &Error::UnwindingPanic);
            return;
        };
        match r {
            Ok(()) => {}
            Err(err) => report_error(ctx, &err),
        }
    }
}

#[cfg(feature = "window")]
unsafe extern "C" fn call_boxed_inverse<A, W, T>(
    ctx: *mut sqlite3_context,
    argc: c_int,
    argv: *mut *mut sqlite3_value,
) where
    A: RefUnwindSafe + UnwindSafe,
    W: WindowAggregate<A, T>,
    T: SqlFnOutput,
{
    unsafe {
        let Some(pac) = aggregate_context::<A>(ctx, size_of::<*mut A>()) else {
            ffi::sqlite3_result_error_nomem(ctx);
            return;
        };

        let r = catch_unwind(|| {
            let boxed_aggr: *mut W = ffi::sqlite3_user_data(ctx).cast::<W>();
            assert!(
                !boxed_aggr.is_null(),
                "Internal error - null aggregate pointer"
            );
            let mut ctx = Context {
                ctx,
                args: slice::from_raw_parts(argv, argc as usize),
            };
            (*boxed_aggr).inverse(&mut ctx, &mut **pac)
        });
        let Ok(r) = r else {
            report_error(ctx, &Error::UnwindingPanic);
            return;
        };
        match r {
            Ok(()) => {}
            Err(err) => report_error(ctx, &err),
        }
    }
}

unsafe extern "C" fn call_boxed_final<A, D, T>(ctx: *mut sqlite3_context)
where
    A: RefUnwindSafe + UnwindSafe,
    D: Aggregate<A, T>,
    T: SqlFnOutput,
{
    unsafe {
        // Within the xFinal callback, it is customary to set N=0 in calls to
        // sqlite3_aggregate_context(C,N) so that no pointless memory allocations occur.
        let a: Option<A> = match aggregate_context::<A>(ctx, 0) {
            Some(pac) => {
                if (*pac).is_null() {
                    None
                } else {
                    let a = Box::from_raw(*pac);
                    Some(*a)
                }
            }
            None => None,
        };

        let r = catch_unwind(|| {
            let boxed_aggr: *mut D = ffi::sqlite3_user_data(ctx).cast::<D>();
            assert!(
                !boxed_aggr.is_null(),
                "Internal error - null aggregate pointer"
            );
            let mut ctx = Context { ctx, args: &mut [] };
            (*boxed_aggr).finalize(&mut ctx, a)
        });
        let Ok(r) = r else {
            report_error(ctx, &Error::UnwindingPanic);
            return;
        };
        sql_result(ctx, &[], r);
    }
}

#[cfg(feature = "window")]
unsafe extern "C" fn call_boxed_value<A, W, T>(ctx: *mut sqlite3_context)
where
    A: RefUnwindSafe + UnwindSafe,
    W: WindowAggregate<A, T>,
    T: SqlFnOutput,
{
    unsafe {
        // Within the xValue callback, it is customary to set N=0 in calls to
        // sqlite3_aggregate_context(C,N) so that no pointless memory allocations occur.
        let pac = aggregate_context::<A>(ctx, 0).filter(|&pac| !(*pac).is_null());

        let r = catch_unwind(|| {
            let boxed_aggr: *mut W = ffi::sqlite3_user_data(ctx).cast::<W>();
            assert!(
                !boxed_aggr.is_null(),
                "Internal error - null aggregate pointer"
            );
            (*boxed_aggr).value(pac.map(|pac| &mut **pac))
        });
        let Ok(r) = r else {
            report_error(ctx, &Error::UnwindingPanic);
            return;
        };
        sql_result(ctx, &[], r);
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[cfg(feature = "window")]
    use crate::functions::WindowAggregate;
    use crate::functions::{Aggregate, Context, FunctionFlags, SqlFnArg, SubType};
    use crate::{Connection, Error, Result};
    use regex::Regex;
    use std::ffi::c_double;

    fn half(ctx: &Context<'_>) -> Result<c_double> {
        assert!(!ctx.is_empty());
        assert_eq!(ctx.len(), 1, "called with unexpected number of arguments");
        assert!(unsafe {
            ctx.get_connection()
                .as_ref()
                .map(std::ops::Deref::deref)
                .is_ok()
        });
        let value = ctx.get::<c_double>(0)?;
        Ok(value / 2f64)
    }

    #[test]
    fn test_function_half() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(
            c"half",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            half,
        )?;
        let result: f64 = db.one_column("SELECT half(6)", [])?;

        assert!((3f64 - result).abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn test_remove_function() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(
            c"half",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            half,
        )?;
        assert!((3f64 - db.one_column::<f64, _>("SELECT half(6)", [])?).abs() < f64::EPSILON);

        db.remove_function(c"half", 1)?;
        db.one_column::<f64, _>("SELECT half(6)", []).unwrap_err();
        Ok(())
    }

    // This implementation of a regexp scalar function uses SQLite's auxiliary data
    // (https://www.sqlite.org/c3ref/get_auxdata.html) to avoid recompiling the regular
    // expression multiple times within one query.
    fn regexp_with_auxiliary(ctx: &Context<'_>) -> Result<bool> {
        assert_eq!(ctx.len(), 2, "called with unexpected number of arguments");
        type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
        let regexp: std::sync::Arc<Regex> = ctx
            .get_or_create_aux(0, |vr| -> Result<_, BoxError> {
                Ok(Regex::new(vr.as_str()?)?)
            })?;

        let is_match = {
            let text = ctx
                .get_raw(1)
                .as_str()
                .map_err(|e| Error::UserFunctionError(e.into()))?;

            regexp.is_match(text)
        };

        Ok(is_match)
    }

    #[test]
    fn test_function_regexp_with_auxiliary() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "BEGIN;
             CREATE TABLE foo (x string);
             INSERT INTO foo VALUES ('lisa');
             INSERT INTO foo VALUES ('lXsi');
             INSERT INTO foo VALUES ('lisX');
             END;",
        )?;
        db.create_scalar_function(
            c"regexp",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            regexp_with_auxiliary,
        )?;

        assert!(db.one_column::<bool, _>("SELECT regexp('l.s[aeiouy]', 'lisa')", [])?);

        assert_eq!(
            2,
            db.one_column::<i64, _>(
                "SELECT COUNT(*) FROM foo WHERE regexp('l.s[aeiouy]', x) == 1",
                [],
            )?
        );
        Ok(())
    }

    #[test]
    fn test_varargs_function() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(
            c"my_concat",
            -1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let mut ret = String::new();

                for idx in 0..ctx.len() {
                    let s = ctx.get::<String>(idx)?;
                    ret.push_str(&s);
                }

                Ok(ret)
            },
        )?;

        for &(expected, query) in &[
            ("", "SELECT my_concat()"),
            ("onetwo", "SELECT my_concat('one', 'two')"),
            ("abc", "SELECT my_concat('a', 'b', 'c')"),
        ] {
            assert_eq!(expected, db.one_column::<String, _>(query, [])?);
        }
        Ok(())
    }

    #[test]
    fn test_get_aux_type_checking() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(c"example", 2, FunctionFlags::default(), |ctx| {
            if !ctx.get::<bool>(1)? {
                ctx.set_aux::<i64>(0, 100)?;
            } else {
                assert_eq!(ctx.get_aux::<String>(0), Err(Error::GetAuxWrongType));
                assert_eq!(*ctx.get_aux::<i64>(0)?.unwrap(), 100);
            }
            Ok(true)
        })?;

        let res: bool = db.query_row(
            "SELECT example(0, i) FROM (SELECT 0 as i UNION SELECT 1)",
            [],
            |r| r.get(0),
        )?;
        // Doesn't actually matter, we'll assert in the function if there's a problem.
        assert!(res);
        Ok(())
    }

    struct Sum;
    struct Count;

    impl Aggregate<i64, Option<i64>> for Sum {
        fn init(&self, _: &mut Context<'_>) -> Result<i64> {
            Ok(0)
        }

        fn step(&self, ctx: &mut Context<'_>, sum: &mut i64) -> Result<()> {
            *sum += ctx.get::<i64>(0)?;
            Ok(())
        }

        fn finalize(&self, _: &mut Context<'_>, sum: Option<i64>) -> Result<Option<i64>> {
            Ok(sum)
        }
    }

    impl Aggregate<i64, i64> for Count {
        fn init(&self, _: &mut Context<'_>) -> Result<i64> {
            Ok(0)
        }

        fn step(&self, _ctx: &mut Context<'_>, sum: &mut i64) -> Result<()> {
            *sum += 1;
            Ok(())
        }

        fn finalize(&self, _: &mut Context<'_>, sum: Option<i64>) -> Result<i64> {
            Ok(sum.unwrap_or(0))
        }
    }

    #[test]
    fn test_sum() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_aggregate_function(
            c"my_sum",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Sum,
        )?;

        // sum should return NULL when given no columns (contrast with count below)
        let no_result = "SELECT my_sum(i) FROM (SELECT 2 AS i WHERE 1 <> 1)";
        assert!(db.one_column::<Option<i64>, _>(no_result, [])?.is_none());

        let single_sum = "SELECT my_sum(i) FROM (SELECT 2 AS i UNION ALL SELECT 2)";
        assert_eq!(4, db.one_column::<i64, _>(single_sum, [])?);

        let dual_sum = "SELECT my_sum(i), my_sum(j) FROM (SELECT 2 AS i, 1 AS j UNION ALL SELECT \
                        2, 1)";
        let result: (i64, i64) = db.query_row(dual_sum, [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        assert_eq!((4, 2), result);
        Ok(())
    }

    #[test]
    fn test_count() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_aggregate_function(
            c"my_count",
            -1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Count,
        )?;

        // count should return 0 when given no columns (contrast with sum above)
        let no_result = "SELECT my_count(i) FROM (SELECT 2 AS i WHERE 1 <> 1)";
        assert_eq!(db.one_column::<i64, _>(no_result, [])?, 0);

        let single_sum = "SELECT my_count(i) FROM (SELECT 2 AS i UNION ALL SELECT 2)";
        assert_eq!(2, db.one_column::<i64, _>(single_sum, [])?);
        Ok(())
    }

    #[cfg(feature = "window")]
    impl WindowAggregate<i64, Option<i64>> for Sum {
        fn inverse(&self, ctx: &mut Context<'_>, sum: &mut i64) -> Result<()> {
            *sum -= ctx.get::<i64>(0)?;
            Ok(())
        }

        fn value(&self, sum: Option<&mut i64>) -> Result<Option<i64>> {
            Ok(sum.copied())
        }
    }

    #[test]
    #[cfg(feature = "window")]
    fn test_window() -> Result<()> {
        use fallible_iterator::FallibleIterator as _;

        let db = Connection::open_in_memory()?;
        db.create_window_function(
            c"sumint",
            1,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            Sum,
        )?;
        db.execute_batch(
            "CREATE TABLE t3(x, y);
             INSERT INTO t3 VALUES('a', 4),
                     ('b', 5),
                     ('c', 3),
                     ('d', 8),
                     ('e', 1);",
        )?;

        let mut stmt = db.prepare(
            "SELECT x, sumint(y) OVER (
                   ORDER BY x ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING
                 ) AS sum_y
                 FROM t3 ORDER BY x;",
        )?;

        let results: Vec<(String, i64)> = stmt
            .query([])?
            .map(|row| Ok((row.get("x")?, row.get("sum_y")?)))
            .collect()?;
        let expected = vec![
            ("a".to_owned(), 9),
            ("b".to_owned(), 12),
            ("c".to_owned(), 16),
            ("d".to_owned(), 12),
            ("e".to_owned(), 9),
        ];
        assert_eq!(expected, results);
        Ok(())
    }

    #[test]
    fn test_sub_type() -> Result<()> {
        fn test_getsubtype(ctx: &Context<'_>) -> Result<i32> {
            Ok(ctx.get_subtype(0) as i32)
        }
        fn test_setsubtype(ctx: &Context<'_>) -> Result<(SqlFnArg, SubType)> {
            use std::ffi::c_uint;
            let value = ctx.get_arg(0);
            let sub_type = ctx.get::<c_uint>(1)?;
            Ok((value, Some(sub_type)))
        }
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(
            c"test_getsubtype",
            1,
            FunctionFlags::SQLITE_UTF8,
            test_getsubtype,
        )?;
        db.create_scalar_function(
            c"test_setsubtype",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_RESULT_SUBTYPE,
            test_setsubtype,
        )?;
        let result: i32 = db.one_column("SELECT test_getsubtype('hello');", [])?;
        assert_eq!(0, result);

        let result: i32 =
            db.one_column("SELECT test_getsubtype(test_setsubtype('hello',123));", [])?;
        assert_eq!(123, result);

        Ok(())
    }

    #[test]
    fn test_blob() -> Result<()> {
        fn test_len(ctx: &Context<'_>) -> Result<u32> {
            let blob = ctx.get_raw(0);
            Ok(blob
                .as_bytes_or_null()?
                .map_or(0, |b| b.len().try_into().unwrap()))
        }
        let db = Connection::open_in_memory()?;
        db.create_scalar_function("test_len", 1, FunctionFlags::SQLITE_DETERMINISTIC, test_len)?;
        assert_eq!(
            6,
            db.one_column::<u32, _>("SELECT test_len(X'53514C697465');", [])?
        );
        assert_eq!(0, db.one_column::<u32, _>("SELECT test_len(X'');", [])?);
        assert_eq!(0, db.one_column::<u32, _>("SELECT test_len(NULL);", [])?);
        Ok(())
    }

    #[test]
    #[cfg(feature = "pointer")]
    fn test_rc_pointer() -> Result<()> {
        use crate::types::ToSqlOutput;
        use std::ops::Deref as _;
        use std::rc::Rc;

        const PTR_TYPE: &std::ffi::CStr = c"my_rust_ptr";
        let rc = Rc::new(1);
        {
            let ptr = ToSqlOutput::from_rc(rc.clone(), PTR_TYPE);
            assert_eq!(2, Rc::strong_count(&rc));
            fn myfunc(ctx: &Context<'_>) -> Result<ToSqlOutput<'static>> {
                let x = unsafe { ctx.get_pointer(0, PTR_TYPE) };
                assert_eq!(x, Some(&1));
                Ok(ToSqlOutput::from_rc(Rc::new(*x.unwrap()), PTR_TYPE))
            }
            let db = Connection::open_in_memory()?;
            db.create_scalar_function("myfunc", 1, FunctionFlags::SQLITE_DETERMINISTIC, myfunc)?;
            let mut stmt = db.prepare("SELECT myfunc(?)")?;
            let result = stmt.query_one([ptr], |r| {
                unsafe { r.get_pointer::<_, i32>(0, PTR_TYPE) }.map(|opt| opt.cloned())
            })?;
            assert_eq!(result.unwrap(), *rc.deref());
        }
        assert_eq!(1, Rc::strong_count(&rc));
        Ok(())
    }

    #[test]
    #[cfg(feature = "pointer")]
    fn test_box_pointer() -> Result<()> {
        use crate::types::ToSqlOutput;

        const PTR_TYPE: &std::ffi::CStr = c"my_rust_ptr";
        let value = 1;
        let ptr = ToSqlOutput::new_boxed(value, PTR_TYPE);
        fn myfunc(ctx: &Context<'_>) -> Result<ToSqlOutput<'static>> {
            let x = unsafe { ctx.get_pointer(0, PTR_TYPE) };
            assert_eq!(x, Some(&1));
            Ok(ToSqlOutput::new_boxed(*x.unwrap(), PTR_TYPE))
        }
        let db = Connection::open_in_memory()?;
        db.create_scalar_function("myfunc", 1, FunctionFlags::SQLITE_DETERMINISTIC, myfunc)?;
        let mut stmt = db.prepare("SELECT myfunc(?)")?;
        let result = stmt.query_one([ptr], |r| {
            unsafe { r.get_pointer::<_, i32>(0, PTR_TYPE) }.map(|opt| opt.cloned())
        })?;
        assert_eq!(result.unwrap(), value);
        Ok(())
    }

    mod try_scalar {
        use super::super::{FunctionFlags, SqlFnArg, SubType};
        use crate::types::{ToSql, ToSqlOutput};
        use crate::{Connection, Error, Result, ffi};
        use std::fmt;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        #[derive(Debug)]
        struct FnError {
            tag: u64,
            detail: String,
            payload: Vec<u8>,
        }

        impl fmt::Display for FnError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}: {}", self.tag, self.detail)
            }
        }

        impl std::error::Error for FnError {}

        fn fn_error(tag: u64) -> Error {
            Error::UserFunctionError(Box::new(FnError {
                tag,
                detail: format!("detail/{tag}"),
                payload: vec![0, 127, 255],
            }))
        }

        fn assert_fn_error(err: &Error, tag: u64) {
            match err {
                Error::UserFunctionError(inner) => {
                    let fe = inner
                        .downcast_ref::<FnError>()
                        .expect("original custom error type was lost");
                    assert_eq!(fe.tag, tag);
                    assert_eq!(fe.detail, format!("detail/{tag}"));
                    assert_eq!(fe.payload, vec![0, 127, 255]);
                }
                other => panic!("expected the callback's error {tag}, got {other:?}"),
            }
        }

        fn flags() -> FunctionFlags {
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC
        }

        /// A scalar function that fails with `fn_error(tag)` while `gate` is
        /// set, and otherwise returns its argument unchanged.
        fn gated_function(
            db: &Connection,
            name: &str,
            tag: u64,
            gate: Arc<AtomicBool>,
        ) -> Result<()> {
            db.try_create_scalar_function(name, 1, flags(), move |ctx| {
                let x = ctx.get::<i64>(0)?;
                if gate.load(Ordering::SeqCst) {
                    return Err(fn_error(tag));
                }
                Ok(x)
            })
        }

        #[test]
        fn success_values_subtypes_and_alias() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.try_create_scalar_function(c"halve2", 1, flags(), |ctx| {
                Ok(ctx.get::<f64>(0)? / 2f64)
            })?;
            assert_eq!(db.one_column::<f64, _>("SELECT halve2(6)", [])?, 3f64);

            // subtypes propagate like with `create_scalar_function`
            db.try_create_scalar_function(
                c"try_setsubtype",
                2,
                FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_RESULT_SUBTYPE,
                |ctx| {
                    let value: SqlFnArg = ctx.get_arg(0);
                    let sub_type: SubType = Some(ctx.get::<u32>(1)?);
                    Ok((value, sub_type))
                },
            )?;
            db.try_create_scalar_function(c"try_getsubtype", 1, FunctionFlags::SQLITE_UTF8, |ctx| {
                Ok(ctx.get_subtype(0) as i32)
            })?;
            let result: i32 =
                db.one_column("SELECT try_getsubtype(try_setsubtype('hello',123));", [])?;
            assert_eq!(123, result);

            // the alias registers the same kind of function
            db.create_scalar_function_with_error(c"alias_fail", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(5))
            })?;
            assert_fn_error(&db.one_column::<i64, _>("SELECT alias_fail()", []).unwrap_err(), 5);
            Ok(())
        }

        #[test]
        fn original_error_object_and_data_preserved() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.try_create_scalar_function(c"fail", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(41))
            })?;
            assert_fn_error(
                &db.query_row("SELECT fail()", [], |r| r.get::<_, i64>(0))
                    .unwrap_err(),
                41,
            );
            // the error was consumed: a genuine SQLite error that follows is
            // reported as itself, not as a leftover function error
            let err = db.prepare("SELEC 1").unwrap_err();
            assert!(matches!(err, Error::SqlInputError { .. }), "{err:?}");
            Ok(())
        }

        #[test]
        fn error_variants_round_trip() -> Result<()> {
            let db = Connection::open_in_memory()?;
            let cases = [
                Error::InvalidParameterName("fn-key".into()),
                Error::IntegralValueOutOfRange(6, -9_223_372_036_854_775_807),
                Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), Some("original busy".into())),
                Error::QueryReturnedNoRows,
                Error::InvalidParameterCount(3, 5),
                Error::SqlInputError {
                    error: ffi::Error::new(ffi::SQLITE_ERROR),
                    msg: "fn SQL failure".into(),
                    sql: "fn source".into(),
                    offset: 5,
                },
            ];
            for (index, expected) in cases.into_iter().enumerate() {
                let name = format!("variant{index}");
                let called = Arc::new(AtomicBool::new(false));
                let called2 = Arc::clone(&called);
                let pending = Mutex::new(Some(expected));
                db.try_create_scalar_function(name.as_str(), 0, flags(), move |_| {
                    called2.store(true, Ordering::SeqCst);
                    Err::<i64, Error>(pending
                        .lock()
                        .unwrap()
                        .take()
                        .expect("unexpected additional call"))
                })?;
                let sql = format!("SELECT {name}()");
                let actual = db.one_column::<i64, _>(&sql, []).unwrap_err();
                assert!(called.load(Ordering::SeqCst), "function was not called");
                // `Error` is not `Clone`; rebuild the expectation per index
                match index {
                    0 => assert_eq!(actual, Error::InvalidParameterName("fn-key".into())),
                    1 => assert_eq!(
                        actual,
                        Error::IntegralValueOutOfRange(6, -9_223_372_036_854_775_807)
                    ),
                    2 => assert_eq!(
                        actual,
                        Error::SqliteFailure(
                            ffi::Error::new(ffi::SQLITE_BUSY),
                            Some("original busy".into())
                        )
                    ),
                    3 => assert_eq!(actual, Error::QueryReturnedNoRows),
                    4 => assert_eq!(actual, Error::InvalidParameterCount(3, 5)),
                    5 => assert_eq!(
                        actual,
                        Error::SqlInputError {
                            error: ffi::Error::new(ffi::SQLITE_ERROR),
                            msg: "fn SQL failure".into(),
                            sql: "fn source".into(),
                            offset: 5,
                        }
                    ),
                    _ => unreachable!(),
                }
                // the connection is fully usable afterwards
                assert_eq!(db.one_column::<i64, _>("SELECT 1", [])?, 1);
            }
            Ok(())
        }

        #[test]
        fn custom_error_variant_round_trip() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.try_create_scalar_function(c"custom", 0, flags(), |_| -> Result<i64> {
                Err(Error::ToSqlConversionFailure(Box::new(FnError {
                    tag: 43,
                    detail: "detail/43".into(),
                    payload: vec![9, 8, 7],
                })))
            })?;
            match db.one_column::<i64, _>("SELECT custom()", []).unwrap_err() {
                Error::ToSqlConversionFailure(inner) => {
                    let fe = inner.downcast_ref::<FnError>().expect("custom type lost");
                    assert_eq!(fe.tag, 43);
                    assert_eq!(fe.payload, vec![9, 8, 7]);
                }
                other => panic!("expected ToSqlConversionFailure, got {other:?}"),
            }
            Ok(())
        }

        struct FailingToSql;
        impl ToSql for FailingToSql {
            fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
                Err(fn_error(77))
            }
        }

        struct PanickingToSql;
        impl ToSql for PanickingToSql {
            fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
                panic!("conversion panic")
            }
        }

        #[test]
        fn result_conversion_error_preserved() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.try_create_scalar_function(c"bad_result", 0, flags(), |_| Ok(FailingToSql))?;
            assert_fn_error(
                &db.one_column::<i64, _>("SELECT bad_result()", []).unwrap_err(),
                77,
            );
            assert_eq!(db.one_column::<i64, _>("SELECT 1", [])?, 1);
            Ok(())
        }

        #[test]
        fn panics_become_unwinding_panic_and_recover() -> Result<()> {
            let db = Connection::open_in_memory()?;
            let panic = Arc::new(AtomicBool::new(true));
            let flag = Arc::clone(&panic);
            db.try_create_scalar_function(c"maybe_panic", 1, flags(), move |ctx| {
                if flag.load(Ordering::SeqCst) {
                    panic!("callback panic");
                }
                ctx.get::<i64>(0)
            })?;
            assert_eq!(
                db.one_column::<i64, _>("SELECT maybe_panic(1)", [])
                    .unwrap_err(),
                Error::UnwindingPanic
            );
            // the function is still registered: normal input succeeds again
            panic.store(false, Ordering::SeqCst);
            assert_eq!(db.one_column::<i64, _>("SELECT maybe_panic(2)", [])?, 2);

            // a panic while converting the return value is caught too
            db.try_create_scalar_function(c"panic_result", 0, flags(), |_| Ok(PanickingToSql))?;
            assert_eq!(
                db.one_column::<i64, _>("SELECT panic_result()", [])
                    .unwrap_err(),
                Error::UnwindingPanic
            );
            assert_eq!(db.one_column::<i64, _>("SELECT 1", [])?, 1);
            Ok(())
        }

        #[test]
        fn execute_and_batch_receive_the_error() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER)")?;
            let gate = Arc::new(AtomicBool::new(true));
            gated_function(&db, "gate_fail", 61, Arc::clone(&gate))?;

            // execute
            let err = db
                .execute("INSERT INTO t VALUES (gate_fail(1))", [])
                .unwrap_err();
            assert_fn_error(&err, 61);
            // no transaction is left behind in autocommit mode
            assert!(db.is_autocommit());

            // execute_batch stops at the failing statement
            let err = db
                .execute_batch(
                    "INSERT INTO t VALUES (1);
                     INSERT INTO t VALUES (gate_fail(2));
                     INSERT INTO t VALUES (3);",
                )
                .unwrap_err();
            assert_fn_error(&err, 61);
            assert_eq!(db.one_column::<i64, _>("SELECT COUNT(*) FROM t", [])?, 1);
            assert_eq!(db.one_column::<i64, _>("SELECT MIN(x) FROM t", [])?, 1);

            // once the gate is lifted the same statements succeed
            gate.store(false, Ordering::SeqCst);
            db.execute("INSERT INTO t VALUES (gate_fail(2))", [])?;
            assert_eq!(db.one_column::<i64, _>("SELECT COUNT(*) FROM t", [])?, 2);
            Ok(())
        }

        #[test]
        fn row_iteration_ends_at_the_failure() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1),(2),(3);")?;
            db.try_create_scalar_function(c"fail_on_two", 1, flags(), |ctx| {
                let x = ctx.get::<i64>(0)?;
                if x == 2 {
                    return Err(fn_error(71));
                }
                Ok(x)
            })?;
            // no ORDER BY: rows are produced lazily, one per fetch
            let mut stmt = db.prepare("SELECT fail_on_two(x) FROM t")?;
            let mut rows = stmt.query([])?;
            {
                // rows delivered before the failure are unaffected
                let row = rows.next()?.expect("first row");
                assert_eq!(row.get::<_, i64>(0)?, 1);
            }
            // the failing fetch reports the original error
            assert_fn_error(&rows.next().unwrap_err(), 71);
            // the result set is over: further fetches report the end
            assert!(rows.next()?.is_none());
            drop(rows);
            // the statement can be reused
            assert_eq!(stmt.query_row([], |r| r.get::<_, i64>(0))?, 1);
            Ok(())
        }

        #[test]
        fn failure_is_delivered_exactly_once() -> Result<()> {
            let db = Connection::open_in_memory()?;
            let calls = Arc::new(AtomicUsize::new(0));
            let calls2 = Arc::clone(&calls);
            db.try_create_scalar_function(c"counted_fail", 0, flags(), move |_| {
                let n = calls2.fetch_add(1, Ordering::SeqCst) as u64;
                Err::<i64, Error>(fn_error(100 + n))
            })?;
            let e1 = db.one_column::<i64, _>("SELECT counted_fail()", []).unwrap_err();
            let e2 = db.one_column::<i64, _>("SELECT counted_fail()", []).unwrap_err();
            // each call gets the error of its own invocation, exactly once
            assert_fn_error(&e1, 100);
            assert_fn_error(&e2, 101);
            Ok(())
        }

        #[test]
        fn failure_inside_aggregate_query() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1),(2),(3);")?;
            db.try_create_scalar_function(c"agg_fail", 1, flags(), |ctx| {
                let x = ctx.get::<i64>(0)?;
                if x == 2 {
                    return Err(fn_error(97));
                }
                Ok(x)
            })?;
            // the function fails while SQLite is stepping an aggregate
            assert_fn_error(
                &db.one_column::<i64, _>("SELECT sum(agg_fail(x)) FROM t", [])
                    .unwrap_err(),
                97,
            );
            // the connection stays usable
            assert_eq!(db.one_column::<i64, _>("SELECT sum(x) FROM t", [])?, 6);
            Ok(())
        }

        #[test]
        fn returning_clause_reports_the_error() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER)")?;
            let gate = Arc::new(AtomicBool::new(true));
            gated_function(&db, "ret_fail", 99, Arc::clone(&gate))?;
            let mut stmt = db.prepare("INSERT INTO t VALUES (ret_fail(1)) RETURNING x")?;
            // the write fails while fetching the returned row
            assert_fn_error(&stmt.query_row([], |r| r.get::<_, i64>(0)).unwrap_err(), 99);
            drop(stmt);
            assert_eq!(db.one_column::<i64, _>("SELECT COUNT(*) FROM t", [])?, 0);
            // the same statement succeeds once the function recovers
            gate.store(false, Ordering::SeqCst);
            let mut stmt = db.prepare("INSERT INTO t VALUES (ret_fail(1)) RETURNING x")?;
            assert_eq!(stmt.query_row([], |r| r.get::<_, i64>(0))?, 1);
            Ok(())
        }

        #[test]
        fn statement_reuse_and_cleanup() -> Result<()> {
            let db = Connection::open_in_memory()?;
            let gate = Arc::new(AtomicBool::new(true));
            gated_function(&db, "reusable", 81, Arc::clone(&gate))?;

            // same statement, rebound and re-executed
            let mut stmt = db.prepare("SELECT reusable(?)")?;
            assert_fn_error(&stmt.query_row([1], |r| r.get::<_, i64>(0)).unwrap_err(), 81);
            gate.store(false, Ordering::SeqCst);
            assert_eq!(stmt.query_row([2], |r| r.get::<_, i64>(0))?, 2);
            drop(stmt);

            // destroying the statement and running other SQL does not
            // resurrect the delivered error
            gate.store(true, Ordering::SeqCst);
            {
                let mut stmt = db.prepare("SELECT reusable(?)")?;
                assert_fn_error(&stmt.query_row([1], |r| r.get::<_, i64>(0)).unwrap_err(), 81);
            }
            assert_eq!(db.one_column::<i64, _>("SELECT 5", [])?, 5);
            gate.store(false, Ordering::SeqCst);
            assert_eq!(db.one_column::<i64, _>("SELECT reusable(4)", [])?, 4);
            Ok(())
        }

        #[cfg(feature = "cache")]
        #[test]
        fn cached_statement_does_not_carry_the_error() -> Result<()> {
            let db = Connection::open_in_memory()?;
            let gate = Arc::new(AtomicBool::new(true));
            gated_function(&db, "cached_reusable", 85, Arc::clone(&gate))?;
            {
                let mut stmt = db.prepare_cached("SELECT cached_reusable(?)")?;
                assert_fn_error(&stmt.query_row([1], |r| r.get::<_, i64>(0)).unwrap_err(), 85);
                // returned to the cache while the function still fails
            }
            gate.store(false, Ordering::SeqCst);
            {
                // the cached statement does not carry the old error
                let mut stmt = db.prepare_cached("SELECT cached_reusable(?)")?;
                assert_eq!(stmt.query_row([3], |r| r.get::<_, i64>(0))?, 3);
            }
            Ok(())
        }

        #[test]
        fn early_drop_leaves_no_error_behind() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1),(2);")?;
            db.try_create_scalar_function(c"would_fail", 1, flags(), |ctx| {
                let x = ctx.get::<i64>(0)?;
                if x == 2 {
                    return Err(fn_error(91));
                }
                Ok(x)
            })?;
            {
                let mut stmt = db.prepare("SELECT would_fail(x) FROM t")?;
                let mut rows = stmt.query([])?;
                assert!(rows.next()?.is_some());
                // stop reading before the failing row and destroy everything
            }
            // no error was left behind to leak into unrelated calls
            assert_eq!(db.one_column::<i64, _>("SELECT 1", [])?, 1);
            let err = db.prepare("SELEC 1").unwrap_err();
            assert!(matches!(err, Error::SqlInputError { .. }), "{err:?}");
            Ok(())
        }

        #[test]
        fn errors_are_isolated_between_statements_and_connections() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1);")?;
            db.try_create_scalar_function(c"fail_a", 1, flags(), |_| -> Result<i64> {
                Err(fn_error(201))
            })?;
            db.try_create_scalar_function(c"fail_b", 1, flags(), |_| -> Result<i64> {
                Err(fn_error(202))
            })?;
            let mut a = db.prepare("SELECT fail_a(x) FROM t")?;
            let mut b = db.prepare("SELECT fail_b(x) FROM t")?;
            // alternating reads: each statement gets its own function's error
            assert_fn_error(&a.query_row([], |r| r.get::<_, i64>(0)).unwrap_err(), 201);
            assert_eq!(db.one_column::<i64, _>("SELECT 5", [])?, 5);
            assert_fn_error(&b.query_row([], |r| r.get::<_, i64>(0)).unwrap_err(), 202);
            drop(a);
            drop(b);

            // connections do not leak errors into each other
            let db1 = Connection::open_in_memory()?;
            let db2 = Connection::open_in_memory()?;
            db1.try_create_scalar_function(c"f", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(301))
            })?;
            db2.try_create_scalar_function(c"f", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(302))
            })?;
            assert_fn_error(&db1.one_column::<i64, _>("SELECT f()", []).unwrap_err(), 301);
            assert_fn_error(&db2.one_column::<i64, _>("SELECT f()", []).unwrap_err(), 302);
            assert_fn_error(&db1.one_column::<i64, _>("SELECT f()", []).unwrap_err(), 301);
            Ok(())
        }

        #[test]
        fn nested_sql_through_context_connection() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.try_create_scalar_function(c"inner_fail", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(51))
            })?;

            // an inner error the callback handles does not fail the outer call
            db.try_create_scalar_function(c"outer_handled", 0, flags(), |ctx| {
                let conn = unsafe { ctx.get_connection()? };
                let r: Result<i64> = conn.query_row("SELECT inner_fail()", [], |r| r.get(0));
                assert_fn_error(&r.unwrap_err(), 51);
                Ok(42i64)
            })?;
            assert_eq!(db.one_column::<i64, _>("SELECT outer_handled()", [])?, 42);

            // an inner error the callback returns is delivered to the outer
            // caller as the same error object
            db.try_create_scalar_function(c"outer_propagated", 0, flags(), |ctx| -> Result<i64> {
                let conn = unsafe { ctx.get_connection()? };
                let r: Result<i64> = conn.query_row("SELECT inner_fail()", [], |r| r.get(0));
                Err(r.unwrap_err())
            })?;
            assert_fn_error(
                &db.one_column::<i64, _>("SELECT outer_propagated()", [])
                    .unwrap_err(),
                51,
            );
            Ok(())
        }

        #[test]
        fn unrelated_sqlite_errors_are_not_masked() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER UNIQUE); INSERT INTO t VALUES (1);")?;
            db.try_create_scalar_function(c"always_fail", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(111))
            })?;
            assert_fn_error(&db.one_column::<i64, _>("SELECT always_fail()", []).unwrap_err(), 111);

            // a unique-constraint violation is reported as itself
            match db.execute("INSERT INTO t VALUES (1)", []).unwrap_err() {
                Error::SqliteFailure(ref err, _) => {
                    assert_eq!(crate::ErrorCode::ConstraintViolation, err.code);
                }
                ref other => panic!("expected SqliteFailure, got {other:?}"),
            }
            // a syntax error keeps its offset information
            match db.prepare("SELEC 1").unwrap_err() {
                Error::SqlInputError { .. } => {}
                ref other => panic!("expected SqlInputError, got {other:?}"),
            }
            // and a generic SQLite error is not rewritten either
            db.execute_batch("BEGIN")?;
            match db.execute_batch("BEGIN").unwrap_err() {
                Error::SqliteFailure(ref err, _) => {
                    assert_eq!(crate::ErrorCode::Unknown, err.code);
                }
                ref other => panic!("expected SqliteFailure, got {other:?}"),
            }
            db.execute_batch("ROLLBACK")?;
            Ok(())
        }

        #[test]
        fn shared_registration_replace_and_remove() -> Result<()> {
            let db = Connection::open_in_memory()?;
            // a try_ registration replaces a legacy one with the same name
            db.create_scalar_function(c"f", 0, flags(), |_| Ok(1i64))?;
            db.try_create_scalar_function(c"f", 0, flags(), |_| Ok(2i64))?;
            assert_eq!(db.one_column::<i64, _>("SELECT f()", [])?, 2);
            // and vice versa; the legacy entry keeps its flattening behavior
            db.create_scalar_function(c"f", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(121))
            })?;
            match db.one_column::<i64, _>("SELECT f()", []).unwrap_err() {
                Error::SqliteFailure(..) => {}
                ref other => panic!("legacy registration must keep flattening, got {other:?}"),
            }
            // remove_function removes a try_ registration
            db.try_create_scalar_function(c"g", 1, flags(), |ctx| ctx.get::<i64>(0))?;
            assert_eq!(db.one_column::<i64, _>("SELECT g(3)", [])?, 3);
            db.remove_function(c"g", 1)?;
            db.one_column::<i64, _>("SELECT g(3)", []).unwrap_err();
            Ok(())
        }

        #[test]
        fn failure_keeps_transaction_behavior() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.execute_batch("CREATE TABLE t(x INTEGER)")?;
            let gate = Arc::new(AtomicBool::new(false));
            gated_function(&db, "txn_fail", 131, Arc::clone(&gate))?;

            db.execute_batch("BEGIN")?;
            db.execute("INSERT INTO t VALUES (1)", [])?;
            gate.store(true, Ordering::SeqCst);
            assert_fn_error(
                &db.execute("INSERT INTO t VALUES (txn_fail(2))", [])
                    .unwrap_err(),
                131,
            );
            // the caller's transaction is neither committed nor rolled back
            assert!(!db.is_autocommit());
            gate.store(false, Ordering::SeqCst);
            db.execute("INSERT INTO t VALUES (txn_fail(3))", [])?;
            db.execute_batch("COMMIT")?;
            assert!(db.is_autocommit());
            assert_eq!(
                db.one_column::<String, _>(
                    "SELECT group_concat(x) FROM (SELECT x FROM t ORDER BY x)",
                    [],
                )?,
                "1,3"
            );
            Ok(())
        }

        #[test]
        fn legacy_registration_behavior_is_unchanged() -> Result<()> {
            let db = Connection::open_in_memory()?;
            db.create_scalar_function(c"legacy_fail", 0, flags(), |_| -> Result<i64> {
                Err(fn_error(141))
            })?;
            // the legacy entry point still flattens the error into a SQLite
            // error; the original object is not delivered
            match db.one_column::<i64, _>("SELECT legacy_fail()", []).unwrap_err() {
                Error::SqliteFailure(_, Some(msg)) => {
                    assert_eq!(msg, fn_error(141).to_string());
                }
                ref other => panic!("expected SqliteFailure, got {other:?}"),
            }
            Ok(())
        }
    }
}
