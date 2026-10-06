//! Incremental BLOB I/O.
//!
//! Note that SQLite does not provide API-level access to change the size of a
//! BLOB; that must be performed through SQL statements.
//!
//! There are two choices for how to perform IO on a [`Blob`].
//!
//! 1. The implementations it provides of the `std::io::Read`, `std::io::Write`,
//!    and `std::io::Seek` traits.
//!
//! 2. A positional IO API, e.g. [`Blob::read_at`], [`Blob::write_at`] and
//!    similar.
//!
//! Documenting these in order:
//!
//! ## 1. `std::io` trait implementations.
//!
//! `Blob` conforms to `std::io::Read`, `std::io::Write`, and `std::io::Seek`,
//! so it plays nicely with other types that build on these (such as
//! `std::io::BufReader` and `std::io::BufWriter`). However, you must be careful
//! with the size of the blob. For example, when using a `BufWriter`, the
//! `BufWriter` will accept more data than the `Blob` will allow, so make sure
//! to call `flush` and check for errors. (See the unit tests in this module for
//! an example.)
//!
//! ## 2. Positional IO
//!
//! `Blob`s also offer a `pread` / `pwrite`-style positional IO api in the form
//! of [`Blob::read_at`], [`Blob::write_at`], [`Blob::raw_read_at`],
//! [`Blob::read_at_exact`], and [`Blob::raw_read_at_exact`].
//!
//! These APIs all take the position to read from or write to from as a
//! parameter, instead of using an internal `pos` value.
//!
//! ### Positional IO Read Variants
//!
//! For the `read` functions, there are several functions provided:
//!
//! - [`Blob::read_at`]
//! - [`Blob::raw_read_at`]
//! - [`Blob::read_at_exact`]
//! - [`Blob::raw_read_at_exact`]
//!
//! These can be divided along two axes: raw/not raw, and exact/inexact:
//!
//! 1. Raw/not raw refers to the type of the destination buffer. The raw
//!    functions take a `&mut [MaybeUninit<u8>]` as the destination buffer,
//!    where the "normal" functions take a `&mut [u8]`.
//!
//!    Using `MaybeUninit` here can be more efficient in some cases, but is
//!    often inconvenient, so both are provided.
//!
//! 2. Exact/inexact refers to whether or not the entire buffer must be
//!    filled in order for the call to be considered a success.
//!
//!    The "exact" functions require the provided buffer be entirely filled, or
//!    they return an error, whereas the "inexact" functions read as much out of
//!    the blob as is available, and return how much they were able to read.
//!
//!    The inexact functions are preferable if you do not know the size of the
//!    blob already, and the exact functions are preferable if you do.
//!
//! ### Comparison to using the `std::io` traits:
//!
//! In general, the positional methods offer the following Pro/Cons compared to
//! using the implementation `std::io::{Read, Write, Seek}` we provide for
//! `Blob`:
//!
//! 1. (Pro) There is no need to first seek to a position in order to perform IO
//!    on it as the position is a parameter.
//!
//! 2. (Pro) `Blob`'s positional read functions don't mutate the blob in any
//!    way, and take `&self`. No `&mut` access required.
//!
//! 3. (Pro) Positional IO functions return `Err(rusqlite::Error)` on failure,
//!    rather than `Err(std::io::Error)`. Returning `rusqlite::Error` is more
//!    accurate and convenient.
//!
//!    Note that for the `std::io` API, no data is lost however, and it can be
//!    recovered with `io_err.downcast::<rusqlite::Error>()` (this can be easy
//!    to forget, though).
//!
//! 4. (Pro, for now). A `raw` version of the read API exists which can allow
//!    reading into a `&mut [MaybeUninit<u8>]` buffer, which avoids a potential
//!    costly initialization step. (However, `std::io` traits will certainly
//!    gain this someday, which is why this is only a "Pro, for now").
//!
//! 5. (Con) The set of functions is more bare-bones than what is offered in
//!    `std::io`, which has a number of adapters, handy algorithms, further
//!    traits.
//!
//! 6. (Con) No meaningful interoperability with other crates, so if you need
//!    that you must use `std::io`.
//!
//! To generalize: the `std::io` traits are useful because they conform to a
//! standard interface that a lot of code knows how to handle, however that
//! interface is not a perfect fit for [`Blob`], so another small set of
//! functions is provided as well.
//!
//! # Example (`std::io`)
//!
//! ```rust
//! # use rusqlite::blob::ZeroBlob;
//! # use rusqlite::{Connection, MAIN_DB};
//! # use std::error::Error;
//! # use std::io::{Read, Seek, SeekFrom, Write};
//! # fn main() -> Result<(), Box<dyn Error>> {
//! let db = Connection::open_in_memory()?;
//! db.execute_batch("CREATE TABLE test_table (content BLOB);")?;
//!
//! // Insert a BLOB into the `content` column of `test_table`. Note that the Blob
//! // I/O API provides no way of inserting or resizing BLOBs in the DB -- this
//! // must be done via SQL.
//! db.execute("INSERT INTO test_table (content) VALUES (ZEROBLOB(10))", [])?;
//!
//! // Get the row id off the BLOB we just inserted.
//! let rowid = db.last_insert_rowid();
//! // Open the BLOB we just inserted for IO.
//! let mut blob = db.blob_open(MAIN_DB, "test_table", "content", rowid, false)?;
//!
//! // Write some data into the blob. Make sure to test that the number of bytes
//! // written matches what you expect; if you try to write too much, the data
//! // will be truncated to the size of the BLOB.
//! let bytes_written = blob.write(b"01234567")?;
//! assert_eq!(bytes_written, 8);
//!
//! // Move back to the start and read into a local buffer.
//! // Same guidance - make sure you check the number of bytes read!
//! blob.seek(SeekFrom::Start(0))?;
//! let mut buf = [0u8; 20];
//! let bytes_read = blob.read(&mut buf[..])?;
//! assert_eq!(bytes_read, 10); // note we read 10 bytes because the blob has size 10
//!
//! // Insert another BLOB, this time using a parameter passed in from
//! // rust (potentially with a dynamic size).
//! db.execute(
//!     "INSERT INTO test_table (content) VALUES (?1)",
//!     [ZeroBlob(64)],
//! )?;
//!
//! // given a new row ID, we can reopen the blob on that row
//! let rowid = db.last_insert_rowid();
//! blob.reopen(rowid)?;
//! // Just check that the size is right.
//! assert_eq!(blob.len(), 64);
//! # Ok(())
//! # }
//! ```
//!
//! # Example (Positional)
//!
//! ```rust
//! # use rusqlite::blob::ZeroBlob;
//! # use rusqlite::{Connection, MAIN_DB};
//! # use std::error::Error;
//! # fn main() -> Result<(), Box<dyn Error>> {
//! let db = Connection::open_in_memory()?;
//! db.execute_batch("CREATE TABLE test_table (content BLOB);")?;
//! // Insert a blob into the `content` column of `test_table`. Note that the Blob
//! // I/O API provides no way of inserting or resizing blobs in the DB -- this
//! // must be done via SQL.
//! db.execute("INSERT INTO test_table (content) VALUES (ZEROBLOB(10))", [])?;
//! // Get the row id off the blob we just inserted.
//! let rowid = db.last_insert_rowid();
//! // Open the blob we just inserted for IO.
//! let mut blob = db.blob_open(MAIN_DB, "test_table", "content", rowid, false)?;
//! // Write some data into the blob.
//! blob.write_at(b"ABCDEF", 2)?;
//!
//! // Read the whole blob into a local buffer.
//! let mut buf = [0u8; 10];
//! blob.read_at_exact(&mut buf, 0)?;
//! assert_eq!(&buf, b"\0\0ABCDEF\0\0");
//!
//! // Insert another blob, this time using a parameter passed in from
//! // rust (potentially with a dynamic size).
//! db.execute(
//!     "INSERT INTO test_table (content) VALUES (?1)",
//!     [ZeroBlob(64)],
//! )?;
//!
//! // given a new row ID, we can reopen the blob on that row
//! let rowid = db.last_insert_rowid();
//! blob.reopen(rowid)?;
//! assert_eq!(blob.len(), 64);
//! # Ok(())
//! # }
//! ```
use std::cell::Cell;
use std::cmp::min;
use std::ffi::c_int;
use std::io;
use std::ptr;

