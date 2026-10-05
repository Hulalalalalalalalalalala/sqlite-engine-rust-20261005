//! Online SQLite backup API.
//!
//! Alternatively, you can create a backup with a simple
//! [`VACUUM INTO <backup_path>`](https://sqlite.org/lang_vacuum.html#vacuuminto).
//!
//! To create a [`Backup`], you must have two distinct [`Connection`]s - one
//! for the source (which can be used while the backup is running) and one for
//! the destination (which cannot).  A [`Backup`] handle exposes three methods:
//! [`step`](Backup::step) will attempt to back up a specified number of pages,
//! [`progress`](Backup::progress) gets the current progress of the backup as of
//! the last call to [`step`](Backup::step), and
//! [`run_to_completion`](Backup::run_to_completion) will attempt to back up the
//! entire source database, allowing you to specify how many pages are backed up
//! at a time and how long the thread should sleep between chunks of pages, and
//! [`run_to_completion_with_callback`](Backup::run_to_completion_with_callback)
//! does the same while letting a closure observe every step, abort the run, and
//! bound the number of consecutive lock conflicts.
//!
//! The following example is equivalent to "Example 2: Online Backup of a
//! Running Database" from [SQLite's Online Backup API
//! documentation](https://www.sqlite.org/backup.html).
//!
//! ```rust,no_run
//! # use rusqlite::{backup, Connection, Result};
//! # use std::path::Path;
//! # use std::time;
//!
//! fn backup_db<P: AsRef<Path>>(
//!     src: &Connection,
//!     dst: P,
//!     progress: fn(backup::Progress),
//! ) -> Result<()> {
//!     let mut dst = Connection::open(dst)?;
//!     let backup = backup::Backup::new(src, &mut dst)?;
//!     backup.run_to_completion(5, time::Duration::from_millis(250), Some(progress))
//! }
//! ```

use std::marker::PhantomData;
use std::path::Path;
use std::ptr;

use std::ffi::c_int;
use std::thread;
use std::time::Duration;

use crate::ffi;

use crate::error::error_from_handle;
use crate::{Connection, Error, MAIN_DB, Name, Result};

impl Connection {
    /// Back up the `name` database to the given
    /// destination path.
    ///
    /// If `progress` is not `None` / [`NO_PROGRESS`], it will be called periodically
    /// until the backup completes.
    ///
    /// For more fine-grained control over the backup process (e.g.,
    /// to sleep periodically during the backup or to back up to an
    /// already-open database connection), see the `backup` module.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the destination path cannot be opened
    /// or if the backup fails.
    pub fn backup<N: Name, P: AsRef<Path>, F: Fn(Progress)>(
        &self,
        name: N,
        dst_path: P,
        progress: Option<F>,
    ) -> Result<()> {
        use self::StepResult::{Busy, Done, Locked, More};
        let mut dst = Self::open(dst_path)?;
        let backup = Backup::new_with_names(self, name, &mut dst, MAIN_DB)?;

        let mut r = More;
        while r == More {
            r = backup.step(100)?;
            if let Some(ref f) = progress {
                f(backup.progress());
            }
        }

        match r {
            Done => Ok(()),
            Busy => Err(unsafe { error_from_handle(ptr::null_mut(), ffi::SQLITE_BUSY) }),
            Locked => Err(unsafe { error_from_handle(ptr::null_mut(), ffi::SQLITE_LOCKED) }),
            More => unreachable!(),
        }
    }

