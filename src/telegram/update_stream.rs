//! Keeping an update stream alive when its connection drops.
//!
//! grammers reconnects on the next request and resolves the missed updates with getDifference, so a failed
//! `next()` is retried on the same stream. Ending the process instead made every short network drop a restart,
//! and every restart re-announced each saved batch to its owner.

use std::time::Duration;

use grammers_client::InvocationError;

use crate::media::download::{Failure, classify};

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    /// The sender is gone or the account is logged out: retrying cannot help.
    Fatal,
    Transient,
}

pub fn fault(error: &InvocationError) -> Fault {
    if let InvocationError::Dropped(reason) = error
        && reason.contains("updates channel was closed")
    {
        return Fault::Fatal;
    }
    match classify(error) {
        Failure::SessionDead => Fault::Fatal,
        _ => Fault::Transient,
    }
}

/// Doubling waits between retries, back to the first one after an update arrives.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    pub fn delay(&mut self) -> Duration {
        let delay = FIRST_DELAY
            .saturating_mul(2u32.saturating_pow(self.failures))
            .min(MAX_DELAY);
        self.failures = self.failures.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grammers_client::sender::RpcError;

    fn rpc(code: i32, name: &str) -> InvocationError {
        InvocationError::Rpc(RpcError {
            code,
            name: name.to_string(),
            value: None,
            caused_by: None,
        })
    }

    #[test]
    fn a_dropped_connection_is_retried() {
        let error = InvocationError::Dropped("SenderPoolRunner was dropped during receive");
        assert_eq!(fault(&error), Fault::Transient);
        assert_eq!(fault(&rpc(500, "RPC_CALL_FAIL")), Fault::Transient);
    }

    #[test]
    fn a_closed_channel_or_dead_session_ends_the_stream() {
        let closed = InvocationError::Dropped("updates channel was closed");
        assert_eq!(fault(&closed), Fault::Fatal);
        assert_eq!(fault(&rpc(401, "AUTH_KEY_UNREGISTERED")), Fault::Fatal);
    }

    #[test]
    fn waits_double_up_to_the_cap_and_reset() {
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..7).map(|_| backoff.delay().as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 30, 30]);
        backoff.reset();
        assert_eq!(backoff.delay().as_secs(), 1);
    }
}