use super::ffi;
use super::types::{ToSql, ToSqlOutput};
use crate::{Connection, Error, Name, Result};

mod pos_io;

/// Handle to an open BLOB. See
/// [`rusqlite::blob`](crate::blob) documentation for in-depth discussion.
pub struct Blob<'conn> {
    conn: &'conn Connection,
    blob: *mut ffi::sqlite3_blob,
    // used by std::io implementations,
    pos: i32,
    // Set once the handle has been invalidated: a failed `reopen`, or an
    // I/O call that reported `SQLITE_ABORT` (which is how SQLite reacts to
    // the underlying row being updated or deleted). Once set, every I/O
    // operation must report `SQLITE_ABORT` instead of a result derived
    // from the (now meaningless) cached blob size.
    poisoned: Cell<bool>,
}

impl Connection {
    /// Open a handle to the BLOB located in `row_id`,
    /// `column`, `table` in database `db`.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `db`/`table`/`column` cannot be converted to a
    /// C-compatible string or if the underlying SQLite BLOB open call
    /// fails.
    #[inline]
    pub fn blob_open<D: Name, N: Name>(
        &self,
        db: D,
        table: N,
        column: N,
        row_id: i64,
        read_only: bool,
    ) -> Result<Blob<'_>> {
        let c = self.db.borrow_mut();
        let mut blob = ptr::null_mut();
        let db = db.as_cstr()?;
        let table = table.as_cstr()?;
        let column = column.as_cstr()?;
        let rc = unsafe {
            ffi::sqlite3_blob_open(
                c.db(),
                db.as_ptr(),
                table.as_ptr(),
                column.as_ptr(),
                row_id,
                std::ffi::c_int::from(!read_only),
                &raw mut blob,
            )
        };
        c.decode_result(rc).map(|()| Blob {
            conn: self,
            blob,
            pos: 0,
            poisoned: Cell::new(false),
        })
    }
}

