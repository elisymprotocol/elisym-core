use elisym_core::*;

fn test_payment() -> PaymentInfo {
    PaymentInfo {
        chain: "solana".into(),
        network: "devnet".into(),
        address: "So11111111111111111111111111111111111111112".into(),
        job_price: None,
    }
}

#[test]
fn test_identity_generate_and_restore() {
    let id = AgentIdentity::generate();
    let hex = id.keys().secret_key().to_secret_hex();
    let restored = AgentIdentity::from_secret_key(&hex).unwrap();
    assert_eq!(id.public_key(), restored.public_key());
}

#[test]
fn test_capability_card_roundtrip() {
    let card = CapabilityCard::new(
        "test-agent",
        "A test agent for integration testing",
        vec!["capability-a".into(), "capability-b".into()],
        test_payment(),
    );

    let json = card.to_json().unwrap();
    let restored = CapabilityCard::from_json(&json).unwrap();

    assert_eq!(restored.name, "test-agent");
    assert_eq!(restored.description, "A test agent for integration testing");
    assert_eq!(restored.capabilities.len(), 2);
    assert_eq!(restored.payment.address, "So11111111111111111111111111111111111111112");
    assert_eq!(restored.payment.chain, "solana");
}

#[test]
fn test_capability_card_missing_payment_fails() {
    let json = r#"{"name":"minimal","description":"Minimal agent","capabilities":[]}"#;
    assert!(CapabilityCard::from_json(json).is_err());
}

#[test]
fn test_job_status_display() {
    assert_eq!(JobStatus::PaymentRequired.as_str(), "payment-required");
    assert_eq!(JobStatus::Processing.as_str(), "processing");
    assert_eq!(JobStatus::Error.as_str(), "error");
    assert_eq!(JobStatus::Success.as_str(), "success");
    assert_eq!(JobStatus::Partial.as_str(), "partial");
    assert_eq!(JobStatus::PaymentCompleted.as_str(), "payment-completed");
}

#[test]
fn test_job_status_serde() {
    let status = JobStatus::PaymentRequired;
    let json = serde_json::to_string(&status).unwrap();
    assert_eq!(json, "\"payment-required\"");

    let restored: JobStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, JobStatus::PaymentRequired);
}

#[test]
fn test_agent_filter_default() {
    let filter = AgentFilter::default();
    assert!(filter.capabilities.is_empty());
    assert!(filter.job_kind.is_none());
    assert!(filter.since.is_none());
}

#[test]
fn test_kind_constants() {
    assert_eq!(KIND_APP_HANDLER, 31990);
    assert_eq!(KIND_JOB_REQUEST_BASE, 5000);
    assert_eq!(KIND_JOB_RESULT_BASE, 6000);
    assert_eq!(KIND_JOB_FEEDBACK, 7000);
}

#[test]
fn test_capability_card_with_payment() {
    let card = CapabilityCard::new("sol-agent", "Agent with payment", vec![], PaymentInfo {
        chain: "solana".into(),
        network: "devnet".into(),
        address: "So11111111111111111111111111111111111111113".into(),
        job_price: Some(1000),
    });

    let json = card.to_json().unwrap();
    assert!(json.contains("payment"));

    let restored = CapabilityCard::from_json(&json).unwrap();
    assert_eq!(restored.payment.address, "So11111111111111111111111111111111111111113");
    assert_eq!(restored.payment.chain, "solana");
    assert_eq!(restored.payment.job_price, Some(1000));
}
