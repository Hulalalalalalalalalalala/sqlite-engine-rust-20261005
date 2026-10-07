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
use std::ffi::{c_int, c_uint, c_void};
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

/// The original error of the most recent failing call to a scalar function
/// registered with
/// [`create_scalar_function_with_error`](Connection::create_scalar_function_with_error)
/// on a connection, along with the SQLite result code the failure was
/// reported as.
///
/// SQLite only carries the result code and message of a function failure
/// back to the caller, so the original error is kept on the connection and
/// reported to the rusqlite call that observes that result code. The slot is
/// cleared at the start of every such function call, and the error is taken
/// when the failure it caused is decoded, so a stale error can never be
/// attributed to a later, unrelated failure.
#[derive(Default)]
struct ScalarFunctionError {
    error: Option<(c_int, Error)>,
}

unsafe fn scalar_function_error_slot(db: *mut ffi::sqlite3) -> *mut ScalarFunctionError {
    unsafe { ffi::sqlite3_get_clientdata(db, c"sqlite3_create_function".as_ptr()) }
        .cast::<ScalarFunctionError>()
}

/// If `code` is the SQLite result code that the most recent failing call to
/// a scalar function registered with `create_scalar_function_with_error` on
/// `db` was reported as, take and return the original error. The error is
/// consumed, so it is reported at most once, by the call that triggered the
/// failure.
pub(crate) unsafe fn take_scalar_function_error(
    db: *mut ffi::sqlite3,
    code: c_int,
) -> Option<Error> {
    if db.is_null() {
        return None;
    }
    let slot = unsafe { scalar_function_error_slot(db) };
    if slot.is_null() {
        return None;
    }
    let error = unsafe { &mut (*slot).error };
    if error.as_ref().is_some_and(|(c, _)| *c == code) {
        error.take().map(|(_, err)| err)
    } else {
        None
    }
}

