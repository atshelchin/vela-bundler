use std::{
    fmt::{Display, Formatter},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use iggy::prelude::{
    CompressionAlgorithm, Identifier, IggyClient, IggyDuration, IggyError, IggyExpiry, IggyMessage,
    MaxTopicSize, MessageClient, Partitioning, StreamClient, TopicClient,
};
use serde_json::Value;

use crate::utils::{
    config::IggyConfig,
    iggy::{ReconnectingIggyClient, is_session_error},
};

/// Retention of automatically provisioned UserOperation topics. Redis delayed payloads must live
/// at least this long after their latest retry because their original Iggy offset is acknowledged.
pub(crate) const USER_OPERATION_QUEUE_RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Durable admission queue for accepted UserOperations.
///
/// Redis and Iggy form a two-phase, at-least-once admission protocol. Redis is written first and
/// Iggy proves that the operation is executable. An enqueue error does not prove that no message
/// was appended: the connection can fail after Iggy commits but before its acknowledgement arrives.
#[derive(Clone)]
pub struct UserOperationQueue {
    client: ReconnectingIggyClient,
    topic: Identifier,
    topic_name: String,
    enqueue_timeout: Duration,
    topology_provisioner: ChainTopologyProvisioner,
}

#[derive(Clone)]
struct ChainTopologyProvisioner {
    client: ReconnectingIggyClient,
    provision_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Debug)]
pub struct UserOperationQueueError {
    message: &'static str,
    session: bool,
}

impl UserOperationQueueError {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            session: false,
        }
    }

    fn iggy(message: &'static str, error: &IggyError) -> Self {
        Self {
            message,
            session: is_session_error(error),
        }
    }

    /// True when the failure came from a dead Iggy session, so a retry only makes sense on a
    /// freshly rebuilt connection.
    fn is_session_error(&self) -> bool {
        self.session
    }
}

impl Display for UserOperationQueueError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for UserOperationQueueError {}