    /// Restore the given source path into the
    /// `name` database. If `progress` is not `None` / [`NO_PROGRESS`], it will be
    /// called periodically until the restore completes.
    ///
    /// For more fine-grained control over the restore process (e.g.,
    /// to sleep periodically during the restore or to restore from an
    /// already-open database connection), see the `backup` module.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the destination path cannot be opened
    /// or if the restore fails.
    pub fn restore<N: Name, P: AsRef<Path>, F: Fn(Progress)>(
        &mut self,
        name: N,
        src_path: P,
        progress: Option<F>,
    ) -> Result<()> {
        use self::StepResult::{Busy, Done, Locked, More};
        let src = Self::open(src_path)?;
        let restore = Backup::new_with_names(&src, MAIN_DB, self, name)?;

        let mut r = More;
        let mut busy_count = 0_i32;
        'restore_loop: while r == More || r == Busy {
            r = restore.step(100)?;
            if let Some(ref f) = progress {
                f(restore.progress());
            }
            if r == Busy {
                busy_count += 1;
                if busy_count >= 3 {
                    break 'restore_loop;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        match r {
            Done => Ok(()),
            Busy => Err(unsafe { error_from_handle(ptr::null_mut(), ffi::SQLITE_BUSY) }),
            Locked => Err(unsafe { error_from_handle(ptr::null_mut(), ffi::SQLITE_LOCKED) }),
            More => unreachable!(),
        }
    }
}

/// Ignore backup / restore progress
pub const NO_PROGRESS: Option<fn(_: Progress)> = None;

/// Possible successful results of calling
/// [`Backup::step`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StepResult {
    /// The backup is complete.
    Done,

    /// The step was successful but there are still more pages that need to be
    /// backed up.
    More,

    /// The step failed because appropriate locks could not be acquired. This is
    /// not a fatal error - the step can be retried.
    Busy,

    /// The step failed because the source connection was writing to the
    /// database. This is not a fatal error - the step can be retried.
    Locked,
}

/// Struct specifying the progress of a backup.
///
/// The percentage completion can be calculated as `(pagecount - remaining) /
/// pagecount`. The progress of a backup is as of the last call to
/// [`step`](Backup::step) - if the source database is modified after a call to
/// [`step`](Backup::step), the progress value will become outdated and
/// potentially incorrect.
#[derive(Copy, Clone, Debug)]
pub struct Progress {
    /// Number of pages in the source database that still need to be backed up.
    pub remaining: c_int,
    /// Total number of pages in the source database.
    pub pagecount: c_int,
}

/// Instruction returned by the callback of
/// [`run_to_completion_with_callback`](Backup::run_to_completion_with_callback)
/// to control the automatic backup loop.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackupControl {
    /// Continue with the next step, sleeping for the configured pause first.
    Continue,

    /// Stop the automatic run immediately: no further pages are copied and no
    /// further sleeping happens. The [`Backup`] handle stays usable.
    Abort,
}

/// The outcome of
/// [`run_to_completion_with_callback`](Backup::run_to_completion_with_callback).
/// Database errors are reported through the enclosing [`Result`]'s `Err`
/// variant instead.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackupRunOutcome {
    /// The backup ran to completion.
    Done,

    /// The callback asked to stop before the backup was complete. Only this
    /// automatic run is ended - the [`Backup`] handle can still be stepped
    /// manually or driven to completion by another call.
    Aborted,
}

/// A handle to an online backup.
pub struct Backup<'a, 'b> {
    phantom_from: PhantomData<&'a Connection>,
    to: &'b Connection,
    b: *mut ffi::sqlite3_backup,
}

impl Backup<'_, '_> {
    /// Attempt to create a new handle that will allow backups from `from` to
    /// `to`. Note that `to` is a `&mut` - this is because SQLite forbids any
    /// API calls on the destination of a backup while the backup is taking
    /// place.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying `sqlite3_backup_init` call returns
    /// `NULL`.
    #[inline]
    pub fn new<'a, 'b>(from: &'a Connection, to: &'b mut Connection) -> Result<Backup<'a, 'b>> {
        Backup::new_with_names(from, MAIN_DB, to, MAIN_DB)
    }

    /// Attempt to create a new handle that will allow backups from the
    /// `from_name` database of `from` to the `to_name` database of `to`. Note
    /// that `to` is a `&mut` - this is because SQLite forbids any API calls on
    /// the destination of a backup while the backup is taking place.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying `sqlite3_backup_init` call returns
    /// `NULL`.
    pub fn new_with_names<'a, 'b, F: Name, T: Name>(
        from: &'a Connection,
        from_name: F,
        to: &'b mut Connection,
        to_name: T,
    ) -> Result<Backup<'a, 'b>> {
        let to_name = to_name.as_cstr()?;
        let from_name = from_name.as_cstr()?;

        let to_db = to.db.borrow_mut().db;

        let b = unsafe {
            let b = ffi::sqlite3_backup_init(
                to_db,
                to_name.as_ptr(),
                from.db.borrow_mut().db,
                from_name.as_ptr(),
            );
            if b.is_null() {
                return Err(error_from_handle(to_db, ffi::sqlite3_errcode(to_db)));
            }
            b
        };

        Ok(Backup {
            phantom_from: PhantomData,
            to,
            b,
        })
    }

