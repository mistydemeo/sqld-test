use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam::channel::RecvTimeoutError;
use rusqlite::types::Value;
use rusqlite::OpenFlags;
use tokio::sync::oneshot;
use tracing::warn;

use crate::query::{ErrorCode, QueryError, QueryResponse, QueryResult};
use crate::query_analysis::{State, Statements};

use super::{Database, TXN_TIMEOUT_SECS};

pub struct SQLiteDb {
    sender: crossbeam::channel::Sender<(Statements, oneshot::Sender<QueryResult>)>,
}

fn execute_query(conn: &rusqlite::Connection, stmts: &Statements) -> QueryResult {
    let mut result = vec![];
    let mut prepared = conn.prepare(&stmts.stmts)?;
    let columns: Vec<(String, Option<String>)> = prepared
        .columns()
        .iter()
        .map(|col| (col.name().into(), col.decl_type().map(str::to_lowercase)))
        .collect();
    let mut rows = prepared.query([])?;
    while let Some(row) = rows.next()? {
        let mut row_ = vec![];
        for (i, _) in columns.iter().enumerate() {
            row_.push(row.get::<usize, Value>(i)?);
        }
        result.push(row_);
    }
    Ok(QueryResponse::ResultSet(columns, result))
}

fn rollback(conn: &rusqlite::Connection) {
    conn.execute("rollback transaction;", ())
        .expect("failed to rollback");
}

macro_rules! ok_or_exit {
    ($e:expr) => {
        if let Err(_) = $e {
            return;
        }
    };
}

impl SQLiteDb {
    pub fn new(path: PathBuf) -> anyhow::Result<Self> {
        let (sender, receiver) =
            crossbeam::channel::unbounded::<(Statements, oneshot::Sender<QueryResult>)>();

        tokio::task::spawn_blocking(move || {
            let conn = crate::wal::open_with_virtual_wal(
                path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .unwrap();

            let mut state = State::Start;
            let mut timeout_deadline = None;
            let mut timedout = false;
            loop {
                let (stmts, sender) = match timeout_deadline {
                    Some(deadline) => match receiver.recv_deadline(deadline) {
                        Ok(msg) => msg,
                        Err(RecvTimeoutError::Timeout) => {
                            warn!("transaction timed out");
                            rollback(&conn);
                            timeout_deadline = None;
                            timedout = true;
                            state = State::Start;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match receiver.recv() {
                        Ok(msg) => msg,
                        Err(_) => break,
                    },
                };

                if !timedout {
                    let result = execute_query(&conn, &stmts);
                    match stmts.state(state) {
                        State::TxnOpened => {
                            timeout_deadline =
                                Some(Instant::now() + Duration::from_secs(TXN_TIMEOUT_SECS));
                            timedout = false;
                            state = State::TxnOpened;
                        }
                        State::TxnClosed => {
                            if result.is_ok() {
                                state = State::Start;
                                timedout = false;
                                timeout_deadline = None;
                            }
                        }
                        State::Start => (),
                        State::Invalid => panic!("invalid state!"),
                    }

                    ok_or_exit!(sender.send(result));
                } else {
                    ok_or_exit!(sender.send(Err(QueryError::new(
                        ErrorCode::TxTimeout,
                        "transaction timedout",
                    ))));
                    timedout = false;
                }
            }
        });

        Ok(Self { sender })
    }
}

#[async_trait::async_trait(?Send)]
impl Database for SQLiteDb {
    async fn execute(&self, query: Statements) -> QueryResult {
        let (sender, receiver) = oneshot::channel();
        let _ = self.sender.send((query, sender));
        receiver
            .await
            .map_err(|e| QueryError::new(ErrorCode::Internal, e))?
    }
}