impl UserOperationQueue {
    /// Connects the message producer and a topology provisioner. The provisioner defaults to the
    /// producer credentials, but can use separately privileged credentials in production. It
    /// creates a stream only when the first producer write proves the stream or topic is absent.
    pub async fn connect(config: &IggyConfig) -> Result<Self, UserOperationQueueError> {
        let client =
            ReconnectingIggyClient::connect(&config.url, "producer", config.enqueue_timeout)
                .await
                .map_err(|_| UserOperationQueueError::new("could not connect to Iggy"))?;

        let provisioner_client = ReconnectingIggyClient::connect(
            &config.provisioner_url,
            "topology-provisioner",
            config.enqueue_timeout,
        )
        .await
        .map_err(|_| {
            UserOperationQueueError::new("could not connect to Iggy topology provisioner")
        })?;
        let topology_provisioner = ChainTopologyProvisioner {
            client: provisioner_client,
            provision_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        let topic: Identifier = config
            .topic
            .as_str()
            .try_into()
            .map_err(|_| UserOperationQueueError::new("invalid Iggy topic name"))?;
        tracing::info!(
            topic = %config.topic,
            automatic_topology_provisioning = true,
            "Iggy UserOperation queue connected"
        );

        Ok(Self {
            client,
            topic,
            topic_name: config.topic.clone(),
            enqueue_timeout: config.enqueue_timeout,
            topology_provisioner,
        })
    }

    /// Returns success only after Iggy confirms the write.
    ///
    /// Callers must treat every error after invoking this method as an unknown delivery outcome.
    /// In particular, they must retain the matching Redis admission record so a consumer can use
    /// an already-appended message as proof, or an idempotent producer retry can append it again.
    ///
    /// Each chain is isolated in its own `chain-{chain_id}` stream and therefore retains FIFO
    /// ordering without sharing a partition with another chain.
    pub async fn enqueue(
        &self,
        chain_id: u64,
        operation: &Value,
    ) -> Result<(), UserOperationQueueError> {
        let payload = serde_json::to_string(operation).map_err(|_| {
            UserOperationQueueError::new("could not serialize UserOperation queue entry")
        })?;
        let stream = stream_for_chain(chain_id)?;
        let stream_name = stream_name_for_chain(chain_id);

        match self.append(&stream, &payload).await {
            Ok(()) => Ok(()),
            Err(send_error) => {
                let topology_was_created = tokio::time::timeout(
                    self.enqueue_timeout,
                    self.topology_provisioner.ensure_chain_topic(
                        &stream,
                        &stream_name,
                        &self.topic,
                        &self.topic_name,
                    ),
                )
                .await
                .map_err(|_| {
                    UserOperationQueueError::new("Iggy topology provisioning timed out")
                })??;
                if !topology_was_created {
                    return Err(send_error);
                }

                self.append(&stream, &payload).await
            }
        }
    }

    async fn append(
        &self,
        stream: &Identifier,
        payload: &str,
    ) -> Result<(), UserOperationQueueError> {
        let client = self.client.get().await;
        match self.append_with(&client, stream, payload).await {
            // A dead session fails every command until the shared client is rebuilt. The retry
            // is safe under the at-least-once admission contract: consumers are idempotent, so a
            // write that committed before the session died only produces a duplicate.
            Err(error) if error.is_session_error() => {
                self.client
                    .reconnect_if_current(&client)
                    .await
                    .map_err(|_| UserOperationQueueError::new("could not reconnect to Iggy"))?;
                let client = self.client.get().await;
                self.append_with(&client, stream, payload).await
            }
            result => result,
        }
    }

    async fn append_with(
        &self,
        client: &IggyClient,
        stream: &Identifier,
        payload: &str,
    ) -> Result<(), UserOperationQueueError> {
        let message = IggyMessage::from_str(payload)
            .map_err(|_| UserOperationQueueError::new("UserOperation queue entry is invalid"))?;
        let mut messages = [message];

        match tokio::time::timeout(
            self.enqueue_timeout,
            client.send_messages(
                stream,
                &self.topic,
                &Partitioning::balanced(),
                &mut messages,
            ),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(UserOperationQueueError::iggy(
                "Iggy rejected the UserOperation queue entry",
                &error,
            )),
            Err(_) => Err(UserOperationQueueError::new(
                "Iggy UserOperation queue write timed out",
            )),
        }
    }
}

impl ChainTopologyProvisioner {
    /// Creates `chain-{id}` and its single-partition topic only after a producer write failed.
    /// A shared lock avoids duplicate create races among concurrent requests in one relay.
    /// Returns false when the stream and topic already exist, so an unrelated producer error is
    /// not hidden by a successful metadata lookup.
    async fn ensure_chain_topic(
        &self,
        stream: &Identifier,
        stream_name: &str,
        topic: &Identifier,
        topic_name: &str,
    ) -> Result<bool, UserOperationQueueError> {
        let _lock = self.provision_lock.lock().await;
        let client = self.client.get().await;
        match Self::ensure_with_client(&client, stream, stream_name, topic, topic_name).await {
            Err(error) if error.is_session_error() => {
                self.client
                    .reconnect_if_current(&client)
                    .await
                    .map_err(|_| {
                        UserOperationQueueError::new(
                            "could not reconnect to Iggy topology provisioner",
                        )
                    })?;
                let client = self.client.get().await;
                Self::ensure_with_client(&client, stream, stream_name, topic, topic_name).await
            }
            result => result,
        }
    }

    async fn ensure_with_client(
        client: &IggyClient,
        stream: &Identifier,
        stream_name: &str,
        topic: &Identifier,
        topic_name: &str,
    ) -> Result<bool, UserOperationQueueError> {
        let stream_was_missing = client
            .get_stream(stream)
            .await
            .map_err(|error| {
                UserOperationQueueError::iggy("could not inspect Iggy chain stream", &error)
            })?
            .is_none();
        if stream_was_missing {
            Self::create_stream_if_missing(client, stream, stream_name).await?;
        }

        let topic_was_missing = client
            .get_topic(stream, topic)
            .await
            .map_err(|error| {
                UserOperationQueueError::iggy("could not inspect Iggy chain topic", &error)
            })?
            .is_none();
        if topic_was_missing {
            Self::create_topic_if_missing(client, stream, topic, topic_name).await?;
        }

        if stream_was_missing || topic_was_missing {
            tracing::info!(
                stream = stream_name,
                topic = topic_name,
                "created Iggy UserOperation queue topology"
            );
        }
        Ok(stream_was_missing || topic_was_missing)
    }

