//! Online SQLite backup API.
//!
//! Alternatively, you can create a backup with a simple
//! [`VACUUM INTO <backup_path>`](https://sqlite.org/lang_vacuum.html#vacuuminto).
//!
//! To create a [`Backup`], you must have two distinct [`Connection`]s - one
//! for the source (which can be used while the backup is running) and one for
//! the destination (which cannot).  A [`Backup`] handle exposes four methods:
//! [`step`](Backup::step) will attempt to back up a specified number of pages,
//! [`progress`](Backup::progress) gets the current progress of the backup as of
//! the last call to [`step`](Backup::step),
//! [`run_to_completion`](Backup::run_to_completion) will attempt to back up the
//! entire source database, allowing you to specify how many pages are backed up
//! at a time and how long the thread should sleep between chunks of pages, and
//! [`run_to_completion_with_progress`](Backup::run_to_completion_with_progress)
//! does the same while passing every attempt to a caller-provided closure that
//! may borrow and mutate local state, may request that the run stop early, and
//! bounds the number of consecutive lock conflicts before giving up.
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

use crate::error::{Error, error_from_handle};
use crate::{Connection, MAIN_DB, Name, Result};

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

/// Outcome of
/// [`Backup::run_to_completion_with_progress`]: the backup ran to completion,
/// or the caller's closure asked to stop before it had finished.
///
/// Database errors are reported as `Err` and therefore do not appear here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackupStatus {
    /// The backup is complete.
    Completed,
    /// The run was stopped because the progress closure returned `false`.
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

    /// Like [`run_to_completion`](Backup::run_to_completion), but instead of
    /// waiting forever when the source is busy or locked, this method lets the
    /// caller observe every attempt and abort the run.
    ///
    /// The backup is driven by repeated calls to
    /// [`step(pages_per_step)`](Backup::step), sleeping for
    /// `pause_between_pages` between attempts (a duration of zero is allowed).
    /// After each attempt, `progress` is called exactly once with the
    /// attempt's [`StepResult`] and the [`Progress`] at that moment; unlike the
    /// callback of [`run_to_completion`](Backup::run_to_completion), it may be
    /// a closure that borrows and mutates its environment. It returns `true`
    /// to keep going or `false` to stop:
    ///
    /// - [`Done`](StepResult::Done) means the backup has finished; the return
    ///   value of the closure is ignored and [`BackupStatus::Completed`] is
    ///   returned even if it returned `false`.
    /// - [`More`](StepResult::More) resets the consecutive lock-conflict
    ///   counter; returning `false` ends the run with
    ///   [`BackupStatus::Aborted`].
    /// - [`Busy`](StepResult::Busy) and [`Locked`](StepResult::Locked) share a
    ///   single consecutive-conflict counter, which starts at zero for every
    ///   call to this method: the first conflict counts as one, and
    ///   [`More`](StepResult::More) clears it. When the counter reaches
    ///   `max_lock_conflicts` after notifying the closure, returning `false`
    ///   yields [`BackupStatus::Aborted`]; returning `true` yields the
    ///   `SQLITE_BUSY` / `SQLITE_LOCKED` error of that last attempt without
    ///   trying again.
    /// - Any other SQLite error from an attempt is returned immediately,
    ///   as-is, without notifying the closure or retrying.
    ///
    /// Neither an abort nor exhausting the lock-conflict budget consumes the
    /// backup: the [`Backup`] handle stays usable for further manual
    /// [`step`](Backup::step) calls or another run of this method. Only this
    /// method's retry loop ends, and the connection's busy-handler settings are
    /// untouched.
    ///
    /// If the source database is modified by another connection while the
    /// backup is in progress, SQLite may restart it and the reported page
    /// counts may go backwards; they are always passed through verbatim.
    ///
    /// # Failure
    ///
    /// Returns [`Error::InvalidBackupParameter`] if `pages_per_step` or
    /// `max_lock_conflicts` is not positive. In that case no page is copied and
    /// `progress` is not called. Also returns `Err` for any non-transient
    /// SQLite error, or for the lock-conflict limit described above.
    ///
    /// ```rust,no_run
    /// # use rusqlite::{backup, Connection};
    /// # use std::time::Duration;
    /// # fn run(src: &Connection, dst: &mut Connection) -> rusqlite::Result<()> {
    /// use backup::{BackupStatus, StepResult};
    ///
    /// let backup = backup::Backup::new(src, dst)?;
    /// let mut conflicts = 0;
    /// let mut aborted = false;
    /// match backup.run_to_completion_with_progress(
    ///     100,
    ///     Duration::from_millis(100),
    ///     10,
    ///     |result, _progress| {
    ///         match result {
    ///             StepResult::Busy | StepResult::Locked => conflicts += 1,
    ///             _ => {}
    ///         }
    ///         // Give up after at most five attempts.
    ///         conflicts < 5
    ///     },
    /// )? {
    ///     BackupStatus::Completed => {}
    ///     BackupStatus::Aborted => aborted = true,
    ///     // `BackupStatus` is `#[non_exhaustive]`.
    ///     _ => {}
    /// }
    /// # let _ = aborted;
    /// # Ok(())
    /// # }
    /// ```
    pub fn run_to_completion_with_progress<F>(
        &self,
        pages_per_step: c_int,
        pause_between_pages: Duration,
        max_lock_conflicts: c_int,
        progress: F,
    ) -> Result<BackupStatus>
    where
        F: FnMut(StepResult, Progress) -> bool,
    {
        if pages_per_step <= 0 {
            return Err(Error::InvalidBackupParameter(format!(
                "pages_per_step must be positive, got {pages_per_step}"
            )));
        }
        if max_lock_conflicts <= 0 {
            return Err(Error::InvalidBackupParameter(format!(
                "max_lock_conflicts must be positive, got {max_lock_conflicts}"
            )));
        }

        run_auto(
            pages_per_step,
            pause_between_pages,
            max_lock_conflicts,
            progress,
            |n| self.step(n),
            || self.progress(),
            thread::sleep,
        )
    }
}