impl Blob<'_> {
    /// Move a BLOB handle to a new row.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite BLOB reopen call fails.
    /// Note that a failed `reopen` invalidates the handle: all subsequent
    /// I/O on it will fail with `SQLITE_ABORT`.
    #[inline]
    pub fn reopen(&mut self, row: i64) -> Result<()> {
        let rc = unsafe { ffi::sqlite3_blob_reopen(self.blob, row) };
        if rc != ffi::SQLITE_OK {
            // SQLite aborts the handle when the reopen fails, so remember
            // that to report `SQLITE_ABORT` (rather than, say, an empty
            // read) from every subsequent I/O operation.
            self.poisoned.set(true);
            return self.conn.decode_result(rc);
        }
        self.pos = 0;
        Ok(())
    }

    /// Return the size in bytes of the BLOB.
    #[inline]
    #[must_use]
    pub fn size(&self) -> i32 {
        unsafe { ffi::sqlite3_blob_bytes(self.blob) }
    }

    /// Return the current size in bytes of the BLOB.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.size().try_into().unwrap()
    }

    /// Return true if the BLOB is empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Close a BLOB handle.
    ///
    /// Calling `close` explicitly is not required (the BLOB will be closed
    /// when the `Blob` is dropped), but it is available, so you can get any
    /// errors that occur.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite close call fails.
    #[inline]
    pub fn close(mut self) -> Result<()> {
        self.close_()
    }

    #[inline]
    fn close_(&mut self) -> Result<()> {
        let rc = unsafe { ffi::sqlite3_blob_close(self.blob) };
        self.blob = ptr::null_mut();
        self.conn.decode_result(rc)
    }

    /// Verify that the handle is still usable, returning the `SQLITE_ABORT`
    /// error reported by SQLite if it has been invalidated (by a failed
    /// [`Blob::reopen`], or by the row being updated or deleted).
    ///
    /// This only checks the cached invalidation state; use
    /// [`Blob::probe_valid`] on code paths that would otherwise return
    /// without performing any SQLite call.
    #[inline]
    fn check_valid(&self) -> Result<()> {
        if self.poisoned.get() {
            self.probe_valid()
        } else {
            Ok(())
        }
    }

    /// Probe the handle with a zero-length read: SQLite reports
    /// `SQLITE_ABORT` for an invalidated handle and `SQLITE_OK` for a live
    /// one, without modifying any data or the stream position. This is how
    /// operations that would otherwise not touch SQLite (reading at the end
    /// of the blob, empty buffers, out-of-range positional writes) still
    /// surface handle invalidation.
    #[cold]
    fn probe_valid(&self) -> Result<()> {
        let mut byte = 0u8;
        let rc = unsafe { ffi::sqlite3_blob_read(self.blob, (&raw mut byte).cast(), 0, 0) };
        self.decode_io_result(rc)
    }

    /// Decode the result of a blob I/O call, remembering if the handle
    /// became invalidated (`SQLITE_ABORT`) so that later calls keep
    /// reporting that error instead of results based on a stale size.
    #[inline]
    fn decode_io_result(&self, rc: c_int) -> Result<()> {
        let res = self.conn.decode_result(rc);
        if let Err(Error::SqliteFailure(ref e, _)) = res
            && e.code == ffi::ErrorCode::OperationAborted
        {
            self.poisoned.set(true);
        }
        res
    }
}