    /// Gets the progress of the backup as of the last call to
    /// [`step`](Backup::step).
    #[inline]
    #[must_use]
    pub fn progress(&self) -> Progress {
        unsafe {
            Progress {
                remaining: ffi::sqlite3_backup_remaining(self.b),
                pagecount: ffi::sqlite3_backup_pagecount(self.b),
            }
        }
    }

    /// Attempts to back up the given number of pages. If `num_pages` is
    /// negative, will attempt to back up all remaining pages. This will hold a
    /// lock on the source database for the duration, so it is probably not
    /// what you want for databases that are currently active (see
    /// [`run_to_completion`](Backup::run_to_completion) for a better
    /// alternative).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying `sqlite3_backup_step` call returns
    /// an error code other than `DONE`, `OK`, `BUSY`, or `LOCKED`. `BUSY` and
    /// `LOCKED` are transient errors and are therefore returned as possible
    /// `Ok` values.
    #[inline]
    pub fn step(&self, num_pages: c_int) -> Result<StepResult> {
        use self::StepResult::{Busy, Done, Locked, More};

        let rc = unsafe { ffi::sqlite3_backup_step(self.b, num_pages) };
        match rc {
            ffi::SQLITE_DONE => Ok(Done),
            ffi::SQLITE_OK => Ok(More),
            ffi::SQLITE_BUSY => Ok(Busy),
            ffi::SQLITE_LOCKED => Ok(Locked),
            _ => self.to.decode_result(rc).map(|()| More),
        }
    }

    /// Attempts to run the entire backup. Will call
    /// [`step(pages_per_step)`](Backup::step) as many times as necessary,
    /// sleeping for `pause_between_pages` between each call to give the
    /// source database time to process any pending queries. This is a
    /// direct implementation of "Example 2: Online Backup of a Running
    /// Database" from [SQLite's Online Backup API documentation](https://www.sqlite.org/backup.html).
    ///
    /// If `progress` is not `None`, it will be called after each step with the
    /// current progress of the backup. Note that is possible the progress may
    /// not change if the step returns `Busy` or `Locked` even though the
    /// backup is still running.
    ///
    /// # Failure
    ///
    /// Will return `Err` if any of the calls to [`step`](Backup::step) return
    /// `Err`.
    pub fn run_to_completion(
        &self,
        pages_per_step: c_int,
        pause_between_pages: Duration,
        progress: Option<fn(Progress)>,
    ) -> Result<()> {
        use self::StepResult::{Busy, Done, Locked, More};

        assert!(pages_per_step > 0, "pages_per_step must be positive");

        loop {
            let r = self.step(pages_per_step)?;
            if let Some(progress) = progress {
                progress(self.progress());
            }
            match r {
                More | Busy | Locked => thread::sleep(pause_between_pages),
                Done => return Ok(()),
            }
        }
    }

