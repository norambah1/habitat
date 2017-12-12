use std::ops::{Deref, DerefMut};
use std::thread;
use std::time::Duration;
use std::fmt;

use r2d2;
use diesel::pg::PgConnection;
use r2d2_diesel::ConnectionManager;

use config::DataStoreCfg;
use error::{Error, Result};

#[derive(Clone)]
pub struct DieselPool {
    inner: r2d2::Pool<ConnectionManager<PgConnection>>,
}

impl fmt::Debug for DieselPool {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Nope")
    }
}

impl DieselPool {
    pub fn new(config: &DataStoreCfg) -> Result<DieselPool> {
        loop {
            let manager = ConnectionManager::<PgConnection>::new(config.to_string());
            match r2d2::Pool::builder()
                .max_size(config.pool_size)
                .connection_timeout(Duration::from_secs(config.connection_timeout_sec))
                .build(manager) {
                Ok(pool) => return Ok(DieselPool { inner: pool }),
                Err(e) => {
                    error!(
                        "Error initializing connection pool to Postgres, will retry: {}",
                        e
                    )
                }
            }
            thread::sleep(Duration::from_millis(config.connection_retry_ms));
        }
    }

    pub fn get_raw(&self) -> Result<r2d2::PooledConnection<ConnectionManager<PgConnection>>> {
        let conn = self.inner.get().map_err(Error::ConnectionTimeout)?;
        Ok(conn)
    }
}

impl Deref for DieselPool {
    type Target = r2d2::Pool<ConnectionManager<PgConnection>>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for DieselPool {
    fn deref_mut(&mut self) -> &mut r2d2::Pool<ConnectionManager<PgConnection>> {
        &mut self.inner
    }
}
