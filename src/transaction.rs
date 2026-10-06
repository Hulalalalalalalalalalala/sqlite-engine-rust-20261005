use crate::pragma::Sql;
use crate::{Connection, Result};
use std::ops::Deref;

/// Options for transaction behavior. See [BEGIN
/// TRANSACTION](http://www.sqlite.org/lang_transaction.html) for details.
#[derive(Copy, Clone)]
#[non_exhaustive]
pub enum TransactionBehavior {
    /// DEFERRED means that the transaction does not actually start until the
    /// database is first accessed.
    Deferred,
    /// IMMEDIATE cause the database connection to start a new write
    /// immediately, without waiting for a writes statement.
    Immediate,
    /// EXCLUSIVE prevents other database connections from reading the database
    /// while the transaction is underway.
    Exclusive,
}

/// Options for how a Transaction or Savepoint should behave when it is dropped.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropBehavior {
    /// Roll back the changes. This is the default.
    Rollback,

    /// Commit the changes.
    Commit,

    /// Do not commit or roll back changes - this will leave the transaction or
    /// savepoint open, so should be used with care.
    Ignore,

    /// Panic. Used to enforce intentional behavior during development.
    Panic,
}

/// Represents a transaction on a database connection.
///
/// ## Note
///
/// Transactions will roll back by default. Use `commit` method to explicitly
/// commit the transaction, or use `set_drop_behavior` to change what happens
/// when the transaction is dropped.
///
/// ## Example
///
/// ```rust,no_run
/// # use rusqlite::{Connection, Result};
/// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
/// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
/// fn perform_queries(conn: &mut Connection) -> Result<()> {
///     let tx = conn.transaction()?;
///
///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
///
///     tx.commit()
/// }
/// ```
#[derive(Debug)]
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    drop_behavior: DropBehavior,
    /// Set once an explicit finish has been attempted, so that the `Drop`
    /// impl does not try to commit or roll back a second time.
    finished: bool,
}

/// Represents a savepoint on a database connection.
///
/// ## Note
///
/// Savepoints will roll back by default. Use `commit` method to explicitly
/// commit the savepoint, or use `set_drop_behavior` to change what happens
/// when the savepoint is dropped.
///
/// ## Example
///
/// ```rust,no_run
/// # use rusqlite::{Connection, Result};
/// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
/// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
/// fn perform_queries(conn: &mut Connection) -> Result<()> {
///     let sp = conn.savepoint()?;
///
///     do_queries_part_1(&sp)?; // sp causes rollback if this fails
///     do_queries_part_2(&sp)?; // sp causes rollback if this fails
///
///     sp.commit()
/// }
/// ```
#[derive(Debug)]
pub struct Savepoint<'conn> {
    conn: &'conn Connection,
    name: String,
    drop_behavior: DropBehavior,
    /// Set once the savepoint has been released or an explicit finish has
    /// been attempted, so that the `Drop` impl does not act a second time.
    committed: bool,
    /// Whether this savepoint began the transaction (i.e. it was created
    /// while the connection was in autocommit mode). Releasing such a
    /// savepoint commits the whole transaction, so cleaning up after a
    /// failed release requires rolling the transaction back; a nested
    /// savepoint may only roll back to itself and release itself.
    starts_transaction: bool,
}

