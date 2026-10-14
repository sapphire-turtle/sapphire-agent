//! The `external-device` subcommand.
//!
//! External devices — the pendant, an ACP client, anything that reaches the
//! agent over HTTP with a bearer token — live in the workgroup's synced
//! ledger, which the sapphire bridge owns. This is the framework's own
//! `external-device` command, sent to the running bridge; `add` lets the
//! device use this agent unless `--app` says otherwise. The token is printed
//! once, by `add` and `rotate`, and is never stored anywhere but on the
//! device.
//!
//! The ledger says who a device is; which room profile it runs under is
//! `[room_profile.<name>].devices` in config.toml, by name or id.

use anyhow::{Context as _, Result};
use sapphire_framework_bridge_api::{
    ExternalDeviceCommand, ExternalDeviceOutcome, describe_external_outcome,
};
use sapphire_framework_bridge_client::BridgeClient;

use crate::device_auth::APP_NAME;

pub async fn run(command: ExternalDeviceCommand) -> Result<()> {
    let client = BridgeClient::connect_running("cli", env!("CARGO_PKG_VERSION"))
        .await
        .context("external devices are managed by the sapphire bridge; start it first")?;
    let added = matches!(command, ExternalDeviceCommand::Add { .. });
    let outcome = client
        .external_device_request(command.request(Some(APP_NAME)))
        .await
        .context("the sapphire bridge refused the request")?;
    for line in describe_external_outcome(&outcome) {
        println!("{line}");
    }
    if added && let ExternalDeviceOutcome::WithToken(r) = &outcome {
        eprintln!(
            "list {:?} (or its id {}) in a [room_profile.<name>].devices array to give it an \
             LLM profile and memory namespace; until then the agent refuses its token",
            r.external_device.name, r.external_device.id
        );
    }
    Ok(())
}
