use std::path::Path;

use nx_core::{Deployments, Result};

// Recorded on Neptuno 45.20261118 with one rollback deployment, trimmed to
// the fields nx reads.
const NEPTUNO: &str = include_str!("fixtures/rpm-ostree-status.json");

#[test]
fn finds_booted_and_rollback() -> Result<()> {
    let deployments = Deployments::from_json(NEPTUNO)?;
    assert_eq!(deployments.all().len(), 2);

    let booted = deployments
        .booted()
        .ok_or(nx_core::Error::Invalid("no booted".into()))?;
    assert_eq!(booted.version.as_deref(), Some("45.20261118"));
    assert_eq!(
        booted.image_digest.as_deref(),
        Some("sha256:92409d3fcf8435bc77e7339def63550887b5cdfde2302af8a0589c0305e7d2b8")
    );
    assert_eq!(booted.image(), Some("ghcr.io/siddhj2206/neptuno:latest"));

    assert!(deployments.staged().is_none());
    let rollback = deployments
        .rollback()
        .ok_or(nx_core::Error::Invalid("no rollback".into()))?;
    assert_eq!(rollback.version.as_deref(), Some("45.20261117"));
    Ok(())
}

/// The fixture with `field` set to `value` on deployment `index`.
fn with(index: usize, field: &str, value: serde_json::Value) -> String {
    let mut json: serde_json::Value = serde_json::from_str(NEPTUNO).unwrap_or_default();
    json["deployments"][index][field] = value;
    json.to_string()
}

#[test]
fn staged_is_not_rollback() -> Result<()> {
    let deployments = Deployments::from_json(&with(1, "staged", true.into()))?;
    assert_eq!(
        deployments.staged().and_then(|d| d.version.as_deref()),
        Some("45.20261117")
    );
    assert!(deployments.rollback().is_none());
    Ok(())
}

#[test]
fn deployment_root_follows_ostree_layout() -> Result<()> {
    let deployments = Deployments::from_json(NEPTUNO)?;
    let rollback = deployments
        .rollback()
        .ok_or(nx_core::Error::Invalid("no rollback".into()))?;
    assert_eq!(
        rollback.root(Path::new("/sysroot")),
        Path::new(
            "/sysroot/ostree/deploy/default/deploy/30e1f7c3f36696120b98a48861728ab6840206364bc44310bfce3ca165e6bb30.0"
        )
    );
    Ok(())
}

#[test]
fn rejects_status_without_a_booted_deployment() {
    assert!(Deployments::from_json(&with(0, "booted", false.into())).is_err());
}

#[test]
fn image_without_transport_prefix_is_kept() -> Result<()> {
    let json = NEPTUNO.replace("ostree-unverified-registry:", "");
    let deployments = Deployments::from_json(&json)?;
    assert_eq!(
        deployments.booted().and_then(|d| d.image()),
        Some("ghcr.io/siddhj2206/neptuno:latest")
    );
    Ok(())
}