impl Transaction<'_> {
    /// Begin a new transaction. Cannot be nested; see `savepoint` for nested
    /// transactions.
    ///
    /// Even though we don't mutate the connection, we take a `&mut Connection`
    /// to prevent nested transactions on the same connection. For cases
    /// where this is unacceptable, [`Transaction::new_unchecked`] is available.
    #[inline]
    pub fn new(conn: &mut Connection, behavior: TransactionBehavior) -> Result<Transaction<'_>> {
        Self::new_unchecked(conn, behavior)
    }

    /// Begin a new transaction, failing if a transaction is open.
    ///
    /// If a transaction is already open, this will return an error. Where
    /// possible, [`Transaction::new`] should be preferred, as it provides a
    /// compile-time guarantee that transactions are not nested.
    #[inline]
    pub fn new_unchecked(
        conn: &Connection,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        let query = match behavior {
            TransactionBehavior::Deferred => "BEGIN DEFERRED",
            TransactionBehavior::Immediate => "BEGIN IMMEDIATE",
            TransactionBehavior::Exclusive => "BEGIN EXCLUSIVE",
        };
        conn.execute_batch(query).map(move |()| Transaction {
            conn,
            drop_behavior: DropBehavior::Rollback,
            finished: false,
        })
    }

    /// Starts a new [savepoint](http://www.sqlite.org/lang_savepoint.html), allowing nested
    /// transactions.
    ///
    /// ## Note
    ///
    /// Just like outer level transactions, savepoint transactions rollback by
    /// default.
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn perform_queries_part_1_succeeds(_conn: &Connection) -> bool { true }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let mut tx = conn.transaction()?;
    ///
    ///     {
    ///         let sp = tx.savepoint()?;
    ///         if perform_queries_part_1_succeeds(&sp) {
    ///             sp.commit()?;
    ///         }
    ///         // otherwise, sp will rollback
    ///     }
    ///
    ///     tx.commit()
    /// }
    /// ```
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new_(self.conn)
    }

    /// Create a new savepoint with a custom savepoint name. See `savepoint()`.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(self.conn, name)
    }

    /// Get the current setting for what happens to the transaction when it is
    /// dropped.
    #[inline]
    #[must_use]
    pub fn drop_behavior(&self) -> DropBehavior {
        self.drop_behavior
    }

    /// Configure the transaction to perform the specified action when it is
    /// dropped.
    #[inline]
    pub fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    /// A convenience method which consumes and commits a transaction.
    ///
    /// ## Note
    ///
    /// The commit is attempted only once, regardless of the configured
    /// [`DropBehavior`]. If it fails, a single best-effort rollback is
    /// attempted to return the connection to autocommit mode, but the
    /// original commit error is returned (even if the cleanup also fails),
    /// and the transaction is left alone when it is dropped.
    #[inline]
    pub fn commit(mut self) -> Result<()> {
        // An explicit commit decides how the transaction ends; make sure the
        // `Drop` impl does not commit, roll back, or panic afterwards.
        self.finished = true;
        self.commit_and_cleanup()
    }

    #[inline]
    fn commit_(&mut self) -> Result<()> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Attempt the commit once. If it fails, the changes were not persisted,
    /// so try to roll back to leave the connection usable, but always report
    /// the original commit error, even if the cleanup fails. SQLite may
    /// already have ended the transaction itself (e.g. when an authorizer
    /// denied the commit), in which case there is nothing to clean up.
    #[inline]
    fn commit_and_cleanup(&mut self) -> Result<()> {
        match self.commit_() {
            Ok(()) => Ok(()),
            Err(err) => {
                if !self.conn.is_autocommit() {
                    let _ = self.rollback_();
                }
                Err(err)
            }
        }
    }

    /// A convenience method which consumes and rolls back a transaction.
    ///
    /// ## Note
    ///
    /// The rollback is attempted only once, regardless of the configured
    /// [`DropBehavior`]. If it fails, the error is returned and the still
    /// active transaction is left for the caller to resolve; the `Drop` impl
    /// does not roll back again, commit, or panic.
    #[inline]
    pub fn rollback(mut self) -> Result<()> {
        // An explicit rollback decides how the transaction ends; make sure
        // the `Drop` impl does not roll back again, commit, or panic
        // afterwards.
        self.finished = true;
        self.rollback_()
    }

    #[inline]
    fn rollback_(&mut self) -> Result<()> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Consumes the transaction, committing or rolling back according to the
    /// current setting (see `drop_behavior`).
    ///
    /// Functionally equivalent to the `Drop` implementation, but allows
    /// callers to see any errors that occur.
    ///
    /// ## Note
    ///
    /// With [`DropBehavior::Commit`], the commit is attempted only once. If
    /// it fails, a single best-effort rollback is attempted to return the
    /// connection to autocommit mode, but the original commit error is
    /// returned (even if the cleanup also fails), and the transaction is
    /// left alone when it is dropped.
    #[inline]
    pub fn finish(mut self) -> Result<()> {
        self.finish_()
    }

    #[inline]
    fn finish_(&mut self) -> Result<()> {
        if self.finished || self.conn.is_autocommit() {
            return Ok(());
        }
        // An explicit finish gets a single attempt; make sure the `Drop`
        // impl does not commit or roll back again afterwards.
        self.finished = true;
        match self.drop_behavior() {
            DropBehavior::Commit => self.commit_and_cleanup(),
            DropBehavior::Rollback => self.rollback_(),
            DropBehavior::Ignore => Ok(()),
            DropBehavior::Panic => panic!("Transaction dropped unexpectedly."),
        }
    }
}

impl Deref for Transaction<'_> {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Connection {
        self.conn
    }
}

#[expect(unused_must_use)]
impl Drop for Transaction<'_> {
    #[inline]
    fn drop(&mut self) {
        self.finish_();
    }
}

