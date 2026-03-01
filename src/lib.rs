//! # elisym-core
//!
//! Rust SDK for AI agents to discover each other via [Nostr](https://github.com/nostr-protocol/nips)
//! and pay for task execution via the [Lightning Network](https://lightning.network/).
//!
//! ## Overview
//!
//! `elisym-core` provides everything an AI agent needs to participate in a
//! decentralized marketplace:
//!
//! - **Discovery** — publish and search agent capabilities using NIP-89 (kind:31990)
//! - **Marketplace** — submit and receive jobs using NIP-90 Data Vending Machines
//! - **Messaging** — encrypted private messages via NIP-17 (NIP-44 + NIP-59 gift wrap)
//! - **Payments** — self-custodial Lightning via LDK-node: BOLT11 invoices, on-chain,
//!   and channel management (feature-gated behind `payments-ldk`)
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use elisym_core::*;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let agent = AgentNodeBuilder::new("my-agent", "Summarizes text")
//!         .capabilities(vec!["summarization".into()])
//!         .build().await?;
//!
//!     // Publish capabilities, subscribe to jobs, send results — see examples/
//!     agent.discovery.search_agents(&AgentFilter::default()).await?;
//!     Ok(())
//! }
//! ```

pub mod error;
pub mod types;
pub mod identity;
pub mod discovery;
pub mod messaging;
pub mod marketplace;
pub(crate) mod dedup;

#[cfg(feature = "payments-ldk")]
pub mod payments;

pub use error::{ElisymError, Result};
pub use types::*;
pub use identity::{AgentIdentity, CapabilityCard};
pub use discovery::{DiscoveryService, DiscoveredAgent, AgentFilter};
pub use messaging::MessagingService;
pub use marketplace::MarketplaceService;

#[cfg(feature = "payments-ldk")]
pub use payments::{PaymentService, PaymentConfig, ChannelInfo};

use nostr_sdk::Client;

/// Main orchestrator that ties all services together.
pub struct AgentNode {
    pub identity: AgentIdentity,
    pub client: Client,
    pub discovery: DiscoveryService,
    pub messaging: MessagingService,
    pub marketplace: MarketplaceService,
    #[cfg(feature = "payments-ldk")]
    pub payments: Option<PaymentService>,
    pub capability_card: CapabilityCard,
}

impl AgentNode {
    /// Gracefully shut down the agent: disconnect from relays and stop the
    /// payment node (if running). Disconnecting the client causes all spawned
    /// subscription tasks to terminate.
    ///
    /// **Note:** Callers should drain any active `mpsc::Receiver` channels
    /// before calling shutdown to process remaining buffered events.
    pub async fn shutdown(&mut self) {
        let _ = self.client.disconnect().await;

        #[cfg(feature = "payments-ldk")]
        if let Some(ref mut payments) = self.payments {
            payments.stop();
        }

        tracing::info!("AgentNode shut down");
    }

    /// Process a job with payment enforcement: generate invoice, send
    /// payment-required feedback, wait for payment, only then deliver result.
    ///
    /// This is the **recommended** way for providers to deliver paid results.
    /// Calling `submit_job_result()` directly skips payment verification.
    #[cfg(feature = "payments-ldk")]
    pub async fn process_job_with_payment(
        &self,
        job: &marketplace::JobRequest,
        result_content: &str,
        amount_msat: u64,
        invoice_description: &str,
        invoice_expiry_secs: u32,
        payment_timeout: std::time::Duration,
    ) -> Result<nostr_sdk::EventId> {
        let payments = self
            .payments
            .as_ref()
            .ok_or_else(|| ElisymError::Payment("Payments not configured".into()))?;

        // 1. Generate BOLT11 invoice
        let invoice = payments.make_invoice(
            amount_msat,
            invoice_description,
            invoice_expiry_secs,
        )?;

        tracing::info!(amount_msat, "Generated invoice for job payment");

        // 2. Send payment-required feedback with invoice
        self.marketplace
            .submit_job_feedback(
                &job.raw_event,
                JobStatus::PaymentRequired,
                None,
                Some(amount_msat),
                Some(&invoice),
            )
            .await?;

        // 3. Poll for payment until confirmed or timeout
        let deadline = tokio::time::Instant::now() + payment_timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                // Send error feedback so customer knows
                let _ = self
                    .marketplace
                    .submit_job_feedback(
                        &job.raw_event,
                        JobStatus::Error,
                        Some("payment-timeout"),
                        None,
                        None,
                    )
                    .await;
                return Err(ElisymError::Payment("Payment timeout — result not delivered".into()));
            }