impl io::Read for Blob<'_> {
    /// Read data from a BLOB incrementally. Will return Ok(0) if the end of
    /// the blob has been reached.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite read call fails.
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check_valid().map_err(io::Error::other)?;
        let max_allowed_len = (self.size() - self.pos) as usize;
        let n = min(buf.len(), max_allowed_len) as i32;
        if n <= 0 {
            // Nothing to read, but still surface `SQLITE_ABORT` if the
            // handle was invalidated.
            self.probe_valid().map_err(io::Error::other)?;
            return Ok(0);
        }
        let rc = unsafe { ffi::sqlite3_blob_read(self.blob, buf.as_mut_ptr().cast(), n, self.pos) };
        self.decode_io_result(rc)
            .map(|()| {
                self.pos += n;
                n as usize
            })
            .map_err(io::Error::other)
    }
}

impl io::Write for Blob<'_> {
    /// Write data into a BLOB incrementally. Will return `Ok(0)` if the end of
    /// the blob has been reached; consider using `Write::write_all(buf)`
    /// if you want to get an error if the entirety of the buffer cannot be
    /// written.
    ///
    /// This function may only modify the contents of the BLOB; it is not
    /// possible to increase the size of a BLOB using this API.
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite write call fails.
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check_valid().map_err(io::Error::other)?;
        let max_allowed_len = (self.size() - self.pos) as usize;
        let n = min(buf.len(), max_allowed_len) as i32;
        if n <= 0 {
            // Nothing to write, but still surface `SQLITE_ABORT` if the
            // handle was invalidated.
            self.probe_valid().map_err(io::Error::other)?;
            return Ok(0);
        }
        let rc = unsafe { ffi::sqlite3_blob_write(self.blob, buf.as_ptr() as *mut _, n, self.pos) };
        self.decode_io_result(rc)
            .map(|()| {
                self.pos += n;
                n as usize
            })
            .map_err(io::Error::other)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Seek for Blob<'_> {
    /// Seek to an offset, in bytes, in BLOB.
    #[inline]
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            io::SeekFrom::Start(offset) => offset as i64,
            io::SeekFrom::Current(offset) => i64::from(self.pos) + offset,
            io::SeekFrom::End(offset) => i64::from(self.size()) + offset,
        };

        if pos < 0 {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to negative position",
            ))
        } else if pos > i64::from(self.size()) {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to position past end of blob",
            ))
        } else {
            self.pos = pos as i32;
            Ok(pos as u64)
        }
    }
}

#[expect(unused_must_use)]
impl Drop for Blob<'_> {
    #[inline]
    fn drop(&mut self) {
        self.close_();
    }
}

/// BLOB of length N that is filled with zeroes.
///
/// Zeroblobs are intended to serve as placeholders for BLOBs whose content is
/// later written using incremental BLOB I/O routines.
///
/// A negative value for the zeroblob results in a zero-length BLOB.
#[derive(Copy, Clone)]
pub struct ZeroBlob(pub u64);

impl ToSql for ZeroBlob {
    #[inline]
    fn to_sql(&self) -> Result<ToSqlOutput<'_>> {
        let Self(length) = *self;
        Ok(ToSqlOutput::ZeroBlob(length))
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use crate::{Connection, MAIN_DB, Result};
    use std::io::{BufRead as _, BufReader, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};