impl Savepoint<'_> {
    #[inline]
    fn with_name_<T: Into<String>>(conn: &Connection, name: T) -> Result<Savepoint<'_>> {
        let name = name.into();
        let sql = cmd("SAVEPOINT", false, name.as_str())?;
        // A savepoint created outside of any transaction begins one, so its
        // release commits the whole transaction.
        let starts_transaction = conn.is_autocommit();
        conn.execute_batch(sql.as_str()).map(|()| Savepoint {
            conn,
            name,
            drop_behavior: DropBehavior::Rollback,
            committed: false,
            starts_transaction,
        })
    }

    #[inline]
    fn new_(conn: &Connection) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(conn, "_rusqlite_sp")
    }

    /// Begin a new savepoint. Can be nested.
    #[inline]
    pub fn new(conn: &mut Connection) -> Result<Savepoint<'_>> {
        Savepoint::new_(conn)
    }

    /// Begin a new savepoint with a user-provided savepoint name.
    #[inline]
    pub fn with_name<T: Into<String>>(conn: &mut Connection, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(conn, name)
    }

    /// Begin a nested savepoint.
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new_(self.conn)
    }

    /// Begin a nested savepoint with a user-provided savepoint name.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name_(self.conn, name)
    }

    /// Get the current setting for what happens to the savepoint when it is
    /// dropped.
    #[inline]
    #[must_use]
    pub fn drop_behavior(&self) -> DropBehavior {
        self.drop_behavior
    }

    /// Configure the savepoint to perform the specified action when it is
    /// dropped.
    #[inline]
    pub fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    /// A convenience method which consumes and commits a savepoint.
    ///
    /// ## Note
    ///
    /// The release is attempted only once, regardless of the configured
    /// [`DropBehavior`]. If it fails, a single best-effort cleanup is
    /// attempted (rolling back the transaction if this savepoint began it,
    /// or rolling back to and releasing this savepoint if it is nested), but
    /// the original release error is returned (even if the cleanup also
    /// fails), and the savepoint is left alone when it is dropped.
    #[inline]
    pub fn commit(mut self) -> Result<()> {
        // An explicit commit decides how the savepoint ends; make sure the
        // `Drop` impl does not release, roll back, or panic afterwards.
        self.committed = true;
        self.commit_and_cleanup()
    }

    #[inline]
    fn commit_(&mut self) -> Result<()> {
        let sql = cmd("RELEASE", false, self.name.as_str())?;
        self.conn.execute_batch(sql.as_str())?;
        self.committed = true;
        Ok(())
    }

    /// Attempt the release once. If it fails, the savepoint is still active,
    /// so clean up according to the savepoint's scope, but always report the
    /// original release error, even if the cleanup fails.
    #[inline]
    fn commit_and_cleanup(&mut self) -> Result<()> {
        match self.commit_() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.cleanup_after_failed_commit();
                Err(err)
            }
        }
    }

    /// A convenience method which rolls back a savepoint.
    ///
    /// ## Note
    ///
    /// Unlike `Transaction`s, savepoints remain active after they have been
    /// rolled back, and can be rolled back again or committed.
    #[inline]
    pub fn rollback(&mut self) -> Result<()> {
        let sql = cmd("ROLLBACK", true, self.name.as_str())?;
        self.conn.execute_batch(sql.as_str())
    }

    /// Consumes the savepoint, committing or rolling back according to the
    /// current setting (see `drop_behavior`).
    ///
    /// Functionally equivalent to the `Drop` implementation, but allows
    /// callers to see any errors that occur.
    ///
    /// ## Note
    ///
    /// With [`DropBehavior::Commit`], the release is attempted only once. If
    /// it fails, a single best-effort cleanup is attempted (rolling back the
    /// transaction if this savepoint began it, or rolling back to and
    /// releasing this savepoint if it is nested), but the original release
    /// error is returned (even if the cleanup also fails), and the savepoint
    /// is left alone when it is dropped.
    #[inline]
    pub fn finish(mut self) -> Result<()> {
        self.finish_()
    }

    #[inline]
    fn finish_(&mut self) -> Result<()> {
        if self.committed {
            return Ok(());
        }
        // An explicit finish gets a single attempt; make sure the `Drop`
        // impl does not release or roll back again afterwards.
        self.committed = true;
        match self.drop_behavior() {
            DropBehavior::Commit => self.commit_and_cleanup(),
            DropBehavior::Rollback => self.rollback().and_then(|()| self.commit_()),
            DropBehavior::Ignore => Ok(()),
            DropBehavior::Panic => panic!("Savepoint dropped unexpectedly."),
        }
    }

    /// Best-effort cleanup after a failed release, limited to the changes
    /// this savepoint is responsible for. The outcome is ignored: callers
    /// report the release error, and a failed cleanup leaves the still
    /// active transaction or savepoint for the caller to resolve.
    #[inline]
    fn cleanup_after_failed_commit(&mut self) {
        if self.conn.is_autocommit() {
            // SQLite already ended the transaction on its own; there is
            // nothing left to clean up.
            return;
        }
        if self.starts_transaction {
            // Releasing this savepoint would have committed the whole
            // transaction, so roll the transaction back to undo the changes
            // and return the connection to autocommit mode.
            let _ = self.conn.execute_batch("ROLLBACK");
        } else {
            // Nested savepoint: undo only the changes made since it was
            // established, then release it, preserving the outer
            // transaction. If the rollback is not possible, do not release
            // either, as that would keep the changes.
            let _ = self.rollback().and_then(|()| self.commit_());
        }
    }
}

fn cmd(cmd: &'static str, to: bool, name: &str) -> Result<Sql> {
    let mut sql = Sql::new();
    sql.push_keyword(cmd)?;
    sql.push_space();
    if to {
        sql.push_keyword("TO")?;
        sql.push_space();
    }
    sql.push_identifier(name);
    Ok(sql)
}

impl Deref for Savepoint<'_> {
    type Target = Connection;

    #[inline]
    fn deref(&self) -> &Connection {
        self.conn
    }
}

#[expect(unused_must_use)]
impl Drop for Savepoint<'_> {
    #[inline]
    fn drop(&mut self) {
        self.finish_();
    }
}

/// Transaction state of a database
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransactionState {
    /// Equivalent to `SQLITE_TXN_NONE`
    None,
    /// Equivalent to `SQLITE_TXN_READ`
    Read,
    /// Equivalent to `SQLITE_TXN_WRITE`
    Write,
}