            match payments.lookup_invoice(&invoice) {
                Ok(status) if status.settled => {
                    tracing::info!(amount_msat, "Payment confirmed");
                    break;
                }
                _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }

        // 4. Payment confirmed — deliver result with retries.
        //    This is critical: payment was received, so we must make best effort
        //    to deliver. If all retries fail, the caller should persist the result
        //    and retry later (use `is_invoice_paid()` on restart to check).
        let mut last_err = None;
        for attempt in 0..3u32 {
            match self
                .marketplace
                .submit_job_result(&job.raw_event, result_content, Some(amount_msat))
                .await
            {
                Ok(event_id) => return Ok(event_id),
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "Failed to deliver result after payment, retrying");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            ElisymError::Payment("Failed to deliver result after payment".into())
        }))
    }
}

/// Builder for constructing an AgentNode with all services configured.
pub struct AgentNodeBuilder {
    name: String,
    description: String,
    capabilities: Vec<String>,
    relays: Vec<String>,
    supported_job_kinds: Vec<u16>,
    secret_key: Option<String>,
    #[cfg(feature = "payments-ldk")]
    payment_config: Option<PaymentConfig>,
}

impl AgentNodeBuilder {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            capabilities: Vec::new(),
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            supported_job_kinds: vec![5100],
            secret_key: None,
            #[cfg(feature = "payments-ldk")]
            payment_config: None,
        }
    }

    pub fn capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn relays(mut self, relays: Vec<String>) -> Self {
        self.relays = relays;
        self
    }

    pub fn supported_job_kinds(mut self, kinds: Vec<u16>) -> Self {
        self.supported_job_kinds = kinds;
        self
    }

    pub fn secret_key(mut self, secret_key: impl Into<String>) -> Self {
        self.secret_key = Some(secret_key.into());
        self
    }

    #[cfg(feature = "payments-ldk")]
    pub fn payment_config(mut self, config: PaymentConfig) -> Self {
        self.payment_config = Some(config);
        self
    }

    pub async fn build(self) -> Result<AgentNode> {
        // Ensure rustls has a crypto provider for TLS (wss:// relay connections).
        // LDK-node pulls in rustls 0.23 which requires an explicit provider.
        #[cfg(feature = "payments-ldk")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let identity = match self.secret_key {
            Some(key) => AgentIdentity::from_secret_key(&key)?,
            None => AgentIdentity::generate(),
        };

        // Start LDK payments if configured
        #[cfg(feature = "payments-ldk")]
        let payments = if let Some(config) = self.payment_config {
            let mut svc = PaymentService::new(config);
            svc.start().await?;
            Some(svc)
        } else {
            None
        };

        // Create capability card
        if self.capabilities.is_empty() {
            tracing::warn!(
                "No capabilities set — this agent will not be discoverable via search_agents()"
            );
        }
        let card = CapabilityCard::new(
            &self.name,
            &self.description,
            self.capabilities.clone(),
        );

        // Create nostr client and connect to relays
        let client = Client::builder().signer(identity.keys().clone()).build();
        for relay in &self.relays {
            client.add_relay(relay.as_str()).await.map_err(|e| {
                ElisymError::Config(format!("Failed to add relay {}: {}", relay, e))
            })?;
        }
        client.connect().await;

        // Wait briefly for relay connections to establish (connect is non-blocking)
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let connected = client.relays().await;
        let connected_count = connected
            .values()
            .filter(|r| r.status() == nostr_sdk::RelayStatus::Connected)
            .count();
        if connected_count == 0 {
            return Err(ElisymError::Config(
                "No relays connected — cannot operate without at least one relay".into(),
            ));
        } else {
            tracing::info!(
                connected = connected_count,
                total = self.relays.len(),
                "Connected to relays"
            );
        }

        // Create services
        let discovery = DiscoveryService::new(client.clone(), identity.clone());
        let messaging = MessagingService::new(client.clone(), identity.clone());
        let marketplace = MarketplaceService::new(client.clone(), identity.clone());

        // Publish capability card
        discovery
            .publish_capability(&card, &self.supported_job_kinds)
            .await?;

        tracing::info!(
            name = %self.name,
            pubkey = %identity.npub(),
            "AgentNode started"
        );

        Ok(AgentNode {
            identity,
            client,
            discovery,
            messaging,
            marketplace,
            #[cfg(feature = "payments-ldk")]
            payments,
            capability_card: card,
        })
    }
}
