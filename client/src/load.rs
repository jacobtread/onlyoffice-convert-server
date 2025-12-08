use crate::{OfficeConvertClient, RequestError};
use bytes::Bytes;
use std::time::Duration;
use tokio::{
    sync::{Mutex, MutexGuard, Semaphore, SemaphorePermit},
    time::{Instant, sleep_until},
};
use tracing::{debug, error};

pub struct LoadBalancerConfig {
    /// Time in-between external busy checks
    pub retry_busy_check_after: Duration,
    /// Time to wait before repeated attempts
    pub retry_single_external: Duration,
    /// Timeout to wait on the notifier for
    pub notify_timeout: Duration,
    /// Number of attempts to retry a file for if the
    /// request fails due to connection loss
    pub retry_attempts: usize,
}

impl Default for LoadBalancerConfig {
    fn default() -> Self {
        Self {
            retry_busy_check_after: Duration::from_secs(5),
            retry_single_external: Duration::from_secs(1),
            notify_timeout: Duration::from_secs(120),
            retry_attempts: 3,
        }
    }
}

struct ClientSlot {
    /// The actual client
    client: OfficeConvertClient,

    /// If the server was busy the last time it was checked this is the
    /// timestamp when the next busy check is allowed to be performed
    next_busy_check: Option<Instant>,
}

/// Round robbin load balancer, will pass convert jobs
/// around to the next available client, connections
/// will wait until there is an available client
pub struct OfficeConvertLoadBalancer {
    /// Available clients the load balancer can use
    clients: Vec<Mutex<ClientSlot>>,

    /// Permit for each client to track number of currently
    /// used client and waiting for free clients
    client_permit: Semaphore,

    /// Timing for various actions
    config: LoadBalancerConfig,
}

enum TryAcquireResult<'a> {
    Acquired {
        client: MutexGuard<'a, ClientSlot>,
        permit: SemaphorePermit<'a>,
    },

    /// All clients are currently active
    BusyInternally,

    /// All available clients are currently blocked externally
    BusyExternally {
        /// Instant that clients should wake up at to check
        /// again for a new available client
        next_wake_time: Instant,
    },
}

impl OfficeConvertLoadBalancer {
    /// Creates a load balancer from the provided collection of clients
    ///
    /// ## Arguments
    /// * `clients` - The clients to load balance amongst
    pub fn new<I>(clients: I) -> Self
    where
        I: IntoIterator<Item = OfficeConvertClient>,
    {
        Self::new_with_timing(clients, Default::default())
    }

    /// Creates a load balancer from the provided collection of clients
    /// with timing configuration
    ///
    /// ## Arguments
    /// * `clients` - The clients to load balance amongst
    /// * `timing` - Timing configuration
    pub fn new_with_timing<I>(clients: I, timing: LoadBalancerConfig) -> Self
    where
        I: IntoIterator<Item = OfficeConvertClient>,
    {
        let clients = clients
            .into_iter()
            .map(|client| {
                Mutex::new(ClientSlot {
                    client,
                    next_busy_check: None,
                })
            })
            .collect::<Vec<_>>();

        let total_clients = clients.len();

        Self {
            clients,
            client_permit: Semaphore::new(total_clients),
            config: timing,
        }
    }

    pub async fn convert(&self, file: Bytes) -> Result<bytes::Bytes, RequestError> {
        let mut attempt = 0;

        let error = loop {
            let (client, _client_permit) = self.acquire_client().await;

            match client.client.convert(file.clone()).await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if error.is_retry() {
                        tracing::error!(
                            ?error,
                            "connection error while attempting to convert, retrying"
                        );

                        attempt += 1;

                        if attempt <= self.config.retry_attempts {
                            continue;
                        }

                        break error;
                    }

                    return Err(error);
                }
            }
        };

        Err(error)
    }

    /// Acquire a client, will wait until a new client is available
    async fn acquire_client(&self) -> (MutexGuard<'_, ClientSlot>, SemaphorePermit<'_>) {
        loop {
            match self.try_acquire_client().await {
                TryAcquireResult::Acquired { client, permit } => return (client, permit),

                TryAcquireResult::BusyInternally => {
                    // Retry immediately so we wait on acquiring the next permit. Realistically
                    // this state would only ever occur if a permit was obtained before a client
                    // lock was released
                    continue;
                }

                TryAcquireResult::BusyExternally { next_wake_time } => {
                    let now = Instant::now();

                    // Check for time drift
                    if now > next_wake_time {
                        continue;
                    }

                    // Sleep until next check
                    sleep_until(next_wake_time).await;
                }
            }
        }
    }

    /// Attempt to acquire a client that is ready to be used
    /// and attempt a conversion
    async fn try_acquire_client(&self) -> TryAcquireResult<'_> {
        // Acquire a permit to obtain a client
        let client_permit = self
            .client_permit
            .acquire()
            .await
            .expect("client permit was closed");

        let mut next_wake_time = None;

        for (index, slot) in self.clients.iter().enumerate() {
            let mut client_lock = match slot.try_lock() {
                Ok(client_lock) => client_lock,
                // Server is already in use, skip it
                Err(_) => continue,
            };

            let slot = &mut *client_lock;

            // If we have more than one client and this client was already checked for being busy earlier
            // then this client will be skipped and won't be checked until a later point
            if let Some(next_busy_check) = slot.next_busy_check {
                // If the busy check for this task is sooner than the next wake time prefer
                // to use the next busy check as the wake time
                if next_wake_time.is_none_or(|wake_time| next_busy_check < wake_time) {
                    next_wake_time = Some(next_busy_check);
                }

                let now = Instant::now();

                // This client is not ready to be checked yet
                if now < next_busy_check {
                    continue;
                }

                // Clear next busy check timestamp (We are about to re-check it)
                slot.next_busy_check = None;
            }

            // Check if the server is busy externally (Busy outside of our control)
            match slot.client.is_busy().await {
                // Server is not busy
                Ok(false) => {
                    debug!("obtained available server {index} for convert");
                    return TryAcquireResult::Acquired {
                        client: client_lock,
                        permit: client_permit,
                    };
                }

                // Client is busy (Externally)
                Ok(true) => {
                    debug!("server at {index} is busy externally");
                }

                // Erroneous clients are considered busy
                Err(err) => {
                    error!("failed to perform server busy check at {index}, assuming busy: {err}");
                }
            }

            // Compute the next busy check timestamp for the client
            let next_busy_check = Instant::now()
                .checked_add(self.config.retry_busy_check_after)
                .expect("time overflowed");
            slot.next_busy_check = Some(next_busy_check);

            if next_wake_time.is_none() {
                next_wake_time = Some(next_busy_check)
            }

            // ..check the next available client
        }

        if let Some(next_wake_time) = next_wake_time {
            TryAcquireResult::BusyExternally { next_wake_time }
        } else {
            TryAcquireResult::BusyInternally
        }
    }
}