impl Connection {
    /// Begin a new transaction with the default behavior (DEFERRED).
    ///
    /// The transaction defaults to rolling back when it is dropped. If you
    /// want the transaction to commit, you must call
    /// [`commit`](Transaction::commit) or
    /// [`set_drop_behavior(DropBehavior::Commit)`](Transaction::set_drop_behavior).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let tx = conn.transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        Transaction::new(self, self.transaction_behavior)
    }

    /// Begin a new transaction with a specified behavior.
    ///
    /// See [`transaction`](Connection::transaction).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        Transaction::new(self, behavior)
    }

    /// Begin a new transaction with the default behavior (DEFERRED).
    ///
    /// Attempt to open a nested transaction will result in a SQLite error.
    /// `Connection::transaction` prevents this at compile time by taking `&mut
    /// self`, but `Connection::unchecked_transaction()` may be used to defer
    /// the checking until runtime.
    ///
    /// See [`Connection::transaction`] and [`Transaction::new_unchecked`]
    /// (which can be used if the default transaction behavior is undesirable).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # use std::rc::Rc;
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: Rc<Connection>) -> Result<()> {
    ///     let tx = conn.unchecked_transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails. The specific
    /// error returned if transactions are nested is currently unspecified.
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(self, self.transaction_behavior)
    }

    /// Begin a new savepoint with the default behavior (DEFERRED).
    ///
    /// The savepoint defaults to rolling back when it is dropped. If you want
    /// the savepoint to commit, you must call [`commit`](Savepoint::commit) or
    /// [`set_drop_behavior(DropBehavior::Commit)`](Savepoint::set_drop_behavior).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     let sp = conn.savepoint()?;
    ///
    ///     do_queries_part_1(&sp)?; // sp causes rollback if this fails
    ///     do_queries_part_2(&sp)?; // sp causes rollback if this fails
    ///
    ///     sp.commit()
    /// }
    /// ```
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn savepoint(&mut self) -> Result<Savepoint<'_>> {
        Savepoint::new(self)
    }

    /// Begin a new savepoint with a specified name.
    ///
    /// See [`savepoint`](Connection::savepoint).
    ///
    /// # Failure
    ///
    /// Will return `Err` if the underlying SQLite call fails.
    #[inline]
    pub fn savepoint_with_name<T: Into<String>>(&mut self, name: T) -> Result<Savepoint<'_>> {
        Savepoint::with_name(self, name)
    }

    /// Determine the transaction state of a database
    pub fn transaction_state<N: crate::Name>(
        &self,
        db_name: Option<N>,
    ) -> Result<TransactionState> {
        self.db.borrow().txn_state(db_name)
    }

    /// Set the default transaction behavior for the connection.
    ///
    /// ## Note
    ///
    /// This will only apply to transactions initiated by [`transaction`](Connection::transaction)
    /// or [`unchecked_transaction`](Connection::unchecked_transaction).
    ///
    /// ## Example
    ///
    /// ```rust,no_run
    /// # use rusqlite::{Connection, Result, TransactionBehavior};
    /// # fn do_queries_part_1(_conn: &Connection) -> Result<()> { Ok(()) }
    /// # fn do_queries_part_2(_conn: &Connection) -> Result<()> { Ok(()) }
    /// fn perform_queries(conn: &mut Connection) -> Result<()> {
    ///     conn.set_transaction_behavior(TransactionBehavior::Immediate);
    ///
    ///     let tx = conn.transaction()?;
    ///
    ///     do_queries_part_1(&tx)?; // tx causes rollback if this fails
    ///     do_queries_part_2(&tx)?; // tx causes rollback if this fails
    ///
    ///     tx.commit()
    /// }
    /// ```
    pub fn set_transaction_behavior(&mut self, behavior: TransactionBehavior) {
        self.transaction_behavior = behavior;
    }
}

#[cfg(all(test, not(miri)))]
mod test {
    use std::assert_matches;
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    use super::DropBehavior;
    use crate::{Connection, Error, Result};