/// State machine backing [`Backup::run_to_completion_with_progress`].
///
/// It is generic over the individual attempt, the progress query, and the
/// sleep so that the retry/abort/counting rules can be exercised
/// deterministically without contriving SQLite lock contention.
fn run_auto<F, A, P, S>(
    pages_per_step: c_int,
    pause_between_pages: Duration,
    max_lock_conflicts: c_int,
    mut progress: F,
    mut attempt: A,
    current_progress: P,
    mut sleep: S,
) -> Result<BackupStatus>
where
    F: FnMut(StepResult, Progress) -> bool,
    A: FnMut(c_int) -> Result<StepResult>,
    P: Fn() -> Progress,
    S: FnMut(Duration),
{
    use self::StepResult::{Busy, Done, Locked, More};

    let mut lock_conflicts = 0;
    loop {
        // One attempt, one notification: other errors propagate before the
        // closure is ever called.
        let r = attempt(pages_per_step)?;
        let should_continue = progress(r, current_progress());

        match r {
            // A completed backup is completed regardless of the closure vote.
            Done => return Ok(BackupStatus::Completed),
            More => {
                lock_conflicts = 0;
                if !should_continue {
                    return Ok(BackupStatus::Aborted);
                }
                sleep(pause_between_pages);
            }
            Busy | Locked => {
                lock_conflicts += 1;
                if !should_continue {
                    return Ok(BackupStatus::Aborted);
                }
                if lock_conflicts >= max_lock_conflicts {
                    let code = if r == Busy {
                        ffi::SQLITE_BUSY
                    } else {
                        ffi::SQLITE_LOCKED
                    };
                    return Err(unsafe { error_from_handle(ptr::null_mut(), code) });
                }
                sleep(pause_between_pages);
            }
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

    use super::{Backup, BackupStatus, NO_PROGRESS, Progress, StepResult, run_auto};
    use crate::ffi;
    use crate::{Connection, Error, MAIN_DB, Result, TEMP_DB};
    use std::collections::VecDeque;
    use std::ffi::c_int;
    use std::time::Duration;

    use self::StepResult::{Busy, Done, Locked, More};

    fn prog(remaining: c_int, pagecount: c_int) -> Progress {
        Progress {
            remaining,
            pagecount,
        }
    }

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

    // ---------- run_auto state-machine tests (deterministic mocks) ----------

    /// Runs `run_auto` with a scripted sequence of attempt results and
    /// progress values. Sleeps are counted rather than performed.
    fn scripted_run(
        pages: c_int,
        max_conflicts: c_int,
        script: &[StepResult],
        mut progress: impl FnMut(StepResult, Progress) -> bool,
    ) -> (Result<BackupStatus>, Vec<(StepResult, Progress)>, usize) {
        use std::cell::Cell;
        let mut steps: VecDeque<StepResult> = script.iter().copied().collect();
        let mut calls = Vec::new();
        let mut sleeps = 0;
        let pages_done = Cell::new(0);
        let result = run_auto(
            pages,
            Duration::from_secs(100),
            max_conflicts,
            |r, p| {
                calls.push((r, p));
                progress(r, p)
            },
            |n| {
                pages_done.set(pages_done.get() + n);
                Ok(steps.pop_front().expect("attempt called past script end"))
            },
            || Progress {
                remaining: 10 - pages_done.get(),
                pagecount: 10,
            },
            |_| sleeps += 1,
        );
        (result, calls, sleeps)
    }

    #[test]
    fn run_auto_completes_and_stops_on_done() {
        // Closure asks to stop at Done, but completion wins regardless.
        let (result, calls, sleeps) = scripted_run(2, 3, &[More, Done], |r, _| r != Done);
        assert_eq!(result.unwrap(), BackupStatus::Completed);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, Done);
        assert_eq!(sleeps, 1); // slept once, after More
    }

    #[test]
    fn run_auto_aborts_on_more_when_closure_stops() {
        let (result, calls, _) = scripted_run(2, 3, &[More, Done], |r, _| r != More);
        assert_eq!(result.unwrap(), BackupStatus::Aborted);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, More);
    }

    #[test]
    fn run_auto_passes_through_progress() {
        let (_, calls, _) = scripted_run(3, 3, &[More, More, Done], |_, _| true);
        assert_eq!(
            calls
                .iter()
                .map(|c| (c.1.remaining, c.1.pagecount))
                .collect::<Vec<_>>(),
            vec![(7, 10), (4, 10), (1, 10)]
        );
    }

    #[test]
    fn run_auto_busy_limit_three_then_errors_after_closure_vote() {
        // Closure wants to keep going: third busy attempt yields SQLITE_BUSY.
        let (result, calls, sleeps) = scripted_run(2, 3, &[Busy, Busy, Busy], |_, _| true);
        let err = result.unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ffi::ErrorCode::DatabaseBusy));
        assert_eq!(calls.len(), 3, "each attempt notifies exactly once");
        // Only the first two conflicts are slept on.
        assert_eq!(sleeps, 2);
        assert_eq!(calls.iter().filter(|c| c.0 == Busy).count(), 3);
    }

    #[test]
    fn run_auto_busy_limit_abort_on_last_conflict() {
        // Closure stops on the third busy attempt: abort, no error.
        let mut seen = 0;
        let (result, calls, sleeps) = scripted_run(2, 3, &[Busy, Busy, Busy], move |_, _| {
            seen += 1;
            seen < 3
        });
        assert_eq!(result.unwrap(), BackupStatus::Aborted);
        assert_eq!(calls.len(), 3);
        assert_eq!(sleeps, 2);
    }

    #[test]
    fn run_auto_abort_on_second_conflict() {
        let mut seen = 0;
        let (result, calls, _) = scripted_run(2, 5, &[Busy, Locked, Done], move |_, _| {
            seen += 1;
            seen < 2
        });
        assert_eq!(result.unwrap(), BackupStatus::Aborted);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, Busy);
        assert_eq!(calls[1].0, Locked);
    }

    #[test]
    fn run_auto_busy_and_locked_share_counter() {
        // 1 busy + 1 locked + 1 busy reaches the limit of 3.
        let (result, calls, sleeps) = scripted_run(1, 3, &[Busy, Locked, Busy], |_, _| true);
        let err = result.unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ffi::ErrorCode::DatabaseBusy));
        assert_eq!(calls.len(), 3);
        assert_eq!(sleeps, 2);
    }

    #[test]
    fn run_auto_more_resets_conflict_counter() {
        // Two busies, a More (resets), then three busies: only the third
        // consecutive post-reset conflict trips the limit of three.
        let script = [Busy, Busy, More, Busy, Busy, Busy];
        let (result, calls, sleeps) = scripted_run(1, 3, &script, |_, _| true);
        let err = result.unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ffi::ErrorCode::DatabaseBusy));
        assert_eq!(calls.len(), 6);
        // Slept after each conflict under the limit (2 + 2) and after More.
        assert_eq!(sleeps, 5);
    }

    #[test]
    fn run_auto_locked_limit_error_is_locked() {
        let (result, calls, _) = scripted_run(1, 2, &[Locked, Locked], |_, _| true);
        let err = result.unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(ffi::ErrorCode::DatabaseLocked)
        );
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn run_auto_other_error_propagates_immediately() {
        let other = |_| {
            Err(crate::error::error_from_sqlite_code(
                ffi::SQLITE_READONLY,
                None,
            ))
        };
        let mut notified = 0;
        let result = run_auto(
            1,
            Duration::ZERO,
            3,
            |_, _| {
                notified += 1;
                true
            },
            other,
            || prog(1, 1),
            |_| panic!("must not sleep after an error"),
        );
        let err = result.unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ffi::ErrorCode::ReadOnly));
        assert_eq!(notified, 0, "closure must not run for other errors");
    }

    #[test]
    fn run_auto_sleeps_zero_is_allowed() {
        let mut attempts = 0;
        let result = run_auto(
            1,
            Duration::ZERO,
            1,
            |_, _| true,
            |_| {
                attempts += 1;
                Ok(if attempts < 50 { More } else { Done })
            },
            || prog(0, 0),
            |_| {},
        );
        assert_eq!(result.unwrap(), BackupStatus::Completed);
        assert_eq!(attempts, 50);
    }

    // ---------- tests using real SQLite connections ----------

    #[test]
    fn auto_run_happy_path_with_mutating_closure() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch(
            "CREATE TABLE foo(x);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 200)
             INSERT INTO foo SELECT randomblob(400) FROM n;",
        )?;
        let mut dst = Connection::open_in_memory()?;
        let backup = Backup::new(&src, &mut dst)?;

        let mut observed = Vec::new();
        let mut attempts = 0;
        let status = backup.run_to_completion_with_progress(10, Duration::ZERO, 3, |r, p| {
            attempts += 1;
            observed.push((r, p.remaining, p.pagecount));
            true
        })?;
        assert_eq!(status, BackupStatus::Completed);
        assert!(attempts >= 1);
        assert_eq!(observed.last().unwrap().0, Done);
        assert_eq!(observed.last().unwrap().1, 0, "Done reports no pages left");
        // Every notification is one attempt with monotone progress here.
        for w in observed.windows(2) {
            assert!(w[1].1 <= w[0].1);
        }

        drop(backup);
        let n: i64 = dst.one_column("SELECT COUNT(*) FROM foo", [])?;
        assert_eq!(n, 200);
        Ok(())
    }

    #[test]
    fn auto_run_abort_leaves_backup_and_destination_reusable() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch(
            "CREATE TABLE foo(x);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 300)
             INSERT INTO foo SELECT randomblob(500) FROM n;",
        )?;
        let mut dst = Connection::open_in_memory()?;
        dst.execute_batch("CREATE TABLE orig(x); INSERT INTO orig VALUES(1);")?;
        let backup = Backup::new(&src, &mut dst)?;

        // Abort after the first More.
        let status =
            backup.run_to_completion_with_progress(5, Duration::ZERO, 2, |r, _| r == Busy)?;
        assert_eq!(status, BackupStatus::Aborted);

        // The same handle can be driven manually to completion.
        while backup.step(50)? != Done {}
        drop(backup);

        let n: i64 = dst.one_column("SELECT COUNT(*) FROM foo", [])?;
        assert_eq!(n, 300);
        // Destination connection usable again; SQLite replaces the database on
        // a completed backup, so check only that reads/writes work.
        dst.execute_batch("INSERT INTO foo VALUES(randomblob(1))")?;
        let n2: i64 = dst.one_column("SELECT COUNT(*) FROM foo", [])?;
        assert_eq!(n2, 301);
        Ok(())
    }

    #[test]
    fn auto_run_abort_then_autorun_again_completes() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch(
            "CREATE TABLE foo(x);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 100)
             INSERT INTO foo SELECT randomblob(500) FROM n;",
        )?;
        let mut dst = Connection::open_in_memory()?;
        let backup = Backup::new(&src, &mut dst)?;

        let status = backup.run_to_completion_with_progress(
            1,
            Duration::ZERO,
            3,
            |_, _| false, // stop at the very first attempt
        )?;
        assert_eq!(status, BackupStatus::Aborted);

        // Counter resets each call; the same handle completes on a second run.
        let status =
            backup.run_to_completion_with_progress(1000, Duration::ZERO, 3, |_, _| true)?;
        assert_eq!(status, BackupStatus::Completed);
        drop(backup);
        let n: i64 = dst.one_column("SELECT COUNT(*) FROM foo", [])?;
        assert_eq!(n, 100);
        Ok(())
    }

    #[test]
    fn auto_run_invalid_parameters_do_nothing() -> Result<()> {
        let src = Connection::open_in_memory()?;
        src.execute_batch("CREATE TABLE foo AS SELECT 42 AS x")?;
        let mut dst = Connection::open_in_memory()?;
        let backup = Backup::new(&src, &mut dst)?;

        for bad_pages in [0, -1, i32::MIN] {
            let mut calls = 0;
            let err = backup
                .run_to_completion_with_progress(bad_pages, Duration::ZERO, 3, |_, _| {
                    calls += 1;
                    true
                })
                .unwrap_err();
            assert!(
                matches!(err, Error::InvalidBackupParameter(_)),
                "got {err:?} for pages {bad_pages}"
            );
            assert_eq!(calls, 0);
        }
        for bad_limit in [0, -1, -100] {
            let mut calls = 0;
            let err = backup
                .run_to_completion_with_progress(5, Duration::ZERO, bad_limit, |_, _| {
                    calls += 1;
                    true
                })
                .unwrap_err();
            assert!(
                matches!(err, Error::InvalidBackupParameter(_)),
                "got {err:?} for limit {bad_limit}"
            );
            assert_eq!(calls, 0);
        }

        // Handle still usable.
        assert_eq!(backup.step(-1)?, Done);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn auto_run_lock_conflict_limit_and_recovery() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let srcp = temp_dir.path().join("src.db");
        let dstp = temp_dir.path().join("dst.db");

        let src = Connection::open(&srcp)?;
        src.execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE t(x);")?;
        src.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 400)
             INSERT INTO t SELECT randomblob(800) FROM n;",
        )?;
        let mut dst = Connection::open(&dstp)?;
        dst.execute_batch("PRAGMA journal_mode=DELETE;")?;

        let blocker = Connection::open(&dstp)?;
        blocker.execute_batch("BEGIN IMMEDIATE")?;

        let backup = Backup::new(&src, &mut dst)?;
        let mut seen_busy = 0;
        let err = backup
            .run_to_completion_with_progress(5, Duration::ZERO, 3, |r, _| {
                if r == Busy {
                    seen_busy += 1;
                }
                true
            })
            .unwrap_err();
        assert_eq!(err.sqlite_error_code(), Some(ffi::ErrorCode::DatabaseBusy));
        assert_eq!(seen_busy, 3);

        // The same backup object can be reused once the lock is released.
        drop(blocker);
        let status = backup.run_to_completion_with_progress(500, Duration::ZERO, 3, |_, _| true)?;
        assert_eq!(status, BackupStatus::Completed);
        drop(backup);

        let check = Connection::open(&dstp)?;
        let n: i64 = check.one_column("SELECT COUNT(*) FROM t", [])?;
        assert_eq!(n, 400);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn auto_run_abort_keeps_original_destination() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let srcp = temp_dir.path().join("src.db");
        let dstp = temp_dir.path().join("dst.db");

        let src = Connection::open(&srcp)?;
        src.execute_batch(
            "CREATE TABLE t(x);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 400)
             INSERT INTO t SELECT randomblob(800) FROM n;",
        )?;
        let mut dst = Connection::open(&dstp)?;
        dst.execute_batch("CREATE TABLE keepme(x); INSERT INTO keepme VALUES (123);")?;

        // Partial progress first.
        let backup = Backup::new(&src, &mut dst)?;
        assert_eq!(backup.step(5)?, More);

        // Abort the automatic run after the next successful partial step.
        let status =
            backup.run_to_completion_with_progress(5, Duration::ZERO, 3, |r, _| r != More)?;
        assert_eq!(status, BackupStatus::Aborted);
        drop(backup);

        // Unfinished backup on release: original tables/rows survive, and the
        // destination is queryable and writable.
        assert_eq!(123, dst.one_column::<i64, _>("SELECT x FROM keepme", [])?);
        dst.execute_batch("INSERT INTO keepme VALUES (456)")?;
        let n: i64 = dst.one_column("SELECT COUNT(*) FROM keepme", [])?;
        assert_eq!(n, 2);
        Ok(())
    }

    #[cfg_attr(
        all(target_family = "wasm", target_os = "unknown"),
        ignore = "no filesystem on this platform"
    )]
    #[test]
    fn auto_run_reports_restart_progress_verbatim() -> Result<()> {
        // A source-side commit mid-backup can restart it, making the remaining
        // page count grow. The run must pass SQLite's numbers through and
        // still finish with a consistent full backup.
        let temp_dir = tempfile::tempdir().unwrap();
        let srcp = temp_dir.path().join("src.db");
        let dstp = temp_dir.path().join("dst.db");

        let src = Connection::open(&srcp)?;
        src.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t(x);")?;
        src.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 400)
             INSERT INTO t SELECT randomblob(800) FROM n;",
        )?;
        let mut dst = Connection::open(&dstp)?;

        let backup = Backup::new(&src, &mut dst)?;
        assert_eq!(backup.step(20)?, More);
        let first = backup.progress();

        // Another connection commits new pages, then drive the auto run with a
        // closure that observes the potentially-restarted progress.
        let writer = Connection::open(&srcp)?;
        writer.execute_batch(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n LIMIT 100)
             INSERT INTO t SELECT randomblob(800) FROM n;",
        )?;
        // Ensure the writer's changes are visible and WAL checkpointed so the
        // backup pagecount is forced to grow.
        writer.query_row::<i64, _, _>("SELECT COUNT(*) FROM t", [], |r| r.get(0))?;
        drop(writer);

        let mut saw_restart = false;
        let mut last_pagecount = first.pagecount;
        let status = backup.run_to_completion_with_progress(5, Duration::ZERO, 3, |r, p| {
            if p.pagecount > last_pagecount {
                saw_restart = true;
            }
            if r == More && p.remaining >= p.pagecount {
                // Remaining caught back up to the new total: restart seen.
                saw_restart = true;
            }
            last_pagecount = last_pagecount.max(p.pagecount);
            true
        })?;
        assert_eq!(status, BackupStatus::Completed);
        assert!(
            saw_restart,
            "expected to observe growth in the page count after a concurrent commit"
        );
        drop(backup);

        let check = Connection::open(&dstp)?;
        let n: i64 = check.one_column("SELECT COUNT(*) FROM t", [])?;
        assert_eq!(
            n, 500,
            "backup must contain a consistent post-write snapshot"
        );
        Ok(())
    }
}
