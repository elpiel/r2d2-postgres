#![doc(html_root_url="https://sfackler.github.io/doc")]
#![feature(if_let, unsafe_destructor)]
extern crate r2d2;
extern crate postgres;

use std::cell::RefCell;
use std::collections::LruCache;
use std::default::Default;
use std::fmt;
use std::mem;
use std::rc::Rc;
use postgres::{PostgresConnection,
               PostgresConnectParams,
               IntoConnectParams,
               SslMode,
               PostgresResult,
               PostgresStatement,
               PostgresCopyInStatement,
               PostgresTransaction};
use postgres::error::{PostgresConnectError, PostgresError};
use postgres::types::ToSql;

pub enum Error {
    ConnectError(PostgresConnectError),
    OtherError(PostgresError),
}

impl fmt::Show for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConnectError(ref e) => write!(fmt, "{}", e),
            OtherError(ref e) => write!(fmt, "{}", e),
        }
    }
}

pub struct PostgresPoolManager {
    params: Result<PostgresConnectParams, PostgresConnectError>,
    ssl_mode: SslMode,
}

impl PostgresPoolManager {
    pub fn new<T: IntoConnectParams>(params: T, ssl_mode: SslMode) -> PostgresPoolManager {
        PostgresPoolManager {
            params: params.into_connect_params(),
            ssl_mode: ssl_mode,
        }
    }
}

impl r2d2::PoolManager<PostgresConnection, Error> for PostgresPoolManager {
    fn connect(&self) -> Result<PostgresConnection, Error> {
        match self.params {
            Ok(ref p) => {
                PostgresConnection::connect(p.clone(), &self.ssl_mode).map_err(ConnectError)
            }
            Err(ref e) => Err(ConnectError(e.clone()))
        }
    }

    fn is_valid(&self, conn: &mut PostgresConnection) -> Result<(), Error> {
        conn.batch_execute("").map_err(OtherError)
    }

    fn has_broken(&self, conn: &mut PostgresConnection) -> bool {
        conn.is_desynchronized()
    }
}

pub struct Config {
    pub statement_pool_size: uint,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            statement_pool_size: 10,
        }
    }
}

pub struct StatementPoolingManager {
    manager: PostgresPoolManager,
    config: Config,
}

impl StatementPoolingManager {
    pub fn new<T>(params: T, ssl_mode: SslMode, config: Config) -> StatementPoolingManager
            where T: IntoConnectParams {
        StatementPoolingManager {
            manager: PostgresPoolManager::new(params, ssl_mode),
            config: config
        }
    }
}

impl r2d2::PoolManager<Connection, Error> for StatementPoolingManager {
    fn connect(&self) -> Result<Connection, Error> {
        Ok(Connection {
            conn: box try!(self.manager.connect()),
            stmts: RefCell::new(LruCache::new(self.config.statement_pool_size))
        })
    }

    fn is_valid(&self, conn: &mut Connection) -> Result<(), Error> {
        self.manager.is_valid(&mut *conn.conn)
    }

    fn has_broken(&self, conn: &mut Connection) -> bool {
        self.manager.has_broken(&mut *conn.conn)
    }
}

pub trait GenericConnection {
    /// Like `PostgresConnection::prepare`.
    fn prepare<'a>(&'a self, query: &str) -> PostgresResult<Rc<PostgresStatement<'a>>>;

    /// Like `PostgresConnection::execute`.
    fn execute(&self, query: &str, params: &[&ToSql]) -> PostgresResult<uint> {
        self.prepare(query).and_then(|s| s.execute(params))
    }

    /// Like `PostgresConnection::prepare_copy_in`.
    fn prepare_copy_in<'a>(&'a self, table: &str, columns: &[&str])
                           -> PostgresResult<PostgresCopyInStatement<'a>>;

    /// Like `PostgresConnection::transaction`.
    fn transaction<'a>(&'a self) -> PostgresResult<Transaction<'a>>;

    /// Like `PostgresConnection::batch_execute`.
    fn batch_execute(&self, query: &str) -> PostgresResult<()>;
}

pub struct Connection {
    conn: Box<PostgresConnection>,
    stmts: RefCell<LruCache<String, Rc<PostgresStatement<'static>>>>,
}

#[unsafe_destructor]
impl Drop for Connection {
    // Just make sure that all the statements drop before the connection
    fn drop(&mut self) {
        self.stmts.borrow_mut().change_capacity(0);
    }
}

impl GenericConnection for Connection {
    fn prepare<'a>(&'a self, query: &str) -> PostgresResult<Rc<PostgresStatement<'a>>> {
        let query = query.into_string();
        let mut stmts = self.stmts.borrow_mut();

        if let Some(stmt) = stmts.get(&query) {
            return Ok(unsafe { mem::transmute(stmt.clone()) });
        }

        let stmt = Rc::new(try!(self.conn.prepare(query[])));
        stmts.put(query, unsafe { mem::transmute(stmt.clone()) });
        Ok(stmt)
    }

    fn prepare_copy_in<'a>(&'a self, table: &str, columns: &[&str])
                           -> PostgresResult<PostgresCopyInStatement<'a>> {
        self.conn.prepare_copy_in(table, columns)
    }

    fn transaction<'a>(&'a self) -> PostgresResult<Transaction<'a>> {
        Ok(Transaction {
            conn: self,
            trans: try!(self.conn.transaction())
        })
    }

    fn batch_execute(&self, query: &str) -> PostgresResult<()> {
        self.conn.batch_execute(query)
    }
}

pub struct Transaction<'a> {
    conn: &'a Connection,
    trans: PostgresTransaction<'a>
}

impl<'a> GenericConnection for Transaction<'a> {
    fn prepare<'a>(&'a self, query: &str) -> PostgresResult<Rc<PostgresStatement<'a>>> {
        let query = query.into_string();
        let mut stmts = self.conn.stmts.borrow_mut();

        if let Some(stmt) = stmts.get(&query) {
            return Ok(unsafe { mem::transmute(stmt.clone()) });
        }

        Ok(Rc::new(try!(self.trans.prepare(query[]))))
    }

    fn prepare_copy_in<'a>(&'a self, table: &str, columns: &[&str])
                           -> PostgresResult<PostgresCopyInStatement<'a>> {
        self.trans.prepare_copy_in(table, columns)
    }

    fn transaction<'a>(&'a self) -> PostgresResult<Transaction<'a>> {
        Ok(Transaction {
            conn: self.conn,
            trans: try!(self.trans.transaction())
        })
    }

    fn batch_execute(&self, query: &str) -> PostgresResult<()> {
        self.trans.batch_execute(query)
    }
}