    /// Attempts to run the entire backup, like
    /// [`run_to_completion`](Backup::run_to_completion), but with a callback
    /// that can observe every step and stop the run, and with an upper bound
    /// on consecutive lock conflicts.
    ///
    /// Calls [`step(pages_per_step)`](Backup::step) repeatedly, sleeping for
    /// `pause_between_pages` (which may be zero) between attempts. After every
    /// attempt that returns [`StepResult::More`], [`StepResult::Busy`],
    /// [`StepResult::Locked`] or [`StepResult::Done`], `callback` is invoked
    /// exactly once with that result and the progress as of this attempt, and
    /// its return value decides whether the run continues. The callback may
    /// borrow and mutate local state (`FnMut`).
    ///
    /// If the callback returns [`BackupControl::Abort`], the run stops
    /// immediately - without sleeping or copying further pages - and
    /// [`BackupRunOutcome::Aborted`] is returned, unless the attempt just made
    /// returned [`StepResult::Done`], in which case the backup is already
    /// complete and [`BackupRunOutcome::Done`] is returned.
    ///
    /// [`StepResult::Busy`] and [`StepResult::Locked`] share a single
    /// consecutive-conflict counter: the first conflict counts as one, and a
    /// [`StepResult::More`] attempt resets the counter to zero. When the
    /// counter reaches `max_consecutive_lock_conflicts`, the callback is still
    /// notified of that attempt; if it asks to continue, the `Busy`/`Locked`
    /// SQLite error of this attempt is returned instead of making another
    /// attempt. The counter is reset on every call of this method. This limit
    /// only bounds the attempts of this run - it does not change the busy
    /// timeout or any other setting of the underlying connections.
    ///
    /// Aborting or hitting the conflict limit only ends this automatic run:
    /// the [`Backup`] handle remains usable for manual
    /// [`step`](Backup::step) calls or another automatic run.
    ///
    /// Note that the reported progress faithfully reflects SQLite's own page
    /// counts: if another connection modifies the source database between
    /// steps, `remaining`/`pagecount` may grow again, and the run is not
    /// considered complete until [`step`](Backup::step) itself reports
    /// [`StepResult::Done`].
    ///
    /// # Failure
    ///
    /// Will return `Err` - without copying any pages or invoking `callback` -
    /// if `pages_per_step` or `max_consecutive_lock_conflicts` is not
    /// positive. Will also return `Err` if any of the calls to
    /// [`step`](Backup::step) return `Err`; such errors are returned as-is,
    /// without notifying `callback` or retrying.
    pub fn run_to_completion_with_callback<F>(
        &self,
        pages_per_step: c_int,
        pause_between_pages: Duration,
        max_consecutive_lock_conflicts: c_int,
        mut callback: F,
    ) -> Result<BackupRunOutcome>
    where
        F: FnMut(StepResult, Progress) -> BackupControl,
    {
        use self::StepResult::{Busy, Done, Locked, More};

        if pages_per_step <= 0 {
            return Err(Error::InvalidParameter(format!(
                "pages_per_step must be positive, got {pages_per_step}"
            )));
        }
        if max_consecutive_lock_conflicts <= 0 {
            return Err(Error::InvalidParameter(format!(
                "max_consecutive_lock_conflicts must be positive, got {max_consecutive_lock_conflicts}"
            )));
        }

        let mut consecutive_lock_conflicts = 0;
        loop {
            let r = self.step(pages_per_step)?;
            let control = callback(r, self.progress());
            match r {
                Done => return Ok(BackupRunOutcome::Done),
                More => {
                    consecutive_lock_conflicts = 0;
                    if control == BackupControl::Abort {
                        return Ok(BackupRunOutcome::Aborted);
                    }
                }
                Busy | Locked => {
                    consecutive_lock_conflicts += 1;
                    if control == BackupControl::Abort {
                        return Ok(BackupRunOutcome::Aborted);
                    }
                    if consecutive_lock_conflicts >= max_consecutive_lock_conflicts {
                        let code = if r == Busy {
                            ffi::SQLITE_BUSY
                        } else {
                            ffi::SQLITE_LOCKED
                        };
                        return Err(unsafe { error_from_handle(ptr::null_mut(), code) });
                    }
                }
            }
            thread::sleep(pause_between_pages);
        }
    }
}