    fn db_with_test_blob() -> Result<(Connection, i64)> {
        let db = Connection::open_in_memory()?;
        let sql = "BEGIN;
                   CREATE TABLE test (content BLOB);
                   INSERT INTO test VALUES (ZEROBLOB(10));
                   END;";
        db.execute_batch(sql)?;
        let rowid = db.last_insert_rowid();
        Ok((db, rowid))
    }

    #[test]
    fn test_blob() -> Result<()> {
        let (db, rowid) = db_with_test_blob()?;

        let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
        assert!(!blob.is_empty());
        assert_eq!(10, blob.len());
        assert_eq!(4, blob.write(b"Clob").unwrap());
        assert_eq!(6, blob.write(b"567890xxxxxx").unwrap()); // cannot write past 10
        assert_eq!(0, blob.write(b"5678").unwrap()); // still cannot write past 10
        blob.flush().unwrap();

        blob.reopen(rowid)?;
        blob.close()?;

        blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, true)?;
        let mut bytes = [0u8; 5];
        assert_eq!(5, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"Clob5");
        assert_eq!(5, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"67890");
        assert_eq!(0, blob.read(&mut bytes[..]).unwrap());

        blob.seek(SeekFrom::Start(2)).unwrap();
        assert_eq!(5, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"ob567");

        // only first 4 bytes of `bytes` should be read into
        blob.seek(SeekFrom::Current(-1)).unwrap();
        assert_eq!(4, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"78907");

        blob.seek(SeekFrom::End(-6)).unwrap();
        assert_eq!(5, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"56789");

        blob.reopen(rowid)?;
        assert_eq!(5, blob.read(&mut bytes[..]).unwrap());
        assert_eq!(&bytes, b"Clob5");

        // should not be able to seek negative or past end
        blob.seek(SeekFrom::Current(-20)).unwrap_err();
        blob.seek(SeekFrom::End(0)).unwrap();
        blob.seek(SeekFrom::Current(1)).unwrap_err();

