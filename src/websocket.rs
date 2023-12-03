use std::collections::HashSet;
use std::env;

use anyhow::{bail, Context};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite;
use tracing::Instrument;
use twitch_api::TWITCH_EVENTSUB_WEBSOCKET_URL;
use twitch_api::eventsub::event::websocket::{
    EventsubWebsocketData, ReconnectPayload, SessionData, WelcomePayload,
};
use twitch_api::eventsub::stream::StreamOnlineV1Payload;
use twitch_api::eventsub::{self, Event, Message, Transport};
use twitch_api::twitch_oauth2::url::Url;
use twitch_api::twitch_oauth2::{AccessToken, UserToken};
use twitch_api::types::UserId;

use super::{Result, TwitchClient};

pub struct WebsocketClient {
    /// The session id of the websocket connection
    session_id: Option<String>,
    /// The token used to authenticate with the Twitch API
    user_token: UserToken,
    /// The client used to make requests to the Twitch API
    client: TwitchClient,
    /// The user ids of the channels we want to listen to
    subscriptions: Mutex<HashSet<UserId>>,
    /// The url to use for websocket
    connect_url: Url,
    new_subscriptions: UnboundedReceiver<UserId>,
    live_notifications: UnboundedSender<StreamOnlineV1Payload>,
    transport: Option<Transport>,
}

impl WebsocketClient {
    /// Connect to the websocket and return the stream
    pub async fn connect(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        anyhow::Error,
    > {
        tracing::info!("connecting to twitch");
        let config = tungstenite::protocol::WebSocketConfig {
            // write_buffer_size: 2048,
            // max_write_buffer_size: 2048,
            // max_message_size: Some(64 << 20), // 64 MiB
            // max_frame_size: Some(16 << 20),   // 16 MiB
            // accept_unmasked_frames: false,
            ..tungstenite::protocol::WebSocketConfig::default()
        };
        let (socket, _) =
            tokio_tungstenite::connect_async_with_config(&self.connect_url, Some(config), false)
                .await
                .context("Can't connect")?;

        Ok(socket)
    }

    /// Run the websocket subscriber
    #[tracing::instrument(name = "subscriber", skip_all, fields())]
    pub async fn run(mut self) -> Result<(), anyhow::Error> {
        // Establish the stream
        let mut s = self
            .connect()
            .await
            .context("when establishing connection")?;
        // Loop over the stream, processing messages as they come in.
        loop {
            tokio::select!(
            Some(msg) = futures::StreamExt::next(&mut s) => {
                let span = tracing::info_span!("message received", raw_message = ?msg);
                let msg = match msg {
                    Err(tungstenite::Error::Protocol(
                        tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
                    )) => {
                        tracing::warn!(
                            "connection was sent an unexpected frame or was reset, reestablishing it"
                        );
                        s = self
                            .connect().instrument(span)
                            .await
                            .context("when reestablishing connection")?;
                        continue
                    }
                    _ => msg.context("when getting message")?,
                };
                self.process_message(msg).instrument(span).await?;
            },
            Some(subscription) = self.new_subscriptions.next() => {
                let mut subscriptions = self.subscriptions.lock().await;
                if subscriptions.insert(subscription.clone()) {
                    self.add_subscription(&subscription).await?;
                }
            }
            );
        }
    }

    /// Process a message from the websocket
    pub async fn process_message(&mut self, msg: tungstenite::Message) -> Result {
        match msg {
            tungstenite::Message::Text(s) => {
                tracing::info!("{s}");
                // Parse the message into a [twitch_api::eventsub::EventsubWebsocketData]
                match Event::parse_websocket(&s)? {
                    EventsubWebsocketData::Welcome {
                        payload: WelcomePayload { session },
                        ..
                    }
                    | EventsubWebsocketData::Reconnect {
                        payload: ReconnectPayload { session },
                        ..
                    } => {
                        self.process_welcome_message(session).await?;
                        Ok(())
                    }
                    // Here is where you would handle the events you want to listen to
                    EventsubWebsocketData::Notification {
                        metadata: _,
                        payload,
                    } => {
                        match payload {
                            Event::StreamOnlineV1(eventsub::Payload {
                                message: Message::Notification(subscription),
                                ..
                            }) => {
                                tracing::info!(
                                    "got online event for {}",
                                    subscription.broadcaster_user_name
                                );
                                self.live_notifications.unbounded_send(subscription)?;
                            }
                            payload => {
                                tracing::error!(?payload, "unexpected message");
                            }
                        }
                        Ok(())
                    }
                    EventsubWebsocketData::Revocation {
                        metadata,
                        payload: _,
                    } => bail!("got revocation event: {metadata:?}"),
                    _ => Ok(()),
                }
            }
            tungstenite::Message::Close(_) => bail!("websocket closed"),
            _ => Ok(()),
        }
    }

    pub async fn process_welcome_message(&mut self, data: SessionData<'_>) -> Result {
        self.session_id = Some(data.id.to_string());
        if let Some(url) = data.reconnect_url {
            self.connect_url = url.parse()?;
        }
        self.transport = Some(eventsub::Transport::websocket(data.id.clone()));
        let user_ids = self.subscriptions.lock().await;
        // TODO sequential sucks
        for user_id in user_ids.iter() {
            self.add_subscription(user_id).await?;
        }
        tracing::info!("listening to ban and unbans");
        Ok(())
    }

    async fn add_subscription(&self, user_id: &UserId) -> Result {
        self.client
            .helix
            .create_eventsub_subscription(
                eventsub::stream::StreamOnlineV1::broadcaster_user_id(user_id.clone()),
                self.transport
                    .clone()
                    .context("tried to add subcription before initialized")?,
                &self.user_token,
            )
            .await?;
        Ok(())
    }

    pub async fn new(
        twitch_client: &TwitchClient,
        new_subscriptions: UnboundedReceiver<UserId>,
        live_notifications: UnboundedSender<StreamOnlineV1Payload>,
        subscriptions: HashSet<UserId>,
    ) -> Result<Self> {
        let user_token = UserToken::from_token(
            twitch_client,
            AccessToken::new(env::var("TWITCH_ACCESS_TOKEN")?),
        )
        .await?;

        Ok(Self {
            session_id: None,
            user_token,
            client: twitch_client.clone(),
            new_subscriptions,
            live_notifications,
            subscriptions: Mutex::new(subscriptions),
            connect_url: TWITCH_EVENTSUB_WEBSOCKET_URL.clone(),
            transport: None,
        })
    }
}
