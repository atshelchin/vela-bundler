//! Session-loss detection and self-healing reconnection for shared Iggy clients.
//!
//! The Iggy SDK transparently re-establishes dropped TCP connections, but a re-login can fail on
//! the server (for example when the server already evicted the session after a stale heartbeat).
//! The client is then permanently connected-but-unauthenticated and every command fails until a
//! brand-new client is built. The helpers here classify that state and rebuild clients on demand.

use std::{fmt::Display, sync::Arc, time::Duration};

use iggy::prelude::{Client, IggyClient, IggyError};
use tokio::sync::{Mutex, RwLock};

/// True when the client's session is dead (dropped, deauthenticated, or forgotten by the server)
/// and no retry on the same client can succeed — only a freshly built connection can.
pub(crate) fn is_session_error(error: &IggyError) -> bool {
    matches!(
        error,
        IggyError::Unauthenticated
            | IggyError::InvalidCredentials
            | IggyError::Disconnected
            | IggyError::NotConnected
            | IggyError::StaleClient
            | IggyError::ConnectionClosed
            | IggyError::ClientNotFound(_)
    )
}

#[derive(Debug)]
pub(crate) enum IggyReconnectError {
    Timeout,
    Iggy(IggyError),
}

impl Display for IggyReconnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("connection attempt timed out"),
            Self::Iggy(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for IggyReconnectError {}

/// A shared Iggy client that request paths can swap out after its session dies.
///
/// `get` hands out the current client without blocking behind a rebuild. A caller whose command
/// failed with a session error passes that same client to `reconnect_if_current`; the first such
/// caller rebuilds the connection while concurrent callers wait and then reuse the replacement
/// instead of stampeding the server with duplicate rebuilds.
#[derive(Clone)]
pub(crate) struct ReconnectingIggyClient {
    connection_string: Arc<str>,
    label: &'static str,
    connect_timeout: Duration,
    client: Arc<RwLock<Arc<IggyClient>>>,
    rebuild: Arc<Mutex<()>>,
}

impl ReconnectingIggyClient {
    pub(crate) async fn connect(
        connection_string: &str,
        label: &'static str,
        connect_timeout: Duration,
    ) -> Result<Self, IggyReconnectError> {
        let client = build_client(connection_string, connect_timeout).await?;
        Ok(Self {
            connection_string: connection_string.into(),
            label,
            connect_timeout,
            client: Arc::new(RwLock::new(Arc::new(client))),
            rebuild: Arc::new(Mutex::new(())),
        })
    }

    pub(crate) async fn get(&self) -> Arc<IggyClient> {
        self.client.read().await.clone()
    }

    /// Replaces the shared client, unless another caller already replaced `stale`.
    pub(crate) async fn reconnect_if_current(
        &self,
        stale: &Arc<IggyClient>,
    ) -> Result<(), IggyReconnectError> {
        let _guard = self.rebuild.lock().await;
        if !Arc::ptr_eq(&*self.client.read().await, stale) {
            return Ok(());
        }

        tracing::warn!(client = self.label, "Iggy session is dead, reconnecting");
        let replacement = build_client(&self.connection_string, self.connect_timeout).await?;
        let replacement = Arc::new(replacement);
        let previous = {
            let mut current = self.client.write().await;
            std::mem::replace(&mut *current, replacement)
        };
        if let Err(error) = previous.shutdown().await {
            tracing::debug!(
                client = self.label,
                %error,
                "could not cleanly shut down the dead Iggy client"
            );
        }
        tracing::info!(client = self.label, "Iggy connection re-established");
        Ok(())
    }
}

async fn build_client(
    connection_string: &str,
    connect_timeout: Duration,
) -> Result<IggyClient, IggyReconnectError> {
    let client =
        IggyClient::from_connection_string(connection_string).map_err(IggyReconnectError::Iggy)?;
    match tokio::time::timeout(connect_timeout, client.connect()).await {
        Ok(Ok(())) => Ok(client),
        Ok(Err(error)) => Err(IggyReconnectError::Iggy(error)),
        Err(_) => Err(IggyReconnectError::Timeout),
    }
}

#[cfg(test)]
mod tests {
    use iggy::prelude::IggyError;

    use super::is_session_error;

    #[test]
    fn classifies_dead_session_errors() {
        for error in [
            IggyError::Unauthenticated,
            IggyError::InvalidCredentials,
            IggyError::Disconnected,
            IggyError::NotConnected,
            IggyError::StaleClient,
            IggyError::ConnectionClosed,
            IggyError::ClientNotFound(0),
        ] {
            assert!(is_session_error(&error), "expected a session error");
        }

        assert!(!is_session_error(&IggyError::InvalidCommand));
        assert!(!is_session_error(&IggyError::CannotParseUrl));
    }
}
