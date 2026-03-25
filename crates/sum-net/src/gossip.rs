use anyhow::Result;
use libp2p::gossipsub::{self, IdentTopic, MessageId};
use tracing::{debug, info};

// ── Topic constants ───────────────────────────────────────────────────────────

pub const TOPIC_TEST: &str       = "sum/test/v1";
pub const TOPIC_CAPABILITY: &str = "sum/capability/v1";
pub const TOPIC_STORAGE: &str    = "sum/storage/v1";

// ── GossipManager ─────────────────────────────────────────────────────────────

/// Manages Gossipsub topic subscriptions and message publishing.
pub struct GossipManager {
    topic_test:       IdentTopic,
    topic_capability: IdentTopic,
    topic_storage:    IdentTopic,
}

impl GossipManager {
    pub fn new() -> Self {
        Self {
            topic_test:       IdentTopic::new(TOPIC_TEST),
            topic_capability: IdentTopic::new(TOPIC_CAPABILITY),
            topic_storage:    IdentTopic::new(TOPIC_STORAGE),
        }
    }

    /// Subscribe this node to all SUM Storage Node Gossipsub topics.
    pub fn subscribe_all(&self, gs: &mut gossipsub::Behaviour) -> Result<()> {
        for topic in self.all_topics() {
            if gs.subscribe(topic)? {
                debug!(topic = %topic, "subscribed to gossipsub topic");
            }
        }
        Ok(())
    }

    /// Publish raw bytes to a named topic.
    pub fn publish(
        &self,
        gs: &mut gossipsub::Behaviour,
        topic_name: &str,
        data: impl Into<Vec<u8>>,
    ) -> Result<MessageId> {
        let topic = self.topic_by_name(topic_name);
        let id = gs
            .publish(topic.clone(), data.into())
            .map_err(|e| anyhow::anyhow!("publish on '{topic_name}': {e}"))?;
        info!(topic = topic_name, "message published");
        Ok(id)
    }

    fn all_topics(&self) -> [&IdentTopic; 3] {
        [
            &self.topic_test,
            &self.topic_capability,
            &self.topic_storage,
        ]
    }

    fn topic_by_name(&self, name: &str) -> &IdentTopic {
        match name {
            TOPIC_CAPABILITY => &self.topic_capability,
            TOPIC_STORAGE    => &self.topic_storage,
            _                => &self.topic_test,
        }
    }
}

impl Default for GossipManager {
    fn default() -> Self {
        Self::new()
    }
}