    async fn create_stream_if_missing(
        client: &IggyClient,
        stream: &Identifier,
        stream_name: &str,
    ) -> Result<(), UserOperationQueueError> {
        if client.create_stream(stream_name).await.is_ok() {
            return Ok(());
        }

        if client
            .get_stream(stream)
            .await
            .map_err(|error| {
                UserOperationQueueError::iggy("could not inspect Iggy chain stream", &error)
            })?
            .is_some()
        {
            return Ok(());
        }

        Err(UserOperationQueueError::new(
            "could not create Iggy chain stream",
        ))
    }

    async fn create_topic_if_missing(
        client: &IggyClient,
        stream: &Identifier,
        topic: &Identifier,
        topic_name: &str,
    ) -> Result<(), UserOperationQueueError> {
        let expiry = IggyExpiry::ExpireDuration(IggyDuration::new(USER_OPERATION_QUEUE_RETENTION));
        if client
            .create_topic(
                stream,
                topic_name,
                1,
                CompressionAlgorithm::None,
                None,
                expiry,
                MaxTopicSize::ServerDefault,
            )
            .await
            .is_ok()
        {
            return Ok(());
        }

        if client
            .get_topic(stream, topic)
            .await
            .map_err(|error| {
                UserOperationQueueError::iggy("could not inspect Iggy chain topic", &error)
            })?
            .is_some()
        {
            return Ok(());
        }

        Err(UserOperationQueueError::new(
            "could not create Iggy chain topic",
        ))
    }
}

fn stream_for_chain(chain_id: u64) -> Result<Identifier, UserOperationQueueError> {
    stream_name_for_chain(chain_id)
        .as_str()
        .try_into()
        .map_err(|_| UserOperationQueueError::new("invalid Iggy stream name for chain"))
}

fn stream_name_for_chain(chain_id: u64) -> String {
    format!("chain-{chain_id}")
}

#[cfg(test)]
mod tests {
    use std::{env, time::Duration};

    use serde_json::json;

    use super::{UserOperationQueue, stream_for_chain};
    use crate::utils::config::IggyConfig;

    #[test]
    fn derives_a_stream_name_from_the_chain_id() {
        let stream = stream_for_chain(42_161).unwrap();

        assert_eq!(stream.get_string_value().unwrap(), "chain-42161");
    }

    #[tokio::test]
    #[ignore = "requires a running Iggy server and VELA_RELAY_IGGY_URL"]
    async fn appends_to_a_preprovisioned_chain_stream() {
        let queue = UserOperationQueue::connect(&IggyConfig {
            url: env::var("VELA_RELAY_IGGY_URL").expect("Iggy connection URL"),
            consumer_url: env::var("VELA_RELAY_IGGY_CONSUMER_URL")
                .unwrap_or_else(|_| env::var("VELA_RELAY_IGGY_URL").expect("Iggy connection URL")),
            provisioner_url: env::var("VELA_RELAY_IGGY_PROVISIONER_URL")
                .unwrap_or_else(|_| env::var("VELA_RELAY_IGGY_URL").expect("Iggy connection URL")),
            topic: "default".into(),
            enqueue_timeout: Duration::from_secs(5),
        })
        .await
        .expect("connect to Iggy");

        queue
            .enqueue(
                1,
                &json!({
                    "schemaVersion": 1,
                    "userOperationHash": "0xiggy-integration-test",
                    "chainId": 1,
                }),
            )
            .await
            .expect("append test envelope");
    }
}
