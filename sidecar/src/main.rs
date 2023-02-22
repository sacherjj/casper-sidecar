extern crate core;

mod event_stream_server;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod performance_tests;
mod rest_server;
mod sql;
mod sqlite_database;
#[cfg(test)]
pub(crate) mod testing;
mod types;
mod utils;

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{Context, Error};
use clap::Parser;
use futures::future::join_all;
use hex_fmt::HexFmt;
use tokio::{
    sync::mpsc::{channel as mpsc_channel, Receiver, Sender},
    task::JoinHandle,
};
use tracing::{debug, error, info, trace, warn};

use casper_event_listener::{EventListener, NodeConnectionInterface, SseEvent};
use casper_event_types::SseData;
use casper_types::ProtocolVersion;

use crate::{
    event_stream_server::{Config as SseConfig, EventStreamServer},
    rest_server::run_server as start_rest_server,
    sqlite_database::SqliteDatabase,
    types::{
        config::{read_config, Config},
        database::{DatabaseWriteError, DatabaseWriter},
        sse_events::*,
    },
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CmdLineArgs {
    /// Path to the TOML-formatted config file
    #[arg(short, long, value_name = "FILE")]
    path_to_config: String,
}

const DEFAULT_CHANNEL_SIZE: usize = 1000;

#[tokio::main]
async fn main() -> Result<(), Error> {
    // Install global collector for tracing
    tracing_subscriber::fmt::init();

    let args = CmdLineArgs::parse();

    let path_to_config = args.path_to_config;

    let config: Config = read_config(&path_to_config).context("Error constructing config")?;
    info!("Configuration loaded");

    run(config).await
}

async fn run(config: Config) -> Result<(), Error> {
    let mut event_listeners = Vec::with_capacity(config.connections.len());

    let mut sse_data_receivers = Vec::new();
    let (api_version_tx, mut api_version_rx) =
        mpsc_channel::<Result<ProtocolVersion, Error>>(config.connections.len() + 10);

    for connection in &config.connections {
        let (inbound_sse_data_sender, inbound_sse_data_receiver) =
            mpsc_channel(config.inbound_channel_size.unwrap_or(DEFAULT_CHANNEL_SIZE));

        sse_data_receivers.push(inbound_sse_data_receiver);

        let node_interface = NodeConnectionInterface {
            ip_address: IpAddr::from_str(&connection.ip_address)?,
            sse_port: connection.sse_port,
            rest_port: connection.rest_port,
        };

        let event_listener = EventListener::new(
            node_interface,
            connection.max_retries,
            Duration::from_secs(connection.delay_between_retries_in_seconds as u64),
            connection.allow_partial_connection,
            inbound_sse_data_sender,
            Duration::from_secs(connection.connection_timeout_in_seconds.unwrap_or(5) as u64),
        );
        event_listeners.push(event_listener);
    }

    let path_to_database_dir = Path::new(&config.storage.storage_path);

    // Creates and initialises Sqlite database
    let sqlite_database =
        SqliteDatabase::new(path_to_database_dir, config.storage.sqlite_config.clone())
            .await
            .context("Error instantiating database")?;

    // Prepare the REST server task - this will be executed later
    let rest_server_handle = tokio::spawn(start_rest_server(
        config.rest_server.clone(),
        sqlite_database.clone(),
    ));

    // This channel allows SseData to be sent from multiple connected nodes to the single EventStreamServer.
    let (outbound_sse_data_sender, mut outbound_sse_data_receiver) =
        mpsc_channel(config.outbound_channel_size.unwrap_or(DEFAULT_CHANNEL_SIZE));

    let connection_configs = config.connections.clone();

    // Task to manage incoming events from all three filters
    let listening_task_handle = tokio::spawn(async move {
        let mut join_handles = Vec::with_capacity(event_listeners.len());

        for ((event_listener, connection_config), sse_data_receiver) in event_listeners
            .into_iter()
            .zip(connection_configs)
            .zip(sse_data_receivers)
        {
            let join_handle = tokio::spawn(sse_processor(
                event_listener,
                api_version_tx.clone(),
                sse_data_receiver,
                outbound_sse_data_sender.clone(),
                sqlite_database.clone(),
                connection_config.enable_logging,
            ));

            join_handles.push(join_handle);
        }

        // The original sender needs to be dropped here as it's existence prevents the event broadcasting logic to proceed
        drop(api_version_tx);
        drop(outbound_sse_data_sender);

        let _ = join_all(join_handles).await;

        Err::<(), Error>(Error::msg("Connected node(s) are unavailable"))
    });

    let event_broadcasting_handle = tokio::spawn(async move {
        // Wait for the listeners to report the API version before spinning up the Event Stream Server.
        let mut api_versions = Vec::new();
        while let Some(api_fetch_res) = api_version_rx.recv().await {
            match api_fetch_res {
                Ok(version) => api_versions.push(version),
                Err(err) => {
                    error!("Error fetching API version from connected node(s): {err}");
                    return Err(err);
                }
            }
        }

        let api_versions_match = api_versions.windows(2).all(|window| window[0] == window[1]);

        if !api_versions_match {
            return Err(Error::msg("Couldn't start Event Stream Server due to inbound streams with mismatched API Versions"));
        } else if api_versions.is_empty() {
            return Err(Error::msg(
                "Couldn't start Event Stream Server - no inbound streams reported API version",
            ));
        }

        // Create new instance for the Sidecar's Event Stream Server
        let mut event_stream_server = EventStreamServer::new(
            SseConfig::new(
                config.event_stream_server.port,
                Some(config.event_stream_server.event_stream_buffer_length),
                Some(config.event_stream_server.max_concurrent_subscribers),
            ),
            PathBuf::from(&config.storage.storage_path),
            api_versions[0],
        )
        .context("Error starting EventStreamServer")?;

        while let Some(sse_data) = outbound_sse_data_receiver.recv().await {
            event_stream_server.broadcast(sse_data);
        }
        Err::<(), Error>(Error::msg("Event broadcasting finished"))
    });

    tokio::try_join!(
        flatten_handle(event_broadcasting_handle),
        flatten_handle(rest_server_handle),
        flatten_handle(listening_task_handle)
    )
    .map(|_| Ok(()))?
}

async fn flatten_handle<T>(handle: JoinHandle<Result<T, Error>>) -> Result<T, Error> {
    match handle.await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => Err(err),
        Err(join_err) => Err(Error::from(join_err)),
    }
}