/// Report `err` to SQLite like [`report_error`], but set the message before
/// the code so that the code is not reset to `SQLITE_ERROR`, and return the
/// code the failure is reported as.
unsafe fn report_error_with_code(ctx: *mut sqlite3_context, err: &Error) -> c_int {
    let code = match *err {
        Error::SqliteFailure(ref err, _) => err.extended_code,
        _ => ffi::SQLITE_CONSTRAINT_FUNCTION,
    };
    unsafe {
        if let Ok(cstr) = str_to_cstring(&err.to_string()) {
            ffi::sqlite3_result_error(ctx, cstr.as_ptr(), -1);
        }
        ffi::sqlite3_result_error_code(ctx, code);
    }
    code
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
    /// A failing callback is reported to the caller as a generic SQLite
    /// error; use
    /// [`create_scalar_function_with_error`](Connection::create_scalar_function_with_error)
    /// to have the caller receive the original error instead.
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

    /// Attach a user-defined scalar function to
    /// this database connection, preserving the original error when the
    /// function fails.
    ///
    /// This behaves like
    /// [`create_scalar_function`](Connection::create_scalar_function) — the
    /// function is registered under `fn_name` with `n_arg` arguments and
    /// `flags`, and it stays registered until it is replaced or removed via
    /// [`remove_function`](Connection::remove_function) — but a failure is
    /// reported to the caller with the original error instead of a generic
    /// SQLite error: if the callback returns an error, or converting its
    /// return value to a SQL value fails, the call that triggered the
    /// function fails with that exact [`Error`], so it can be matched and
    /// inspected (an [`Error::UserFunctionError`], for example, can be
    /// downcast to recover the error the callback reported). If the callback
    /// — or the conversion of its return value — panics, the triggering
    /// call fails with [`Error::UnwindingPanic`].
    ///
    /// The error is reported only to the call that triggered the failure:
    /// other statements and other connections are unaffected, the function
    /// stays registered, and later calls behave normally.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use rusqlite::{Connection, Error, Result};
    /// # use rusqlite::functions::FunctionFlags;
    /// fn scalar_function_with_error_example(db: &Connection) -> Result<()> {
    ///     db.create_scalar_function_with_error(
    ///         "parse_int",
    ///         1,
    ///         FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
    ///         |ctx| {
    ///             let s = ctx.get::<String>(0)?;
    ///             s.parse::<i64>()
    ///                 .map_err(|e| Error::UserFunctionError(Box::new(e)))
    ///         },
    ///     )?;
    ///
    ///     let err = db
    ///         .query_row::<i64, _, _>("SELECT parse_int('nope')", [], |r| r.get(0))
    ///         .unwrap_err();
    ///     match err {
    ///         Error::UserFunctionError(e) => assert!(e.is::<std::num::ParseIntError>()),
    ///         e => panic!("unexpected error: {e}"),
    ///     }
    ///     Ok(())
    /// }
    /// ```
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
        self.db
            .borrow_mut()
            .create_scalar_function_with_error(fn_name, n_arg, flags, x_func)
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
    ///         db.create_scalar_function_with_error(
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
    fn create_scalar_function_with_error<F, N: Name, T>(
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
                let db = ffi::sqlite3_context_db_handle(ctx);
                // A fresh call cannot observe a previous call's error.
                let slot = scalar_function_error_slot(db);
                if !slot.is_null() {
                    (*slot).error = None;
                }
                let r = catch_unwind(|| {
                    let boxed_f: *const F = ffi::sqlite3_user_data(ctx).cast::<F>();
                    assert!(!boxed_f.is_null(), "Internal error - null function pointer");
                    let r = (*boxed_f)(&Context { ctx, args });
                    _sql_result(ctx, args, r)
                });
                let r = match r {
                    Ok(r) => r,
                    Err(_) => Err(Error::UnwindingPanic),
                };
                if let Err(err) = r {
                    let code = report_error_with_code(ctx, &err);
                    if !slot.is_null() {
                        (*slot).error = Some((code, err));
                    }
                }
            }
        }

        // The slot the original error is reported through is shared by all
        // such functions on this connection.
        if unsafe { self.get_clientdata::<ScalarFunctionError, _>(c"sqlite3_create_function") }?
            .is_null()
        {
            self.set_clientdata(
                c"sqlite3_create_function",
                Some(ScalarFunctionError::default()),
                |_, _| ffi::SQLITE_OK,
            )?;
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
                Some(call_boxed_closure_with_error::<F, T>),
                None,
                None,
                Some(free_boxed_value::<F>),
            )
        };
        self.decode_result(r)
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

    #[derive(Debug)]
    struct CustomError(i32);

    impl std::fmt::Display for CustomError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "custom error {}", self.0)
        }
    }

    impl std::error::Error for CustomError {}

    /// `failing(x)` returns `x`, or fails with
    /// `Error::UserFunctionError(CustomError(x))` when `x` is negative.
    fn failing_function(db: &Connection) -> Result<()> {
        db.create_scalar_function_with_error(
            c"failing",
            1,
            FunctionFlags::SQLITE_UTF8,
            |ctx| -> Result<i64> {
                let x = ctx.get::<i64>(0)?;
                if x < 0 {
                    Err(Error::UserFunctionError(Box::new(CustomError(x as i32))))
                } else {
                    Ok(x)
                }
            },
        )
    }

    fn assert_custom_error(err: Error, expected: i32) {
        match err {
            Error::UserFunctionError(e) => {
                assert_eq!(expected, e.downcast_ref::<CustomError>().unwrap().0);
            }
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_scalar_function_with_error() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        // normal values pass through
        assert_eq!(3, db.one_column::<i64, _>("SELECT failing(3)", [])?);
        // the original error object is delivered to the triggering call
        let err = db
            .one_column::<i64, _>("SELECT failing(-7)", [])
            .unwrap_err();
        assert_custom_error(err, -7);
        // the function stays registered and recovers
        assert_eq!(3, db.one_column::<i64, _>("SELECT failing(3)", [])?);
        // a later failure reports its own error, not a replayed one
        let err = db
            .one_column::<i64, _>("SELECT failing(-2)", [])
            .unwrap_err();
        assert_custom_error(err, -2);
        Ok(())
    }

    #[test]
    fn test_scalar_function_error_not_preserved_by_default() {
        let db = Connection::open_in_memory().unwrap();
        db.create_scalar_function(
            c"failing",
            0,
            FunctionFlags::SQLITE_UTF8,
            |_| -> Result<i64> { Err(Error::UserFunctionError(Box::new(CustomError(1)))) },
        )
        .unwrap();
        // the classic registration keeps its behavior of a generic SQLite error
        match db.one_column::<i64, _>("SELECT failing()", []).unwrap_err() {
            Error::SqliteFailure(..) => {}
            e => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn test_scalar_function_with_error_execute_and_batch() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE t(x);")?;
        // writes report their own function error
        let err = db
            .execute("INSERT INTO t VALUES (failing(-1))", [])
            .unwrap_err();
        assert_custom_error(err, -1);
        // so does batch execution
        let err = db
            .execute_batch("INSERT INTO t VALUES (failing(-2));")
            .unwrap_err();
        assert_custom_error(err, -2);
        // nothing was inserted
        assert_eq!(0, db.one_column::<i64, _>("SELECT COUNT(*) FROM t", [])?);
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_rows() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(-1),(3);")?;
        let mut stmt = db.prepare("SELECT failing(x) FROM t")?;
        let mut rows = stmt.query([])?;
        // rows delivered before the failure are unaffected
        assert_eq!(1, rows.next()?.unwrap().get::<_, i64>(0)?);
        assert_eq!(2, rows.next()?.unwrap().get::<_, i64>(0)?);
        // the failing read reports the original error
        assert_custom_error(rows.next().unwrap_err(), -1);
        // the result set is over
        assert!(rows.next()?.is_none());
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_early_drop() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(-1);")?;
        {
            let mut stmt = db.prepare("SELECT failing(x) FROM t")?;
            let mut rows = stmt.query([])?;
            assert_eq!(1, rows.next()?.unwrap().get::<_, i64>(0)?);
            // drop the result set before reaching the failing row
        }
        // nothing leaks into later, unrelated calls
        assert_eq!(42, db.one_column::<i64, _>("SELECT 42", [])?);
        db.execute("INSERT INTO t VALUES (2)", [])?;
        // and the failing row still fails with its own error when re-read
        let mut stmt = db.prepare("SELECT failing(x) FROM t ORDER BY x")?;
        let mut rows = stmt.query([])?;
        assert_custom_error(rows.next().unwrap_err(), -1);
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_interleaved_statements() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(-1),(2);")?;
        let mut s1 = db.prepare("SELECT failing(x) FROM t")?;
        let mut s2 = db.prepare("SELECT x * 10 FROM t")?;
        let mut r1 = s1.query([])?;
        let mut r2 = s2.query([])?;
        assert_eq!(1, r1.next()?.unwrap().get::<_, i64>(0)?);
        assert_eq!(10, r2.next()?.unwrap().get::<_, i64>(0)?);
        // the error is reported to the statement that triggered it only
        assert_custom_error(r1.next().unwrap_err(), -1);
        assert_eq!(-10, r2.next()?.unwrap().get::<_, i64>(0)?);
        assert_eq!(20, r2.next()?.unwrap().get::<_, i64>(0)?);
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_two_connections() -> Result<()> {
        let db1 = Connection::open_in_memory()?;
        let db2 = Connection::open_in_memory()?;
        failing_function(&db1)?;
        failing_function(&db2)?;
        assert_custom_error(
            db1.one_column::<i64, _>("SELECT failing(-1)", [])
                .unwrap_err(),
            -1,
        );
        // the other connection is unaffected
        assert_eq!(3, db2.one_column::<i64, _>("SELECT failing(3)", [])?);
        assert_custom_error(
            db2.one_column::<i64, _>("SELECT failing(-2)", [])
                .unwrap_err(),
            -2,
        );
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_then_sqlite_error() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE u(x UNIQUE); INSERT INTO u VALUES (1);")?;
        // trigger and deliver a function error
        assert_custom_error(
            db.one_column::<i64, _>("SELECT failing(-1)", [])
                .unwrap_err(),
            -1,
        );
        // a genuine constraint violation keeps its own error
        match db.execute("INSERT INTO u VALUES (1)", []).unwrap_err() {
            Error::SqliteFailure(e, _) => {
                assert_eq!(e.code, crate::ffi::ErrorCode::ConstraintViolation);
            }
            e => panic!("unexpected error: {e}"),
        }
        // so does a syntax error
        match db.prepare("SELEC oops").unwrap_err() {
            Error::SqlInputError { .. } | Error::SqliteFailure(..) => {}
            e => panic!("unexpected error: {e}"),
        }
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_reexecute() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        let mut stmt = db.prepare("SELECT failing(?)")?;
        // fail, then rebind and succeed on the same statement
        assert_custom_error(
            stmt.query_one([-1], |r| r.get::<_, i64>(0)).unwrap_err(),
            -1,
        );
        assert_eq!(2, stmt.query_one([2], |r| r.get::<_, i64>(0))?);
        // a new failure reports a fresh error, not the delivered one
        assert_custom_error(
            stmt.query_one([-5], |r| r.get::<_, i64>(0)).unwrap_err(),
            -5,
        );
        assert_eq!(7, stmt.query_one([7], |r| r.get::<_, i64>(0))?);
        Ok(())
    }

    #[test]
    #[cfg(feature = "cache")]
    fn test_scalar_function_with_error_cached_statement() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        for i in 1..=2 {
            let mut stmt = db.prepare_cached("SELECT failing(?)")?;
            // each execution of the cached statement reports its own error
            assert_custom_error(
                stmt.query_one([-i], |r| r.get::<_, i64>(0)).unwrap_err(),
                -i,
            );
            assert_eq!(i64::from(i), stmt.query_one([i], |r| r.get::<_, i64>(0))?);
        }
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_panic() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function_with_error(
            c"panics",
            1,
            FunctionFlags::SQLITE_UTF8,
            |ctx| -> Result<i64> {
                let x = ctx.get::<i64>(0)?;
                assert!(x >= 0, "boom");
                Ok(x)
            },
        )?;
        assert_eq!(
            Error::UnwindingPanic,
            db.one_column::<i64, _>("SELECT panics(-1)", [])
                .unwrap_err()
        );
        // the process is still alive and the function still works
        assert_eq!(3, db.one_column::<i64, _>("SELECT panics(3)", [])?);
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_output_conversion() -> Result<()> {
        use crate::types::{ToSql, ToSqlOutput};

        struct Failing;
        impl ToSql for Failing {
            fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
                Err(Error::ToSqlConversionFailure(Box::new(CustomError(9))))
            }
        }
        struct Panicking;
        impl ToSql for Panicking {
            fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
                panic!("boom")
            }
        }
        let db = Connection::open_in_memory()?;
        db.create_scalar_function_with_error(c"fails", 0, FunctionFlags::SQLITE_UTF8, |_| {
            Ok(Failing)
        })?;
        db.create_scalar_function_with_error(c"panics", 0, FunctionFlags::SQLITE_UTF8, |_| {
            Ok(Panicking)
        })?;
        // a conversion failure reports the original error
        match db.one_column::<i64, _>("SELECT fails()", []).unwrap_err() {
            Error::ToSqlConversionFailure(e) => {
                assert_eq!(9, e.downcast_ref::<CustomError>().unwrap().0);
            }
            e => panic!("unexpected error: {e}"),
        }
        // a panicking conversion reports Error::UnwindingPanic
        assert_eq!(
            Error::UnwindingPanic,
            db.one_column::<i64, _>("SELECT panics()", []).unwrap_err()
        );
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_subtype() -> Result<()> {
        fn getsubtype(ctx: &Context<'_>) -> Result<i32> {
            Ok(ctx.get_subtype(0) as i32)
        }
        fn setsubtype(ctx: &Context<'_>) -> Result<(SqlFnArg, SubType)> {
            use std::ffi::c_uint;
            let value = ctx.get_arg(0);
            let sub_type = ctx.get::<c_uint>(1)?;
            Ok((value, Some(sub_type)))
        }
        let db = Connection::open_in_memory()?;
        db.create_scalar_function(c"getsubtype", 1, FunctionFlags::SQLITE_UTF8, getsubtype)?;
        db.create_scalar_function_with_error(
            c"setsubtype",
            2,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_RESULT_SUBTYPE,
            setsubtype,
        )?;
        // normal return values and subtypes pass through
        assert_eq!(
            123,
            db.one_column::<i32, _>("SELECT getsubtype(setsubtype('hello', 123))", [])?
        );
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_inner_sql() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE u(x UNIQUE); INSERT INTO u VALUES (1);")?;
        // an inner error handled by the callback does not affect the outer call
        db.create_scalar_function_with_error(
            c"inner_handled",
            0,
            FunctionFlags::SQLITE_UTF8,
            |ctx| {
                let conn = unsafe { ctx.get_connection()? };
                match conn.execute("INSERT INTO u VALUES (1)", []) {
                    Err(Error::SqliteFailure(..)) => Ok(42i64),
                    r => panic!("unexpected result: {r:?}"),
                }
            },
        )?;
        assert_eq!(42, db.one_column::<i64, _>("SELECT inner_handled()", [])?);
        // an inner error returned by the callback is delivered as-is
        db.create_scalar_function_with_error(
            c"inner_propagated",
            0,
            FunctionFlags::SQLITE_UTF8,
            |ctx| {
                let conn = unsafe { ctx.get_connection()? };
                conn.execute("INSERT INTO u VALUES (1)", [])?;
                Ok(0i64)
            },
        )?;
        match db
            .one_column::<i64, _>("SELECT inner_propagated()", [])
            .unwrap_err()
        {
            Error::SqliteFailure(e, m) => {
                assert_eq!(e.code, crate::ffi::ErrorCode::ConstraintViolation);
                assert_eq!(m.as_deref(), Some("UNIQUE constraint failed: u.x"));
            }
            e => panic!("unexpected error: {e}"),
        }
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_transaction() -> Result<()> {
        let db = Connection::open_in_memory()?;
        failing_function(&db)?;
        db.execute_batch("CREATE TABLE t(x);")?;
        db.execute_batch("BEGIN")?;
        db.execute("INSERT INTO t VALUES (1)", [])?;
        // a function failure neither commits nor rolls back the transaction
        assert_custom_error(
            db.execute("INSERT INTO t VALUES (failing(-1))", [])
                .unwrap_err(),
            -1,
        );
        assert!(!db.is_autocommit());
        db.execute("INSERT INTO t VALUES (2)", [])?;
        db.execute_batch("COMMIT")?;
        assert_eq!(2, db.one_column::<i64, _>("SELECT COUNT(*) FROM t", [])?);
        Ok(())
    }

    #[test]
    fn test_scalar_function_with_error_replace_and_remove() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.create_scalar_function_with_error(c"f", 0, FunctionFlags::SQLITE_UTF8, |_| Ok(1i64))?;
        assert_eq!(1, db.one_column::<i64, _>("SELECT f()", [])?);
        // replacing with a failing variant works
        db.create_scalar_function_with_error(
            c"f",
            0,
            FunctionFlags::SQLITE_UTF8,
            |_| -> Result<i64> { Err(Error::UserFunctionError(Box::new(CustomError(1)))) },
        )?;
        assert_custom_error(db.one_column::<i64, _>("SELECT f()", []).unwrap_err(), 1);
        // replacing with the classic registration works
        db.create_scalar_function(c"f", 0, FunctionFlags::SQLITE_UTF8, |_| Ok(2i64))?;
        assert_eq!(2, db.one_column::<i64, _>("SELECT f()", [])?);
        // removing works
        db.remove_function(c"f", 0)?;
        db.one_column::<i64, _>("SELECT f()", []).unwrap_err();
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
}
