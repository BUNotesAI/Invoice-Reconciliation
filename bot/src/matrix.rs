//! Matrix side of the bot: a persisted device session, inbound messages to the service, and the outbox sender.
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use matrix_sdk::ruma::api::client::{filter::FilterDefinition, sync::sync_events::v3::Filter};
use matrix_sdk::{
    Client, Room,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    media::{MediaFormat, MediaRequestParameters},
    ruma::{
        OwnedRoomId, OwnedTransactionId, UInt,
        events::room::{
            member::StrippedRoomMemberEvent,
            message::{MessageType, OriginalSyncRoomMessageEvent},
        },
    },
    store::RoomLoadSettings,
};

use crate::service::Service;

pub const MAX_UPLOAD: u64 = 20 * 1024 * 1024;

/// Restores the bot's device session if one was saved, otherwise logs in once and saves it (mode 0600).
/// Reusing the session keeps one device instead of adding a new one on every start (P0 finding F2).
pub async fn connect(
    homeserver: &str,
    user: &str,
    password: &str,
    session_file: &Path,
) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(homeserver)
        .build()
        .await
        .context("Matrix client initialization failed")?;
    if let Ok(bytes) = std::fs::read(session_file) {
        let session: MatrixSession =
            serde_json::from_slice(&bytes).context("invalid saved Matrix session")?;
        client
            .matrix_auth()
            .restore_session(session, RoomLoadSettings::default())
            .await
            .context("restoring the Matrix session failed")?;
        return Ok(client);
    }
    client
        .matrix_auth()
        .login_username(user, password)
        .initial_device_display_name("Reimbursement bot")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Matrix login failed"))?;
    let session = client
        .matrix_auth()
        .session()
        .context("no session after login")?;
    let body = serde_json::to_vec(&session)?;
    write_private(session_file, &body)?;
    Ok(client)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

/// Routes room messages to the service. The homeserver-authenticated sender is the only identity used.
pub fn register_handlers(client: &Client, service: Arc<Service>) {
    let inbound = service.clone();
    client.add_event_handler(
        move |event: OriginalSyncRoomMessageEvent, room: Room, client: Client| {
            let service = inbound.clone();
            async move {
                if client.user_id() == Some(&event.sender) {
                    return;
                }
                let sender = event.sender.to_string();
                let room_id = room.room_id().to_string();
                let event_id = event.event_id.to_string();
                let result = match event.content.msgtype {
                    MessageType::Text(text) => {
                        service
                            .receive_text(&sender, &room_id, &event_id, &text.body)
                            .await
                    }
                    MessageType::File(file) => {
                        let name = file.filename.clone().unwrap_or_else(|| file.body.clone());
                        let size = file.info.as_ref().and_then(|info| info.size);
                        download(
                            &client,
                            &service,
                            file.source,
                            size,
                            &sender,
                            &room_id,
                            &event_id,
                            &name,
                        )
                        .await
                    }
                    MessageType::Image(image) => {
                        let name = image.filename.clone().unwrap_or_else(|| image.body.clone());
                        let size = image.info.as_ref().and_then(|info| info.size);
                        download(
                            &client,
                            &service,
                            image.source,
                            size,
                            &sender,
                            &room_id,
                            &event_id,
                            &name,
                        )
                        .await
                    }
                    _ => Ok(()),
                };
                if let Err(error) = result {
                    eprintln!("message handling failed: {error}");
                }
            }
        },
    );
    // Join direct invitations only from people the policy or configuration knows.
    let invites = service.clone();
    client.add_event_handler(
        move |event: StrippedRoomMemberEvent, room: Room, client: Client| {
            let service = invites.clone();
            async move {
                if client.user_id().map(|id| id.as_str()) != Some(event.state_key.as_str()) {
                    return;
                }
                if service
                    .config
                    .applicants
                    .contains_key(event.sender.as_str())
                {
                    let _ = room.join().await;
                }
            }
        },
    );
}