        // write_all should detect when we return Ok(0) because there is no space left,
        // and return a write error
        blob.reopen(rowid)?;
        blob.write_all(b"0123456789x").unwrap_err();
        Ok(())
    }

    #[test]
    fn test_blob_in_bufreader() -> Result<()> {
        let (db, rowid) = db_with_test_blob()?;

        let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
        assert_eq!(8, blob.write(b"one\ntwo\n").unwrap());

        blob.reopen(rowid)?;
        let mut reader = BufReader::new(blob);

        let mut line = String::new();
        assert_eq!(4, reader.read_line(&mut line).unwrap());
        assert_eq!("one\n", line);

        line.clear();
        assert_eq!(4, reader.read_line(&mut line).unwrap());
        assert_eq!("two\n", line);

        line.clear();
        assert_eq!(2, reader.read_line(&mut line).unwrap());
        assert_eq!("\0\0", line);
        Ok(())
    }

    #[test]
    fn test_blob_in_bufwriter() -> Result<()> {
        let (db, rowid) = db_with_test_blob()?;

        {
            let blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
            let mut writer = BufWriter::new(blob);

            // trying to write too much and then flush should fail
            assert_eq!(8, writer.write(b"01234567").unwrap());
            assert_eq!(8, writer.write(b"01234567").unwrap());
            writer.flush().unwrap_err();
        }

        {
            // ... but it should've written the first 10 bytes
            let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
            let mut bytes = [0u8; 10];
            assert_eq!(10, blob.read(&mut bytes[..]).unwrap());
            assert_eq!(b"0123456701", &bytes);
        }

        {
            let blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
            let mut writer = BufWriter::new(blob);

            // trying to write_all too much should fail
            writer.write_all(b"aaaaaaaaaabbbbb").unwrap();
            writer.flush().unwrap_err();
        }

        {
            // ... but it should've written the first 10 bytes
            let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
            let mut bytes = [0u8; 10];
            assert_eq!(10, blob.read(&mut bytes[..]).unwrap());
            assert_eq!(b"aaaaaaaaaa", &bytes);
            Ok(())
        }
    }

    #[test]
    fn zero_blob() -> Result<()> {
        use crate::types::ToSql as _;
        let zb = super::ZeroBlob(1);
        assert!(zb.to_sql().is_ok());
        Ok(())
    }

    fn expect_abort<T: std::fmt::Debug>(res: Result<T>) {
        match res {
            Err(crate::Error::SqliteFailure(ref e, _))
                if e.code == crate::ffi::ErrorCode::OperationAborted => {}
            other => panic!("expected SQLITE_ABORT error, got {other:?}"),
        }
    }

    fn expect_io_abort<T: std::fmt::Debug>(res: std::io::Result<T>) {
        let err = res.expect_err("expected I/O error");
        let inner = err
            .downcast::<crate::Error>()
            .expect("io::Error should wrap a rusqlite::Error");
        assert_eq!(
            inner.sqlite_error_code(),
            Some(crate::ffi::ErrorCode::OperationAborted),
            "expected SQLITE_ABORT, got {inner:?}"
        );
    }

    #[test]
    fn test_blob_invalidated_by_failed_reopen() -> Result<()> {
        let (db, rowid) = db_with_test_blob()?;
        let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
        blob.write_all(b"0123456789").unwrap();

        // Reopening a row that does not exist returns the original SQLite
        // error (SQLITE_ERROR), and invalidates the handle.
        let err = blob.reopen(rowid + 100).unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(crate::ffi::ErrorCode::Unknown)
        );

        // Every subsequent I/O operation reports SQLITE_ABORT, on the first
        // and on later calls alike.
        let mut buf = [0xAAu8; 4];
        expect_io_abort(blob.read(&mut buf));
        assert_eq!(&buf, &[0xAAu8; 4], "buffer must be untouched");
        expect_io_abort(blob.read(&mut buf));
        expect_io_abort(blob.write(b"xy"));
        expect_abort(blob.read_at(&mut buf, 0));
        expect_abort(blob.read_at_exact(&mut buf, 0));
        expect_abort(blob.write_at(b"xy", 0));
        expect_abort(blob.write_all_at(b"xy", 0));
        let mut uninit = [std::mem::MaybeUninit::uninit(); 4];
        expect_abort(blob.raw_read_at(&mut uninit, 0));
        expect_abort(blob.raw_read_at_exact(&mut uninit, 0));

        // The handle cannot be resurrected by reopening the original row.
        blob.reopen(rowid).unwrap_err();

        // Closing works, and a fresh handle on the same connection is fine.
        blob.close()?;
        let mut blob = db.blob_open(MAIN_DB, c"test", c"content", rowid, false)?;
        let mut buf = [0u8; 10];
        blob.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"0123456789");
        Ok(())
    }

    #[test]
    fn test_blob_invalidated_by_reopen_to_non_blob() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE t (content);
             INSERT INTO t VALUES (ZEROBLOB(8));
             INSERT INTO t VALUES (42);",
        )?;
        let mut blob = db.blob_open(MAIN_DB, c"t", c"content", 1, false)?;
        // The target column of the new row is not a BLOB/TEXT: the reopen
        // fails with the original error and invalidates the handle.
        let err = blob.reopen(2).unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(crate::ffi::ErrorCode::Unknown)
        );
        expect_io_abort(blob.read(&mut [0u8; 4]));
        expect_abort(blob.read_at(&mut [0u8; 4], 0));
        expect_io_abort(blob.write(b"xy"));
        Ok(())
    }

    #[test]
    fn test_blob_invalidated_by_row_update_or_delete() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t (content BLOB, other TEXT);")?;
        db.execute("INSERT INTO t VALUES (ZEROBLOB(10), 'a')", [])?;
        let rowid = db.last_insert_rowid();

        // Updating even an unrelated column of the row invalidates the handle.
        let mut blob = db.blob_open(MAIN_DB, c"t", c"content", rowid, false)?;
        blob.write_all(b"0123456789").unwrap();
        db.execute("UPDATE t SET other = 'b' WHERE rowid = ?1", [rowid])?;
        let mut buf = [0u8; 10];
        expect_io_abort(blob.read(&mut buf));
        // Subsequent calls keep failing the same way.
        expect_io_abort(blob.read(&mut buf));
        expect_abort(blob.read_at(&mut buf, 0));
        expect_abort(blob.write_at(b"zz", 0));
        blob.close()?;

        // Deleting the row invalidates the handle too.
        let mut blob = db.blob_open(MAIN_DB, c"t", c"content", rowid, false)?;
        db.execute("DELETE FROM t WHERE rowid = ?1", [rowid])?;
        expect_io_abort(blob.write(b"zz"));
        expect_io_abort(blob.read(&mut buf));
        expect_abort(blob.read_at_exact(&mut buf, 0));
        blob.close()?;
        Ok(())
    }

    #[test]
    fn test_blob_invalidation_short_circuit_paths() -> Result<()> {
        // Each of these operations would, on a valid handle, return without
        // calling into SQLite; on an invalidated handle they must all report
        // SQLITE_ABORT instead of an empty result or a BlobSizeError.
        for case in 0..8 {
            let db = Connection::open_in_memory()?;
            db.execute_batch(
                "CREATE TABLE t (content BLOB);
                 INSERT INTO t VALUES (ZEROBLOB(10));",
            )?;
            let rowid = db.last_insert_rowid();
            let mut blob = db.blob_open(MAIN_DB, c"t", c"content", rowid, false)?;
            if case == 0 || case == 5 {
                blob.seek(SeekFrom::End(0)).unwrap();
            }
            db.execute("DELETE FROM t WHERE rowid = ?1", [rowid])?;
            let mut buf = [0u8; 4];
            match case {
                0 => expect_io_abort(blob.read(&mut buf)), // at end of stream
                1 => expect_io_abort(blob.read(&mut [])),  // empty buffer
                2 => expect_abort(blob.read_at(&mut buf, 10)), // offset past end
                3 => expect_abort(blob.read_at(&mut buf, usize::MAX)), // huge offset
                4 => expect_abort(blob.read_at_exact(&mut buf, 12)), // exact, past end
                5 => expect_io_abort(blob.write(b"ab")),   // write at end
                6 => expect_abort(blob.write_at(b"abcdef", 8)), // out-of-range write
                7 => expect_abort(blob.write_at(b"ab", usize::MAX)), // huge offset write
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[test]
    fn test_blob_invalidation_inside_transaction() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t (content BLOB, other TEXT);")?;
        db.execute("INSERT INTO t VALUES (ZEROBLOB(10), 'a')", [])?;
        let rowid = db.last_insert_rowid();

        let tx = db.transaction()?;
        tx.execute("INSERT INTO t VALUES (ZEROBLOB(4), 'b')", [])?;
        let mut blob = tx.blob_open(MAIN_DB, c"t", c"content", rowid, false)?;
        tx.execute("UPDATE t SET other = 'c' WHERE rowid = ?1", [rowid])?;
        let mut buf = [0u8; 4];
        expect_io_abort(blob.read(&mut buf));
        blob.close()?;
        // The I/O error must not have committed or rolled back the caller's
        // transaction; the caller decides what happens to its changes.
        tx.rollback()?;

        let count: i64 = db.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?;
        assert_eq!(count, 1);
        let other: String =
            db.query_row("SELECT other FROM t WHERE rowid = ?1", [rowid], |r| {
                r.get(0)
            })?;
        assert_eq!(other, "a");
        Ok(())
    }

    #[test]
    fn test_zero_length_blob_unaffected() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(
            "CREATE TABLE t (content BLOB);
             INSERT INTO t VALUES (ZEROBLOB(0));",
        )?;
        let rowid = db.last_insert_rowid();
        let mut blob = db.blob_open(MAIN_DB, c"t", c"content", rowid, false)?;
        assert!(blob.is_empty());
        // A genuine zero-length BLOB still yields empty results, not
        // invalidation errors.
        let mut buf = [0u8; 4];
        assert_eq!(blob.read(&mut buf).unwrap(), 0);
        assert_eq!(blob.write(b"ab").unwrap(), 0);
        assert_eq!(blob.read_at(&mut buf, 0)?, 0);
        blob.read_at_exact(&mut [], 0)?;
        blob.write_at(&[], 0)?;
        let mut uninit = [std::mem::MaybeUninit::<u8>::uninit(); 2];
        assert_eq!(blob.raw_read_at(&mut uninit, 0)?.len(), 0);
        blob.raw_read_at_exact(&mut [], 0)?;
        Ok(())
    }
}