impl Drop for Backup<'_, '_> {
    #[inline]
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_backup_finish(self.b) };
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::{Backup, BackupControl, BackupRunOutcome, NO_PROGRESS, Progress, StepResult};
    use crate::{Connection, Error, ErrorCode, MAIN_DB, Result, TEMP_DB};
    use std::time::Duration;

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn backup_to_path() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("test.db3");

        fn progress(_: Progress) {}

        src.backup(MAIN_DB, path.as_path(), Some(progress))?;
        src.backup(MAIN_DB, path.as_path(), NO_PROGRESS)?;

        let mut dst = Connection::open_in_memory()?;
        dst.restore(MAIN_DB, path.as_path(), Some(progress))?;
        dst.restore(MAIN_DB, path, NO_PROGRESS)?;

        Ok(())
    }

    #[test]
    fn test_backup() -> Result<()> {
        let src = Connection::open_in_memory()?;
        let sql = "BEGIN;
                   CREATE TABLE foo(x INTEGER);
                   INSERT INTO foo VALUES(42);
                   END;";
        src.execute_batch(sql)?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            backup.step(-1)?;
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);

        src.execute_batch("INSERT INTO foo VALUES(43)")?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            backup.run_to_completion(5, Duration::from_millis(250), None)?;
        }

        let the_answer: i64 = dst.one_column("SELECT SUM(x) FROM foo", [])?;
        assert_eq!(42 + 43, the_answer);
        Ok(())
    }

    #[test]
    fn test_backup_temp() -> Result<()> {
        let src = Connection::open_in_memory()?;
        let sql = "BEGIN;
                   CREATE TEMPORARY TABLE foo(x INTEGER);
                   INSERT INTO foo VALUES(42);
                   END;";
        src.execute_batch(sql)?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new_with_names(&src, TEMP_DB, &mut dst, MAIN_DB)?;
            backup.step(-1)?;
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);

        src.execute_batch("INSERT INTO foo VALUES(43)")?;

        {
            let backup = Backup::new_with_names(&src, TEMP_DB, &mut dst, MAIN_DB)?;
            backup.run_to_completion(5, Duration::from_millis(250), None)?;
        }

        let the_answer: i64 = dst.one_column("SELECT SUM(x) FROM foo", [])?;
        assert_eq!(42 + 43, the_answer);
        Ok(())
    }

    #[test]
    fn test_backup_attached() -> Result<()> {
        let src = Connection::open_in_memory()?;
        let sql = "ATTACH DATABASE ':memory:' AS my_attached;
                   BEGIN;
                   CREATE TABLE my_attached.foo(x INTEGER);
                   INSERT INTO my_attached.foo VALUES(42);
                   END;";
        src.execute_batch(sql)?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new_with_names(&src, c"my_attached", &mut dst, MAIN_DB)?;
            backup.step(-1)?;
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);

        src.execute_batch("INSERT INTO foo VALUES(43)")?;

        {
            let backup = Backup::new_with_names(&src, c"my_attached", &mut dst, MAIN_DB)?;
            backup.run_to_completion(5, Duration::from_millis(250), None)?;
        }

        let the_answer: i64 = dst.one_column("SELECT SUM(x) FROM foo", [])?;
        assert_eq!(42 + 43, the_answer);
        Ok(())
    }

    #[test]
    fn test_run_to_completion_with_callback_done() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch(
            "BEGIN;
             CREATE TABLE foo(x INTEGER);
             INSERT INTO foo VALUES(42);
             END;",
        )?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            let mut results = Vec::new();
            let outcome = backup.run_to_completion_with_callback(
                1,
                Duration::ZERO,
                3,
                |result, progress| {
                    results.push((result, progress.remaining, progress.pagecount));
                    BackupControl::Continue
                },
            )?;
            assert_eq!(BackupRunOutcome::Done, outcome);
            // every attempt notified the callback exactly once, ending with Done
            assert_eq!(Some(&StepResult::Done), results.last().map(|r| &r.0));
            assert!(results[..results.len() - 1]
                .iter()
                .all(|r| r.0 == StepResult::More));
            // the final notification reports no remaining pages
            assert_eq!(0, results.last().unwrap().1);
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_run_to_completion_with_callback_abort_and_resume() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch(
            "BEGIN;
             CREATE TABLE foo(x INTEGER);
             INSERT INTO foo VALUES(42);
             END;",
        )?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new(&src, &mut dst)?;

            // abort on the very first notification
            let mut calls = 0;
            let outcome = backup.run_to_completion_with_callback(
                1,
                Duration::from_millis(1),
                3,
                |_, _| {
                    calls += 1;
                    BackupControl::Abort
                },
            )?;
            assert_eq!(BackupRunOutcome::Aborted, outcome);
            assert_eq!(1, calls);

            // the same handle can be driven to completion afterwards
            let outcome =
                backup.run_to_completion_with_callback(5, Duration::ZERO, 3, |_, _| {
                    BackupControl::Continue
                })?;
            assert_eq!(BackupRunOutcome::Done, outcome);
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_run_to_completion_with_callback_done_beats_abort() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            // the whole database fits in a single step, so the first (and
            // only) attempt returns Done; aborting cannot undo that
            let outcome = backup.run_to_completion_with_callback(
                1000,
                Duration::ZERO,
                3,
                |result, _| {
                    assert_eq!(StepResult::Done, result);
                    BackupControl::Abort
                },
            )?;
            assert_eq!(BackupRunOutcome::Done, outcome);
        }

        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_run_to_completion_with_callback_invalid_params() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;

        let mut dst = Connection::open_in_memory()?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            for (pages, limit) in [(0, 3), (-1, 3), (5, 0), (5, -2)] {
                let mut calls = 0;
                let err = backup
                    .run_to_completion_with_callback(
                        pages,
                        Duration::ZERO,
                        limit,
                        |_, _| {
                            calls += 1;
                            BackupControl::Continue
                        },
                    )
                    .unwrap_err();
                assert!(
                    matches!(err, Error::InvalidParameter(_)),
                    "expected InvalidParameter, got {err:?}"
                );
                assert_eq!(0, calls, "callback must not run on invalid parameters");
            }
        }

        // nothing was copied
        assert!(dst.one_column::<i64, _>(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'foo'",
            [],
        ).is_ok_and(|n| n == 0));
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_run_to_completion_with_callback_busy_limit() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let src_path = temp_dir.path().join("src.db3");
        let dst_path = temp_dir.path().join("dst.db3");

        let src = Connection::open(&src_path)?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;

        let mut dst = Connection::open(&dst_path)?;

        // hold a write lock on the destination from another connection so
        // every backup attempt fails with Busy
        let blocker = Connection::open(&dst_path)?;
        blocker.execute_batch("BEGIN EXCLUSIVE; CREATE TABLE held(y);")?;

        {
            let backup = Backup::new(&src, &mut dst)?;

            // limit of 3: the third consecutive Busy is reported to the
            // callback, then returned as an error without a fourth attempt
            let mut calls = 0;
            let err = backup
                .run_to_completion_with_callback(5, Duration::ZERO, 3, |result, _| {
                    calls += 1;
                    assert_eq!(StepResult::Busy, result);
                    BackupControl::Continue
                })
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    Error::SqliteFailure(ref e, _) if e.code == ErrorCode::DatabaseBusy
                ),
                "expected SQLITE_BUSY, got {err:?}"
            );
            assert_eq!(3, calls);

            // aborting on a conflict reports Aborted instead of the error
            let outcome =
                backup.run_to_completion_with_callback(5, Duration::ZERO, 3, |result, _| {
                    assert_eq!(StepResult::Busy, result);
                    BackupControl::Abort
                })?;
            assert_eq!(BackupRunOutcome::Aborted, outcome);

            // once the lock is gone the same handle completes; the conflict
            // counter restarts with each call
            blocker.execute_batch("ROLLBACK;")?;
            let outcome =
                backup.run_to_completion_with_callback(5, Duration::ZERO, 3, |_, _| {
                    BackupControl::Continue
                })?;
            assert_eq!(BackupRunOutcome::Done, outcome);
        }

        // the destination connection is usable again once the backup is dropped
        assert_eq!(42, dst.one_column::<i64, _>("SELECT x FROM foo", [])?);
        dst.execute_batch("INSERT INTO foo VALUES(43)")?;
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn test_run_to_completion_with_callback_aborted_backup_untouched() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let src_path = temp_dir.path().join("src.db3");
        let dst_path = temp_dir.path().join("dst.db3");

        let src = Connection::open(&src_path)?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;

        let mut dst = Connection::open(&dst_path)?;
        dst.execute_batch("CREATE TABLE original(y INTEGER); INSERT INTO original VALUES(7);")?;

        {
            let backup = Backup::new(&src, &mut dst)?;
            let outcome =
                backup.run_to_completion_with_callback(1, Duration::ZERO, 3, |_, _| {
                    BackupControl::Abort
                })?;
            assert_eq!(BackupRunOutcome::Aborted, outcome);
        }

        // dropping an unfinished backup leaves the destination's own schema
        // and data alone, and the connection keeps working
        assert_eq!(
            7,
            dst.one_column::<i64, _>("SELECT y FROM original", [])?
        );
        dst.execute_batch("INSERT INTO original VALUES(8)")?;
        assert_eq!(
            2,
            dst.one_column::<i64, _>("SELECT COUNT(*) FROM original", [])?
        );
        Ok(())
    }
}
