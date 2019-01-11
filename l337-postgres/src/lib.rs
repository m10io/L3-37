//! Postgres adapater for l3-37 pool
// #![deny(missing_docs, missing_debug_implementations)]

use futures::sync::oneshot;
use futures::{Async, Future};
use tokio::executor::spawn;
use tokio_postgres::error::Error;
use tokio_postgres::{Client, MakeTlsMode, Socket, TlsMode};

type Result<T> = std::result::Result<T, Error>;

pub struct AsyncConnection {
    pub client: Client,
    broken: bool,
    receiver: oneshot::Receiver<bool>,
}

/// A `ManageConnection` for `tokio_postgres::Connection`s.
pub struct PostgresConnectionManager<M> {
    connect_config: String,
    tls_mode: M,
}

impl<M> PostgresConnectionManager<M> {
    /// Create a new `PostgresConnectionManager`.
    pub fn new<C: Into<String>>(
        connect_config: C,
        tls_mode: M,
    ) -> Result<PostgresConnectionManager<M>> {
        Ok(PostgresConnectionManager {
            connect_config: connect_config.into(),
            tls_mode,
        })
    }
}

impl<M> l3_37::ManageConnection for PostgresConnectionManager<M>
where
    M: MakeTlsMode<Socket> + Clone + Send + Sync + 'static,
    M::Stream: Send,
    M::TlsMode: Send,
    <<M as MakeTlsMode<Socket>>::TlsMode as TlsMode<Socket>>::Future: Send,
{
    type Connection = AsyncConnection;
    type Error = Error;

    fn connect(
        &self,
    ) -> Box<Future<Item = Self::Connection, Error = l3_37::Error<Self::Error>> + 'static + Send>
    {
        Box::new(
            tokio_postgres::connect(&self.connect_config, self.tls_mode.clone())
                .map(|(client, connection)| {
                    let (sender, receiver) = oneshot::channel();
                    spawn(connection.map_err(|_| {
                        sender
                            .send(true)
                            .unwrap_or_else(|e| panic!("failed to send shutdown notice: {}", e));
                    }));
                    AsyncConnection {
                        broken: false,
                        client,
                        receiver,
                    }
                })
                .map_err(l3_37::Error::External),
        )
    }

    fn is_valid(
        &self,
        mut conn: Self::Connection,
    ) -> Box<Future<Item = (), Error = l3_37::Error<Self::Error>>> {
        // If we can execute this without erroring, we're definitely still connected to the datbase
        Box::new(
            conn.client
                .batch_execute("")
                .map_err(l3_37::Error::External),
        )
    }

    fn has_broken(&self, conn: &mut Self::Connection) -> bool {
        if conn.broken {
            return true;
        }

        match conn.receiver.poll() {
            // If we get any message, the connection task stopped, which means this connection is
            // now dead
            Ok(Async::Ready(_)) => {
                conn.broken = true;
                true
            }
            // If the future isn't ready, then we haven't sent a value which means the future is
            // stil successfully running
            Ok(Async::NotReady) => false,
            // This should never happen, we don't shutdown the future
            Err(err) => panic!("polling oneshot failed: {}", err),
        }
    }

    fn timed_out(&self) -> l3_37::Error<Self::Error> {
        unimplemented!()
        // Error::io(io::ErrorKind::TimedOut.into())
    }
}

impl<M> std::fmt::Debug for PostgresConnectionManager<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("PostgresConnectionManager")
            .field("connect_config", &self.connect_config)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use l3_37::{Config, Pool};
    use std::thread::sleep;
    use std::time::Duration;
    use tokio::runtime::current_thread::Runtime;
    use tokio_postgres::NoTls;

    #[test]
    fn it_works() {
        let mngr = PostgresConnectionManager::new(
            "postgres://pass_user:password@localhost:5433/postgres",
            NoTls,
        )
        .unwrap();

        let mut runtime = Runtime::new().expect("could not run");
        let config: Config = Default::default();
        let future = Pool::new(mngr, config).and_then(|pool| {
            pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 1::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(1, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| ((), connection))
                    .map_err(l3_37::Error::External)
            })
        });

        runtime.block_on(future).expect("could not run");
    }

    #[test]
    fn it_allows_multiple_queries_at_the_same_time() {
        let mngr = PostgresConnectionManager::new(
            "postgres://pass_user:password@localhost:5433/postgres",
            NoTls,
        )
        .unwrap();

        let mut runtime = Runtime::new().expect("could not run");
        let config: Config = Default::default();
        let future = Pool::new(mngr, config).and_then(|pool| {
            let q1 = pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 1::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(1, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| {
                        sleep(Duration::from_secs(5));
                        ((), connection)
                    })
                    .map_err(l3_37::Error::External)
            });

            let q2 = pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 2::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(2, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| {
                        sleep(Duration::from_secs(5));
                        ((), connection)
                    })
                    .map_err(l3_37::Error::External)
            });

            q1.join(q2)
        });

        runtime.block_on(future).expect("could not run");
    }

    #[test]
    fn it_reuses_connections() {
        let mngr = PostgresConnectionManager::new(
            "postgres://pass_user:password@localhost:5433/postgres",
            NoTls,
        )
        .unwrap();

        let mut runtime = Runtime::new().expect("could not run");
        let config: Config = Default::default();
        let future = Pool::new(mngr, config).and_then(|pool| {
            let q1 = pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 1::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(1, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| {
                        sleep(Duration::from_secs(5));
                        ((), connection)
                    })
                    .map_err(l3_37::Error::External)
            });

            let q2 = pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 2::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(2, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| {
                        sleep(Duration::from_secs(5));
                        ((), connection)
                    })
                    .map_err(l3_37::Error::External)
            });

            let q3 = pool.connection().and_then(|mut conn| {
                conn.client
                    .prepare("SELECT 3::INT4")
                    .and_then(move |select| {
                        conn.client.query(&select, &[]).for_each(|row| {
                            assert_eq!(3, row.get::<_, i32>(0));
                            Ok(())
                        })
                    })
                    .map(|connection| {
                        sleep(Duration::from_secs(5));
                        ((), connection)
                    })
                    .map_err(l3_37::Error::External)
            });

            q1.join3(q2, q3)
        });

        runtime.block_on(future).expect("could not run");
    }
}