#[allow(clippy::too_many_arguments)]
async fn download(
    client: &Client,
    service: &Service,
    source: matrix_sdk::ruma::events::room::MediaSource,
    size: Option<UInt>,
    sender: &str,
    room: &str,
    event_id: &str,
    name: &str,
) -> Result<(), crate::service::ServiceError> {
    // Declared sizes are checked before downloading; the core checks the bytes again.
    if size.is_some_and(|size| u64::from(size) > MAX_UPLOAD) {
        return service
            .refuse_upload(sender, room, event_id, name, "文件超过 20 MB")
            .await;
    }
    let request = MediaRequestParameters {
        source,
        format: MediaFormat::File,
    };
    match client.media().get_media_content(&request, false).await {
        Ok(bytes) if bytes.len() as u64 <= MAX_UPLOAD => {
            service
                .receive_file(sender, room, event_id, name, &bytes)
                .await
        }
        Ok(_) => {
            service
                .refuse_upload(sender, room, event_id, name, "文件超过 20 MB")
                .await
        }
        Err(_) => {
            service
                .refuse_upload(sender, room, event_id, name, "文件下载失败，请重发")
                .await
        }
    }
}

/// Sends queued messages with their stored transaction ids; the homeserver drops a repeated transaction.
pub async fn send_outbox(client: &Client, service: &Service) -> usize {
    let pending = match service.with_store(|store| store.pending_outbox()) {
        Ok(pending) => pending,
        Err(_) => return 0,
    };
    let mut sent = 0;
    for message in pending {
        let Ok(room_id) = OwnedRoomId::try_from(message.room_id.as_str()) else {
            continue;
        };
        let Some(room) = client.get_room(&room_id) else {
            continue;
        };
        let txn = OwnedTransactionId::from(message.txn_id.as_str());
        match room
            .send_raw("m.room.message", message.content.clone())
            .with_transaction_id(&txn)
            .await
        {
            Ok(response) => {
                if service
                    .with_store(|store| {
                        store.mark_sent(message.id, response.response.event_id.as_str())
                    })
                    .is_ok()
                {
                    sent += 1;
                }
            }
            Err(error) => eprintln!("outbox send failed, will retry: {error}"),
        }
    }
    sent
}

/// Sync loop with a persisted token, so messages sent while the bot was down are handled after a restart;
/// inbound de-duplication makes the replay harmless. The first start skips older history.
pub async fn run_sync(client: Client, service: Arc<Service>) -> Result<()> {
    let token_key = "matrix-sync-token";
    let mut token = service
        .with_store(|store| store.setting(token_key))
        .ok()
        .flatten();
    // A full sync before any handler exists: it loads the joined rooms (a restored session starts with none, and an
    // incremental sync only mentions rooms with new activity, so queued replies to quiet rooms would stay unsent).
    let warm_up = client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(1)))
        .await?;
    if token.is_none() {
        // First start: history before now is not replayed.
        token = Some(warm_up.next_batch.clone());
    }
    register_handlers(&client, service.clone());
    // A burst of uploads must not be cut to the server's default timeline window (often 10 events per room).
    let mut definition = FilterDefinition::default();
    definition.room.timeline.limit = Some(UInt::from(250u32));
    loop {
        let mut settings = SyncSettings::default()
            .timeout(Duration::from_secs(10))
            .filter(Filter::FilterDefinition(definition.clone()));
        if let Some(token) = &token {
            settings = settings.token(token.clone());
        }
        match client.sync_once(settings).await {
            Ok(response) => {
                token = Some(response.next_batch.clone());
                let _ = service.with_store(|store| {
                    store.begin().and_then(|work| {
                        work.put_setting(token_key, &response.next_batch)?;
                        work.commit()
                    })
                });
            }
            Err(error) => {
                eprintln!("sync failed, retrying: {error}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Outbox sender and month-end reminder, one tick per second.
pub async fn run_outbox(client: Client, service: Arc<Service>) -> Result<()> {
    loop {
        let _ = service.remind();
        send_outbox(&client, &service).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
