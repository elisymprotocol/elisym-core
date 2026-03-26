use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Entry;
use std::time::Duration;

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::identity::{AgentIdentity, CapabilityCard};
use crate::types::{kind, KIND_APP_HANDLER};

/// Convert a capability name to its Nostr d-tag form (lowercase, spaces → hyphens).
pub fn to_d_tag(name: &str) -> String {
    name.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// A discovered agent with its capability cards and event metadata.
#[derive(Debug, Clone)]
pub struct DiscoveredAgent {
    pub pubkey: PublicKey,
    pub cards: Vec<CapabilityCard>,
    pub event_id: EventId,
    pub supported_kinds: Vec<u16>,
    /// Number of requested capabilities that matched (for relevance sorting).
    pub match_count: usize,
}

/// Filter for searching agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentFilter {
    pub capabilities: Vec<String>,
    pub job_kind: Option<u16>,
    pub since: Option<Timestamp>,
    /// Maximum number of agents to return. `None` means no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Free-text query to match against agent name and description (case-insensitive substring).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

/// Check if a single query capability matches any of the event's tags.
///
/// Matching is fuzzy: both the query and tags are split on delimiters (`-`, `_`, ` `)
/// into tokens. A query token matches a tag token if they are equal or one is a prefix
/// of the other (min 3 chars).
fn matches_capability(query_cap: &str, event_tags: &HashSet<&str>) -> bool {
    // Exact match first (fast path)
    if event_tags.contains(query_cap) {
        return true;
    }
    // Fuzzy: split query into tokens, check if every token
    // appears in at least one tag's tokens
    let query_tokens: Vec<&str> = query_cap
        .split(['-', '_', ' '])
        .filter(|t| !t.is_empty())
        .collect();
    query_tokens.iter().all(|qt| {
        let qt_lower = qt.to_lowercase();
        event_tags.iter().any(|tag| {
            tag.split(['-', '_', ' ']).any(|tt| {
                let tt_lower = tt.to_lowercase();
                tt_lower == qt_lower
                    || (qt_lower.len() >= 3 && tt_lower.starts_with(&qt_lower))
                    || (tt_lower.len() >= 3 && qt_lower.starts_with(&tt_lower))
            })
        })
    })
}

/// Check if a free-text query matches a capability card (name, description, or capabilities).
fn matches_query(query: &str, card: &CapabilityCard) -> bool {
    let q = query.to_lowercase();
    let name_lower = card.name.to_lowercase();
    let desc_lower = card.description.to_lowercase();
    name_lower.contains(&q)
        || desc_lower.contains(&q)
        || card
            .capabilities
            .iter()
            .any(|c| c.to_lowercase().contains(&q))
}

/// Service for publishing and discovering agent capabilities via NIP-89.
#[derive(Debug, Clone)]
pub struct DiscoveryService {
    client: Client,
    identity: AgentIdentity,
}

impl DiscoveryService {
    pub fn new(client: Client, identity: AgentIdentity) -> Self {
        Self { client, identity }
    }

    /// Publish a capability card as a NIP-89 kind:31990 parameterized replaceable event.
    pub async fn publish_capability(
        &self,
        card: &CapabilityCard,
        supported_job_kinds: &[u16],
    ) -> Result<EventId> {
        if card.payment.is_none() {
            return Err(crate::error::ElisymError::InvalidCapabilityCard(
                "payment info is required to publish a capability card".into(),
            ));
        }
        let content = card.to_json()?;
        let d_tag = to_d_tag(&card.name);

        let mut tags: Vec<Tag> = vec![
            Tag::identifier(d_tag),
            Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec!["elisym".to_string()],
            ),
        ];

        for k in supported_job_kinds {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::K)),
                vec![k.to_string()],
            ));
        }

        for cap in &card.capabilities {
            tags.push(Tag::custom(
                TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::T)),
                vec![cap.clone()],
            ));
        }

        let builder = EventBuilder::new(kind(KIND_APP_HANDLER), content).tags(tags);
        let output = self.client.send_event_builder(builder).await?;

        tracing::info!(event_id = %output.val, "Published capability card");
        Ok(output.val)
    }

    /// Update the Nostr kind:0 profile metadata from the capability card.
    /// Call once at agent startup to set name, description, picture, and website.
    pub async fn update_profile(&self, card: &CapabilityCard) -> Result<EventId> {
        let pubkey_hex = self.identity.public_key().to_hex();
        let picture_url = format!("https://robohash.org/{pubkey_hex}");
        let about = format!("{} | Powered by Elisym", card.description);
        let metadata = Metadata::new()
            .name(&card.name)
            .about(about)
            .picture(Url::parse(&picture_url).expect("valid robohash URL"))
            .website(Url::parse("https://elisym.network").expect("valid elisym URL"));
        let output = self.client.set_metadata(&metadata).await?;
        tracing::info!(event_id = %output.val, "Updated Nostr profile (kind:0)");
        Ok(output.val)
    }

    /// Search for agents matching the given filter.
    ///
    /// NIP-01 `custom_tag` uses OR semantics for multiple values, so we fetch
    /// all elisym agents from relays and apply capability filtering locally
    /// to get correct AND semantics (agent must have ALL requested capabilities).
    ///
    /// **Scalability note:** This fetches all elisym capability cards from relays
    /// and filters client-side. At small scale this is fine, but on a busy network
    /// with thousands of agents it will become expensive. The `limit` field
    /// truncates results *after* fetching. Relay-side pagination or caching may
    /// be needed for production at scale.
    pub async fn search_agents(&self, filter: &AgentFilter) -> Result<Vec<DiscoveredAgent>> {
        let mut f = Filter::new().kind(kind(KIND_APP_HANDLER));

        // Only filter by "elisym" tag on the relay side — adding capabilities
        // here would use OR semantics (NIP-01), returning agents matching ANY tag.
        f = f.custom_tag(
            SingleLetterTag::lowercase(Alphabet::T),
            vec!["elisym".to_string()],
        );

        if let Some(job_kind) = filter.job_kind {
            f = f.custom_tag(
                SingleLetterTag::lowercase(Alphabet::K),
                vec![job_kind.to_string()],
            );
        }

        if let Some(since) = filter.since {
            f = f.since(since);
        }

        let events = self
            .client
            .fetch_events(vec![f], Some(Duration::from_secs(10)))
            .await?;

        // Dedup by (pubkey, d-tag) — same card may arrive from multiple relays.
        // Keep only the newest event per (pubkey, d-tag) pair.
        let mut latest_by_key: HashMap<(PublicKey, String), Event> = HashMap::new();
        for event in events {
            let d_tag = event
                .tags
                .iter()
                .find_map(|t| {
                    let s = t.as_slice();
                    if s.first().map(|v| v.as_str()) == Some("d") {
                        s.get(1).map(|v| v.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            let key = (event.pubkey, d_tag);
            match latest_by_key.entry(key) {
                Entry::Vacant(e) => { e.insert(event); }
                Entry::Occupied(mut e) => {
                    if event.created_at > e.get().created_at {
                        e.insert(event);
                    }
                }
            }
        }

        // Group parsed cards and tags by pubkey, then compute match_count once
        // over the union of all capability tags per agent.
        struct CardEntry {
            card: CapabilityCard,
            created_at: Timestamp,
            tags: Vec<String>,
            kinds: Vec<u16>,
        }

        struct AgentAccum {
            entries: Vec<CardEntry>,
            event_id: EventId,
            latest_created_at: Timestamp,
        }

        let mut agent_map: HashMap<PublicKey, AgentAccum> = HashMap::new();

        for event in latest_by_key.into_values() {
            match CapabilityCard::from_json(&event.content) {
                Ok(card) => {
                    let mut event_tags = Vec::new();
                    let mut supported_kinds: Vec<u16> = Vec::new();
                    for tag in event.tags.iter() {
                        let s = tag.as_slice();
                        match s.first().map(|v| v.as_str()) {
                            Some("t") => {
                                if let Some(v) = s.get(1) {
                                    event_tags.push(v.to_string());
                                }
                            }
                            Some("k") => {
                                if let Some(v) = s.get(1).and_then(|v| v.parse().ok()) {
                                    supported_kinds.push(v);
                                }
                            }
                            _ => {}
                        }
                    }

                    let created_at = event.created_at;
                    let entry = CardEntry {
                        card,
                        created_at,
                        tags: event_tags,
                        kinds: supported_kinds,
                    };

                    match agent_map.entry(event.pubkey) {
                        Entry::Occupied(mut e) => {
                            let acc = e.get_mut();
                            // Deduplicate by card name — keep the newer version
                            if let Some(pos) = acc.entries.iter().position(|e| e.card.name == entry.card.name) {
                                if created_at > acc.entries[pos].created_at {
                                    acc.entries[pos] = entry;
                                }
                            } else {
                                acc.entries.push(entry);
                            }
                            if created_at > acc.latest_created_at {
                                acc.event_id = event.id;
                                acc.latest_created_at = created_at;
                            }
                        }
                        Entry::Vacant(e) => {
                            e.insert(AgentAccum {
                                entries: vec![entry],
                                event_id: event.id,
                                latest_created_at: created_at,
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        event_id = %event.id,
                        error = %e,
                        "Failed to parse capability card, skipping"
                    );
                }
            }
        }

        // Recompute all_tags and supported_kinds from surviving card entries,
        // then apply filters per-agent.
        let mut agents: Vec<DiscoveredAgent> = Vec::new();
        for (pubkey, mut acc) in agent_map {
            // Post-filter: free-text query against any of the agent's cards.
            if let Some(ref query) = filter.query {
                if !acc.entries.iter().any(|e| matches_query(query, &e.card)) {
                    continue;
                }
            }

            // Collect tags and kinds only from the final (deduplicated) entries.
            let all_tags: HashSet<String> = acc.entries.iter().flat_map(|e| e.tags.iter().cloned()).collect();
            let mut supported_kinds: Vec<u16> = Vec::new();
            for e in &acc.entries {
                for &k in &e.kinds {
                    if !supported_kinds.contains(&k) {
                        supported_kinds.push(k);
                    }
                }
            }

            let match_count = if filter.capabilities.is_empty() {
                0
            } else {
                let tag_refs: HashSet<&str> = all_tags.iter().map(|s| s.as_str()).collect();
                let count = filter
                    .capabilities
                    .iter()
                    .filter(|cap| matches_capability(cap, &tag_refs))
                    .count();
                if count == 0 {
                    continue;
                }
                count
            };

            // Sort cards by created_at descending (newest first) for deterministic order.
            acc.entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            let cards = acc.entries.into_iter().map(|e| e.card).collect();

            agents.push(DiscoveredAgent {
                pubkey,
                cards,
                event_id: acc.event_id,
                supported_kinds,
                match_count,
            });
        }

        if !filter.capabilities.is_empty() {
            agents.sort_by(|a, b| b.match_count.cmp(&a.match_count));
        }

        if let Some(limit) = filter.limit {
            agents.truncate(limit);
        }

        Ok(agents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── to_d_tag ──

    #[test]
    fn test_d_tag_basic() {
        assert_eq!(to_d_tag("My Agent"), "my-agent");
    }

    #[test]
    fn test_d_tag_multiple_spaces() {
        assert_eq!(to_d_tag("Stock  Price   Analyzer"), "stock-price-analyzer");
    }

    #[test]
    fn test_d_tag_already_lowercase() {
        assert_eq!(to_d_tag("summarizer"), "summarizer");
    }

    #[test]
    fn test_d_tag_leading_trailing_spaces() {
        assert_eq!(to_d_tag("  hello world  "), "hello-world");
    }

    #[test]
    fn test_d_tag_empty() {
        assert_eq!(to_d_tag(""), "");
    }

    // ── matches_capability ──

    #[test]
    fn test_exact_match() {
        let tags: HashSet<&str> = ["translation", "elisym"].into();
        assert!(matches_capability("translation", &tags));
    }

    #[test]
    fn test_no_match() {
        let tags: HashSet<&str> = ["summarization"].into();
        assert!(!matches_capability("translation", &tags));
    }

    #[test]
    fn test_fuzzy_prefix_query_shorter() {
        // "stock" is a prefix of "stocks" (len >= 3)
        let tags: HashSet<&str> = ["stocks"].into();
        assert!(matches_capability("stock", &tags));
    }

    #[test]
    fn test_fuzzy_prefix_tag_shorter() {
        // "summarization" starts with "summar" (tag token len >= 3)
        let tags: HashSet<&str> = ["summar"].into();
        assert!(matches_capability("summarization", &tags));
    }

    #[test]
    fn test_compound_tag_token_split() {
        // "text-summarization" splits into ["text", "summarization"]
        // Both must match some tag token
        let tags: HashSet<&str> = ["text", "summarization"].into();
        assert!(matches_capability("text-summarization", &tags));
    }

    #[test]
    fn test_case_insensitive() {
        let tags: HashSet<&str> = ["translation"].into();
        assert!(matches_capability("Translation", &tags));
    }

    #[test]
    fn test_short_token_no_fuzzy() {
        // "ai" is < 3 chars, so it won't fuzzy-prefix-match "aim"
        let tags: HashSet<&str> = ["aim"].into();
        assert!(!matches_capability("ai", &tags));
    }

    #[test]
    fn test_compound_all_tokens_must_match() {
        // "stock-analysis" requires both "stock" and "analysis"
        let tags: HashSet<&str> = ["stocks"].into();
        assert!(!matches_capability("stock-analysis", &tags));
    }

    #[test]
    fn test_empty_tags_no_match() {
        let tags: HashSet<&str> = HashSet::new();
        assert!(!matches_capability("translation", &tags));
    }

    // ── matches_query ──

    #[test]
    fn test_query_matches_name() {
        let card = CapabilityCard::new("Stock Analyzer", "Analyzes things", vec![]);
        assert!(matches_query("stock", &card));
    }

    #[test]
    fn test_query_matches_description() {
        let card = CapabilityCard::new("Agent", "Translates text between languages", vec![]);
        assert!(matches_query("translates", &card));
    }

    #[test]
    fn test_query_matches_capability() {
        let card = CapabilityCard::new("Agent", "Does stuff", vec!["summarization".into()]);
        assert!(matches_query("summar", &card));
    }

    #[test]
    fn test_query_no_match() {
        let card = CapabilityCard::new("Agent", "Does stuff", vec!["coding".into()]);
        assert!(!matches_query("translation", &card));
    }
}