    fn checked_memory_handle() -> Result<Connection> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE foo (x INTEGER)")?;
        Ok(db)
    }

    #[test]
    fn test_drop() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // default: rollback
        }
        {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(2)")?;
            tx.set_drop_behavior(DropBehavior::Commit);
        }
        {
            let tx = db.transaction()?;
            assert_eq!(2, tx.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        }
        Ok(())
    }
    fn assert_nested_tx_error(e: Error) {
        assert_matches!(
            e,
            Error::SqliteFailure(
                crate::ffi::Error {
                    code: crate::ErrorCode::Unknown,
                    extended_code: crate::ffi::SQLITE_ERROR,
                },
                Some(msg),
            ) if msg.contains("transaction")
        );
    }

    #[test]
    fn test_unchecked_nesting() -> Result<()> {
        let db = checked_memory_handle()?;

        {
            let tx = db.unchecked_transaction()?;
            let e = tx.unchecked_transaction().unwrap_err();
            assert_nested_tx_error(e);
            // default: rollback
        }
        {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // Ensure this doesn't interfere with ongoing transaction
            let e = tx.unchecked_transaction().unwrap_err();
            assert_nested_tx_error(e);

            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            tx.commit()?;
        }

        assert_eq!(2, db.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn test_explicit_rollback_commit() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let mut tx = db.transaction()?;
            {
                let mut sp = tx.savepoint()?;
                sp.execute_batch("INSERT INTO foo VALUES(1)")?;
                sp.rollback()?;
                sp.execute_batch("INSERT INTO foo VALUES(2)")?;
                sp.commit()?;
            }
            tx.commit()?;
        }
        {
            let tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(4)")?;
            tx.commit()?;
        }
        {
            let tx = db.transaction()?;
            assert_eq!(6, tx.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        }
        Ok(())
    }

    #[test]
    fn test_savepoint() -> Result<()> {
        let mut db = checked_memory_handle()?;
        {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            assert_current_sum(1, &tx)?;
            tx.set_drop_behavior(DropBehavior::Commit);
            {
                let mut sp1 = tx.savepoint()?;
                sp1.execute_batch("INSERT INTO foo VALUES(2)")?;
                assert_current_sum(3, &sp1)?;
                // will roll back sp1
                {
                    let mut sp2 = sp1.savepoint()?;
                    sp2.execute_batch("INSERT INTO foo VALUES(4)")?;
                    assert_current_sum(7, &sp2)?;
                    // will roll back sp2
                    {
                        let sp3 = sp2.savepoint()?;
                        sp3.execute_batch("INSERT INTO foo VALUES(8)")?;
                        assert_current_sum(15, &sp3)?;
                        sp3.commit()?;
                        // committed sp3, but will be erased by sp2 rollback
                    }
                    assert_current_sum(15, &sp2)?;
                }
                assert_current_sum(3, &sp1)?;
            }
            assert_current_sum(1, &tx)?;
        }
        assert_current_sum(1, &db)?;
        Ok(())
    }

    #[test]
    fn test_ignore_drop_behavior() -> Result<()> {
        let mut db = checked_memory_handle()?;

        let mut tx = db.transaction()?;
        {
            let mut sp1 = tx.savepoint()?;
            insert(1, &sp1)?;
            sp1.rollback()?;
            insert(2, &sp1)?;
            {
                let mut sp2 = sp1.savepoint()?;
                sp2.set_drop_behavior(DropBehavior::Ignore);
                insert(4, &sp2)?;
            }
            assert_current_sum(6, &sp1)?;
            sp1.commit()?;
        }
        assert_current_sum(6, &tx)?;
        Ok(())
    }

    #[test]
    fn test_savepoint_drop_behavior_releases() -> Result<()> {
        let mut db = checked_memory_handle()?;

        {
            let mut sp = db.savepoint()?;
            sp.set_drop_behavior(DropBehavior::Commit);
        }
        assert!(db.is_autocommit());
        {
            let mut sp = db.savepoint()?;
            sp.set_drop_behavior(DropBehavior::Rollback);
        }
        assert!(db.is_autocommit());

        Ok(())
    }

    #[test]
    fn test_savepoint_release_error() -> Result<()> {
        let mut db = checked_memory_handle()?;

        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch("CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL); CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);")?;
        {
            let mut sp = db.savepoint()?;
            sp.execute("INSERT INTO f VALUES (0)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
        }
        assert!(db.is_autocommit());

        Ok(())
    }

    #[test]
    fn test_savepoint_names() -> Result<()> {
        let mut db = checked_memory_handle()?;

        {
            let mut sp1 = db.savepoint_with_name("my_sp")?;
            insert(1, &sp1)?;
            assert_current_sum(1, &sp1)?;
            {
                let mut sp2 = sp1.savepoint_with_name("my_sp")?;
                sp2.set_drop_behavior(DropBehavior::Commit);
                insert(2, &sp2)?;
                assert_current_sum(3, &sp2)?;
                sp2.rollback()?;
                assert_current_sum(1, &sp2)?;
                insert(4, &sp2)?;
            }
            assert_current_sum(5, &sp1)?;
            sp1.rollback()?;
            {
                let mut sp2 = sp1.savepoint_with_name("my_sp")?;
                sp2.set_drop_behavior(DropBehavior::Ignore);
                insert(8, &sp2)?;
            }
            assert_current_sum(8, &sp1)?;
            sp1.commit()?;
        }
        assert_current_sum(8, &db)?;
        Ok(())
    }

    #[test]
    fn test_rc() -> Result<()> {
        use std::rc::Rc;
        let mut conn = Connection::open_in_memory()?;
        let rc_txn = Rc::new(conn.transaction()?);

        // This will compile only if Transaction is Debug
        Rc::try_unwrap(rc_txn).unwrap();
        Ok(())
    }

    fn insert(x: i32, conn: &Connection) -> Result<usize> {
        conn.execute("INSERT INTO foo VALUES(?1)", [x])
    }

    fn assert_current_sum(x: i32, conn: &Connection) -> Result<()> {
        assert_eq!(x, conn.one_column::<i32, _>("SELECT SUM(x) FROM foo", [])?);
        Ok(())
    }

    #[test]
    fn txn_state() -> Result<()> {
        use super::TransactionState;
        use crate::{DEFAULT_NAME, MAIN_DB};
        let db = Connection::open_in_memory()?;
        assert_eq!(TransactionState::None, db.transaction_state(Some(MAIN_DB))?);
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        db.execute_batch("BEGIN")?;
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        let _: i32 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
        db.pragma_update(None, "user_version", 1)?;
        assert_eq!(TransactionState::Write, db.transaction_state(DEFAULT_NAME)?);
        db.execute_batch("ROLLBACK")?;
        Ok(())
    }

    #[test]
    fn auto_commit() -> Result<()> {
        use super::TransactionState;
        use crate::DEFAULT_NAME;
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(i UNIQUE);")?;
        assert!(db.is_autocommit());
        let mut stmt = db.prepare("SELECT name FROM sqlite_master")?;
        assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        {
            let mut rows = stmt.query([])?;
            assert!(rows.next()?.is_some()); // start reading
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            db.execute("INSERT INTO t VALUES (1)", [])?; // auto-commit
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            assert!(rows.next()?.is_some()); // still reading
            assert_eq!(TransactionState::Read, db.transaction_state(DEFAULT_NAME)?);
            assert!(rows.next()?.is_none()); // end
            assert_eq!(TransactionState::None, db.transaction_state(DEFAULT_NAME)?);
        }
        Ok(())
    }

    fn assert_fk_error(e: &Error) {
        match e {
            Error::SqliteFailure(ffi_err, Some(msg)) => {
                assert_eq!(crate::ErrorCode::ConstraintViolation, ffi_err.code);
                assert_eq!(
                    crate::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
                    ffi_err.extended_code
                );
                assert!(msg.contains("FOREIGN KEY"), "unexpected message: {msg}");
            }
            _ => panic!("expected a foreign key constraint error, got {e:?}"),
        }
    }

    fn assert_auth_error(e: &Error) {
        match e {
            Error::SqliteFailure(ffi_err, _) => {
                assert_eq!(
                    crate::ErrorCode::AuthorizationForStatementDenied,
                    ffi_err.code
                );
                assert_eq!(crate::ffi::SQLITE_AUTH, ffi_err.extended_code);
            }
            _ => panic!("expected an authorization error, got {e:?}"),
        }
    }

    #[test]
    fn test_finish_commit_fk_failure() -> Result<()> {
        let mut db = checked_memory_handle()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(
            "CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL);
             CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO f VALUES (0)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        // the commit error is reported, even though the cleanup rollback succeeded
        assert_fk_error(&err);
        // the cleanup rollback restored autocommit and undid the changes
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM f", [])?);
        // a new transaction can be started
        db.transaction()?.commit()
    }

    #[test]
    fn test_finish_commit_busy() -> Result<()> {
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("finish_busy.db3");
        Connection::open(&path)?
            .execute_batch("CREATE TABLE foo(x INTEGER); INSERT INTO foo VALUES(42);")?;

        let mut db1 = Connection::open(&path)?;
        let db2 = Connection::open(&path)?;
        db1.busy_timeout(Duration::from_millis(0))?;
        db2.busy_timeout(Duration::from_millis(0))?;

        // db2 holds a read transaction, so db1's commit cannot get the
        // exclusive lock it needs in rollback journal mode
        db2.execute_batch("BEGIN")?;
        db2.query_row("SELECT x FROM foo LIMIT 1", [], |_| Ok(()))?;

        let err = {
            let mut tx = db1.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        match &err {
            Error::SqliteFailure(ffi_err, _) => {
                assert_eq!(crate::ErrorCode::DatabaseBusy, ffi_err.code);
            }
            _ => panic!("expected a busy error, got {err:?}"),
        }
        // the cleanup rollback restored autocommit and undid the write
        assert!(db1.is_autocommit());
        assert_eq!(1, db1.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        db2.execute_batch("ROLLBACK")?;
        // a new transaction can be started
        db1.transaction()?.commit()
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_finish_commit_cleanup_rollback_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut db = checked_memory_handle()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(
            "CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL);
             CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let deny_rollback = Arc::new(AtomicBool::new(true));
        {
            let deny_rollback = Arc::clone(&deny_rollback);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } if deny_rollback.load(Ordering::SeqCst) => Authorization::Deny,
                _ => Authorization::Allow,
            }))?;
        }
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO f VALUES (0)", [])?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        // the original commit error is reported, not the denied cleanup rollback
        assert_fk_error(&err);
        // the cleanup rollback was denied, so the transaction is still active
        assert!(!db.is_autocommit());
        // the caller can lift the restriction and resolve the transaction
        deny_rollback.store(false, Ordering::SeqCst);
        db.execute_batch("ROLLBACK")?;
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM f", [])?);
        Ok(())
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_finish_rollback_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut db = checked_memory_handle()?;
        let deny_rollback = Arc::new(AtomicBool::new(true));
        {
            let deny_rollback = Arc::clone(&deny_rollback);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } if deny_rollback.load(Ordering::SeqCst) => Authorization::Deny,
                _ => Authorization::Allow,
            }))?;
        }
        let err = {
            let tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // default drop behavior: rollback
            tx.finish().unwrap_err()
        };
        assert_auth_error(&err);
        // the rollback was denied, so the transaction is still active
        assert!(!db.is_autocommit());
        // the caller can lift the restriction and keep using the connection
        deny_rollback.store(false, Ordering::SeqCst);
        db.execute_batch("COMMIT")?;
        assert!(db.is_autocommit());
        assert_current_sum(1, &db)
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_finish_commit_denied_by_authorizer() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut db = checked_memory_handle()?;
        let commit_attempts = Arc::new(AtomicUsize::new(0));
        let rollback_attempts = Arc::new(AtomicUsize::new(0));
        {
            let commit_attempts = Arc::clone(&commit_attempts);
            let rollback_attempts = Arc::clone(&rollback_attempts);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => Authorization::Allow,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => {
                    rollback_attempts.fetch_add(1, Ordering::SeqCst);
                    Authorization::Allow
                }
                AuthAction::Transaction { .. } => {
                    // COMMIT
                    commit_attempts.fetch_add(1, Ordering::SeqCst);
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))?;
        }
        let err = {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.finish().unwrap_err()
        };
        // the denial is reported as-is
        assert_auth_error(&err);
        // SQLite left the transaction active when the commit was denied, so
        // a single cleanup rollback was attempted (and allowed)
        assert!(db.is_autocommit());
        assert_eq!(1, rollback_attempts.load(Ordering::SeqCst));
        // the commit was attempted exactly once (dropping the finished
        // transaction did not retry it)
        assert_eq!(1, commit_attempts.load(Ordering::SeqCst));
        // nothing was committed
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        db.execute_batch("INSERT INTO foo VALUES(2)")?;
        assert_current_sum(2, &db)
    }

    #[test]
    fn test_savepoint_finish_commit_fk_failure() -> Result<()> {
        let mut db = checked_memory_handle()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(
            "CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL);
             CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let err = {
            let mut sp = db.savepoint()?;
            sp.execute("INSERT INTO f VALUES (0)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
            sp.finish().unwrap_err()
        };
        // the release error is reported, even though the cleanup succeeded
        assert_fk_error(&err);
        // the savepoint began the transaction, so the cleanup rollback
        // restored autocommit and undid the changes
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM f", [])?);
        // a new transaction can be started
        db.transaction()?.commit()
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_nested_savepoint_finish_commit_release_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let mut db = checked_memory_handle()?;
        // deny the first savepoint release only
        let deny_release = Arc::new(AtomicBool::new(true));
        let releases = Arc::new(AtomicUsize::new(0));
        {
            let deny_release = Arc::clone(&deny_release);
            let releases = Arc::clone(&releases);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Savepoint {
                    operation: TransactionOperation::Release,
                    ..
                } => {
                    releases.fetch_add(1, Ordering::SeqCst);
                    if deny_release.swap(false, Ordering::SeqCst) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                _ => Authorization::Allow,
            }))?;
        }
        let mut tx = db.transaction()?;
        tx.execute_batch("INSERT INTO foo VALUES(1)")?;
        let err = {
            let mut sp = tx.savepoint()?;
            sp.execute_batch("INSERT INTO foo VALUES(2)")?;
            sp.set_drop_behavior(DropBehavior::Commit);
            sp.finish().unwrap_err()
        };
        // the denied release is reported
        assert_auth_error(&err);
        // the cleanup undid only the savepoint's changes and released it:
        // one failed release plus one cleanup release, and no further release
        // when the savepoint was dropped
        assert_eq!(2, releases.load(Ordering::SeqCst));
        assert_current_sum(1, &tx)?;
        // the outer transaction is preserved and can still commit
        tx.execute_batch("INSERT INTO foo VALUES(4)")?;
        tx.commit()?;
        assert_current_sum(5, &db)
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_savepoint_finish_rollback_release_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let mut db = checked_memory_handle()?;
        let deny_release = Arc::new(AtomicBool::new(true));
        {
            let deny_release = Arc::clone(&deny_release);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Savepoint {
                    operation: TransactionOperation::Release,
                    ..
                } if deny_release.load(Ordering::SeqCst) => Authorization::Deny,
                _ => Authorization::Allow,
            }))?;
        }
        let mut tx = db.transaction()?;
        tx.execute_batch("INSERT INTO foo VALUES(1)")?;
        let err = {
            let sp = tx.savepoint()?;
            sp.execute_batch("INSERT INTO foo VALUES(2)")?;
            // default drop behavior: roll back to the savepoint, then release it
            sp.finish().unwrap_err()
        };
        // the rollback to the savepoint was allowed, but releasing it was
        // denied, and that first error is reported
        assert_auth_error(&err);
        // the savepoint's changes were rolled back...
        assert_current_sum(1, &tx)?;
        // ...and the savepoint is still active, so the caller can lift the
        // restriction and release it explicitly
        deny_release.store(false, Ordering::SeqCst);
        tx.execute_batch("RELEASE _rusqlite_sp")?;
        tx.execute_batch("INSERT INTO foo VALUES(4)")?;
        tx.commit()?;
        assert_current_sum(5, &db)
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_explicit_commit_denied_by_authorizer() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let mut db = checked_memory_handle()?;
        let commit_attempts = Arc::new(AtomicUsize::new(0));
        let rollback_attempts = Arc::new(AtomicUsize::new(0));
        {
            let commit_attempts = Arc::clone(&commit_attempts);
            let rollback_attempts = Arc::clone(&rollback_attempts);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => Authorization::Allow,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => {
                    rollback_attempts.fetch_add(1, Ordering::SeqCst);
                    Authorization::Allow
                }
                AuthAction::Transaction { .. } => {
                    // COMMIT
                    commit_attempts.fetch_add(1, Ordering::SeqCst);
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))?;
        }
        let err = {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.commit().unwrap_err()
        };
        // the denial is reported as-is
        assert_auth_error(&err);
        // a single cleanup rollback was attempted (and allowed)
        assert!(db.is_autocommit());
        assert_eq!(1, rollback_attempts.load(Ordering::SeqCst));
        // the commit was attempted exactly once: the explicit commit
        // overrides the drop behavior, so dropping the transaction did not
        // retry it
        assert_eq!(1, commit_attempts.load(Ordering::SeqCst));
        // nothing was committed
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        db.execute_batch("INSERT INTO foo VALUES(2)")?;
        assert_current_sum(2, &db)
    }

    #[test]
    fn test_explicit_commit_failure_no_panic() -> Result<()> {
        let mut db = checked_memory_handle()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(
            "CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL);
             CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let err = {
            let mut tx = db.transaction()?;
            tx.execute("INSERT INTO f VALUES (0)", [])?;
            // the explicit commit must return the database error, not panic
            tx.set_drop_behavior(DropBehavior::Panic);
            tx.commit().unwrap_err()
        };
        assert_fk_error(&err);
        // the cleanup rollback restored autocommit and undid the changes
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM f", [])?);
        db.transaction()?.commit()
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_explicit_rollback_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let mut db = checked_memory_handle()?;
        let deny_rollback = Arc::new(AtomicBool::new(true));
        let rollback_attempts = Arc::new(AtomicUsize::new(0));
        {
            let deny_rollback = Arc::clone(&deny_rollback);
            let rollback_attempts = Arc::clone(&rollback_attempts);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => {
                    rollback_attempts.fetch_add(1, Ordering::SeqCst);
                    if deny_rollback.load(Ordering::SeqCst) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                _ => Authorization::Allow,
            }))?;
        }
        let err = {
            let mut tx = db.transaction()?;
            tx.execute_batch("INSERT INTO foo VALUES(1)")?;
            // would commit on drop if the explicit rollback did not
            // override the drop behavior
            tx.set_drop_behavior(DropBehavior::Commit);
            tx.rollback().unwrap_err()
        };
        assert_auth_error(&err);
        // the rollback was attempted exactly once and denied, so the
        // transaction is still active and was not committed on drop
        assert_eq!(1, rollback_attempts.load(Ordering::SeqCst));
        assert!(!db.is_autocommit());
        // the caller can lift the restriction and resolve the transaction
        deny_rollback.store(false, Ordering::SeqCst);
        db.execute_batch("ROLLBACK")?;
        assert!(db.is_autocommit());
        assert_eq!(0, db.one_column::<i32, _>("SELECT COUNT(*) FROM foo", [])?);
        Ok(())
    }

    #[cfg(feature = "hooks")]
    #[test]
    fn test_explicit_savepoint_commit_release_denied() -> Result<()> {
        use crate::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let mut db = checked_memory_handle()?;
        // deny the first savepoint release only
        let deny_release = Arc::new(AtomicBool::new(true));
        let releases = Arc::new(AtomicUsize::new(0));
        {
            let deny_release = Arc::clone(&deny_release);
            let releases = Arc::clone(&releases);
            db.authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                AuthAction::Savepoint {
                    operation: TransactionOperation::Release,
                    ..
                } => {
                    releases.fetch_add(1, Ordering::SeqCst);
                    if deny_release.swap(false, Ordering::SeqCst) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }
                _ => Authorization::Allow,
            }))?;
        }
        let mut tx = db.transaction()?;
        tx.execute_batch("INSERT INTO foo VALUES(1)")?;
        let err = {
            let mut sp = tx.savepoint()?;
            sp.execute_batch("INSERT INTO foo VALUES(2)")?;
            sp.set_drop_behavior(DropBehavior::Commit);
            sp.commit().unwrap_err()
        };
        // the denied release is reported
        assert_auth_error(&err);
        // the cleanup undid only the savepoint's changes and released it:
        // one failed release plus one cleanup release, and no further release
        // when the savepoint was dropped
        assert_eq!(2, releases.load(Ordering::SeqCst));
        assert_current_sum(1, &tx)?;
        // the outer transaction is preserved and can still commit
        tx.execute_batch("INSERT INTO foo VALUES(4)")?;
        tx.commit()?;
        assert_current_sum(5, &db)
    }

    #[test]
    fn test_nested_savepoint_release_defers_fk_check() -> Result<()> {
        let mut db = checked_memory_handle()?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.execute_batch(
            "CREATE TABLE r(n INTEGER PRIMARY KEY NOT NULL);
             CREATE TABLE f(n REFERENCES r(n) DEFERRABLE INITIALLY DEFERRED);",
        )?;
        let mut tx = db.transaction()?;
        {
            let mut sp = tx.savepoint()?;
            sp.execute("INSERT INTO f VALUES (0)", [])?;
            sp.set_drop_behavior(DropBehavior::Commit);
            // releasing a nested savepoint must not check the deferred
            // foreign key constraints yet
            sp.finish()?;
        }
        // repair the violation before the outer commit checks it
        tx.execute("INSERT INTO r VALUES (0)", [])?;
        tx.commit()
    }
}
