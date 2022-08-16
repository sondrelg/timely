extern crate redis;

use crate::_errors::TLError;
use crate::{MaxSleepExceededError, RedisError};
use log::{debug, info};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_asyncio::tokio::future_into_py;
use redis::{AsyncCommands, Client};

use crate::_utils::{
    get_script, now_millis, open_client_connection, receive_shared_state, send_shared_state,
    validate_redis_url,
};

/// Pure rust DTO for the data we need to pass to our thread
/// We could pass the Semaphore itself, but this seemed simpler.
pub(crate) struct ThreadState {
    pub(crate) client: Client,
    pub(crate) queue_key: String,
    pub(crate) capacity: u32,
    pub(crate) max_sleep: u32,
}

impl ThreadState {
    fn from(slf: &PyRef<Semaphore>) -> ThreadState {
        ThreadState {
            queue_key: slf.queue_key.clone(),
            capacity: slf.capacity,
            client: slf.client.clone(),
            max_sleep: slf.max_sleep,
        }
    }
}

#[pyclass]
#[pyo3(name = "Semaphore")]
#[pyo3(module = "timely")]
pub struct Semaphore {
    #[pyo3(get)]
    capacity: u32,
    #[pyo3(get)]
    queue_key: String,
    #[pyo3(get)]
    max_sleep: u32,
    client: Client,
}

#[pymethods]
impl Semaphore {
    /// Create a new class instance.
    #[new]
    fn new(
        name: String,
        capacity: u32,
        max_sleep: Option<u32>,
        redis_url: Option<&str>,
    ) -> PyResult<Self> {
        Ok(Self {
            capacity,
            queue_key: format!("__timely-{}", name),
            max_sleep: max_sleep.unwrap_or(0),
            client: validate_redis_url(redis_url)?,
        })
    }

    fn __aenter__<'a>(slf: PyRef<'_, Self>, py: Python<'a>) -> PyResult<&'a PyAny> {
        let receiver = send_shared_state::<ThreadState, TLError>(ThreadState::from(&slf))?;

        future_into_py(py, async {
            // Retrieve thread state struct
            let ts = receive_shared_state::<ThreadState, TLError>(receiver)?;

            // Connect to redis
            let mut connection = open_client_connection(&ts.client).await?;

            // Define queue if it doesn't already exist
            if get_script("src/scripts/rpushnx.lua")
                .key(&ts.queue_key)
                .arg(ts.capacity)
                .invoke_async(&mut connection)
                .await
                .map_err(|e| RedisError::new_err(e.to_string()))?
            {
                info!(
                    "Created new semaphore queue with a capacity of {}",
                    &ts.capacity
                );
            }

            // Wait for our turn - this waits non-blockingly until we're free to proceed
            let start = now_millis();
            connection
                .blpop::<&str, Option<()>>(&ts.queue_key, ts.max_sleep as usize)
                .await
                .map_err(|e| RedisError::new_err(e.to_string()))?;

            // Raise an exception if we waited too long
            if ts.max_sleep != 0 && (now_millis() - start) > (ts.max_sleep as f32 * 1000.0) as u64 {
                return Err(MaxSleepExceededError::new_err(
                    "Max sleep exceeded when waiting for Semaphore".to_string(),
                ));
            };

            debug!("Acquired semaphore");
            Ok(())
        })
    }

    #[args(_a = "*")]
    fn __aexit__<'a>(slf: PyRef<'_, Self>, py: Python<'a>, _a: &'a PyTuple) -> PyResult<&'a PyAny> {
        let receiver = send_shared_state::<ThreadState, TLError>(ThreadState::from(&slf))?;
        future_into_py(py, async {
            let ts = receive_shared_state::<ThreadState, TLError>(receiver)?;

            // Connect to redis
            let mut connection = open_client_connection(&ts.client).await?;

            // Define queue if it doesn't exist
            get_script("src/scripts/rpushx.lua")
                .key(&ts.queue_key)
                .invoke_async(&mut connection)
                .await
                .map_err(|e| RedisError::new_err(e.to_string()))?;

            debug!("Released semaphore");
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("Semaphore instance for queue {}", &self.queue_key)
    }
}