/// Function to handle single event in the sse_processor.
/// Returns false if the handling indicated that no other messages should be processed.
/// Returns true otherwise.
async fn handle_single_event(
    sse_event: SseEvent,
    sqlite_database: SqliteDatabase,
    enable_event_logging: bool,
    outbound_sse_data_sender: Sender<SseData>,
) {
    match sse_event.data {
        SseData::ApiVersion(version) => {
            if enable_event_logging {
                info!(%version, "API Version");
            }
        }
        SseData::BlockAdded { block, block_hash } => {
            if enable_event_logging {
                let hex_block_hash = HexFmt(block_hash.inner());
                info!("Block Added: {:18}", hex_block_hash);
                debug!("Block Added: {}", hex_block_hash);
            }

            let res = sqlite_database
                .save_block_added(
                    BlockAdded::new(block_hash, block.clone()),
                    sse_event.id,
                    sse_event.source.to_string(),
                )
                .await;

            match res {
                Ok(_) => {
                    let _ =
                        outbound_sse_data_sender.send(SseData::BlockAdded { block, block_hash }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received BlockAdded ({}), logged in event_log",
                        HexFmt(block_hash.inner())
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving BlockAdded"),
            }
        }
        SseData::DeployAccepted { deploy } => {
            if enable_event_logging {
                let hex_deploy_hash = HexFmt(deploy.id().inner());
                info!("Deploy Accepted: {:18}", hex_deploy_hash);
                debug!("Deploy Accepted: {}", hex_deploy_hash);
            }
            let deploy_accepted = DeployAccepted::new(deploy.clone());
            let res = sqlite_database
                .save_deploy_accepted(deploy_accepted, sse_event.id, sse_event.source.to_string())
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::DeployAccepted { deploy }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received DeployAccepted ({}), logged in event_log",
                        HexFmt(deploy.id().inner())
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving DeployAccepted"),
            }
        }
        SseData::DeployExpired { deploy_hash } => {
            if enable_event_logging {
                let hex_deploy_hash = HexFmt(deploy_hash.inner());
                info!("Deploy Expired: {:18}", hex_deploy_hash);
                debug!("Deploy Expired: {}", hex_deploy_hash);
            }
            let res = sqlite_database
                .save_deploy_expired(
                    DeployExpired::new(deploy_hash),
                    sse_event.id,
                    sse_event.source.to_string(),
                )
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::DeployExpired { deploy_hash }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received DeployExpired ({}), logged in event_log",
                        HexFmt(deploy_hash.inner())
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving DeployExpired"),
            }
        }
        SseData::DeployProcessed {
            deploy_hash,
            account,
            timestamp,
            ttl,
            dependencies,
            block_hash,
            execution_result,
        } => {
            if enable_event_logging {
                let hex_deploy_hash = HexFmt(deploy_hash.inner());
                info!("Deploy Processed: {:18}", hex_deploy_hash);
                debug!("Deploy Processed: {}", hex_deploy_hash);
            }
            let deploy_processed = DeployProcessed::new(
                deploy_hash.clone(),
                account.clone(),
                timestamp,
                ttl,
                dependencies.clone(),
                block_hash.clone(),
                execution_result.clone(),
            );
            let res = sqlite_database
                .save_deploy_processed(
                    deploy_processed.clone(),
                    sse_event.id,
                    sse_event.source.to_string(),
                )
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::DeployProcessed {
                        deploy_hash,
                        account,
                        timestamp,
                        ttl,
                        dependencies,
                        block_hash,
                        execution_result,
                    }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received DeployProcessed ({}), logged in event_log",
                        HexFmt(deploy_hash.inner())
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving DeployProcessed"),
            }
        }
        SseData::Fault {
            era_id,
            timestamp,
            public_key,
        } => {
            let fault = Fault::new(era_id, public_key.clone(), timestamp);
            warn!(%fault, "Fault reported");
            let res = sqlite_database
                .save_fault(fault.clone(), sse_event.id, sse_event.source.to_string())
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::Fault {
                        era_id,
                        timestamp,
                        public_key,
                    }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!("Already received Fault ({:#?}), logged in event_log", fault);
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving Fault"),
            }
        }
        SseData::FinalitySignature(fs) => {
            if enable_event_logging {
                debug!("Finality Signature: {} for {}", fs.signature, fs.block_hash);
            }
            let finality_signature = FinalitySignature::new(fs.clone());
            let res = sqlite_database
                .save_finality_signature(
                    finality_signature.clone(),
                    sse_event.id,
                    sse_event.source.to_string(),
                )
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::FinalitySignature(fs)).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received FinalitySignature ({}), logged in event_log",
                        fs.signature
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => {
                    warn!(?other_err, "Unexpected error saving FinalitySignature")
                }
            }
        }
        SseData::Step {
            era_id,
            execution_effect,
        } => {
            let step = Step::new(era_id, execution_effect.clone());
            if enable_event_logging {
                info!("Step at era: {}", era_id.value());
            }
            let res = sqlite_database
                .save_step(step, sse_event.id, sse_event.source.to_string())
                .await;

            match res {
                Ok(_) => {
                    let _ = outbound_sse_data_sender.send(SseData::Step {
                        era_id,
                        execution_effect,
                    }).await;
                }
                Err(DatabaseWriteError::UniqueConstraint(uc_err)) => {
                    debug!(
                        "Already received Step ({}), logged in event_log",
                        era_id.value()
                    );
                    trace!(?uc_err);
                }
                Err(other_err) => warn!(?other_err, "Unexpected error saving Step"),
            }
        }
        SseData::Shutdown => {
            warn!("Node ({}) is unavailable", sse_event.source.to_string());
        }
    }
}

async fn sse_processor(
    mut sse_event_listener: EventListener,
    api_version_reporter: Sender<Result<ProtocolVersion, Error>>,
    mut inbound_sse_data_receiver: Receiver<SseEvent>,
    outbound_sse_data_sender: Sender<SseData>,
    sqlite_database: SqliteDatabase,
    enable_event_logging: bool,
) {
    // This task starts the listener pushing events to the sse_data_receiver
    tokio::spawn(async move {
        let _ = sse_event_listener
            .stream_aggregated_events(api_version_reporter)
            .await;
    });
    while let Some(sse_event) = inbound_sse_data_receiver.recv().await {
        handle_single_event(
            sse_event,
            sqlite_database.clone(),
            enable_event_logging,
            outbound_sse_data_sender.clone(),
        )
        .await
    }
}
