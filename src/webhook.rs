use std::collections::{HashMap, HashSet};
use std::env;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;

use anyhow::Context;
use axum::body::HttpBody;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{http, Router};
use bonsaidb::core::schema::SerializedView;
use bonsaidb::local::AsyncDatabase;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use rand::distributions::DistString;
use tokio::select;
use tokio::sync::RwLock;
use twitch_api::eventsub::stream::{StreamOnlineV1, StreamOnlineV1Payload};
use twitch_api::eventsub::{self as twitch_eventsub, Event, EventType, Message, Payload, Status};
use twitch_api::twitch_oauth2::{AppAccessToken, ClientId, ClientSecret, TwitchToken};

use super::{Result, TwitchClient, TwitchUserId};
use crate::Subscriptions;

pub async fn eventsub_register(
    token: Arc<RwLock<AppAccessToken>>,
    client: &TwitchClient,
    website: &str,
    // TODO reconsider this being a string :D
    sign_secret: &str,
    db: AsyncDatabase,
) -> Result {
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // check every day
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(24 * 60 * 60));

    loop {
        // first check if we are already registered
        interval.tick().await;
        tracing::info!("checking subs");
        let subs: HashMap<_, _> = client
            .helix
            .get_eventsub_subscriptions(Status::Enabled, None, None, &*token.read().await)
            .map_ok(|events| {
                futures::stream::iter(events.subscriptions.into_iter().map(anyhow::Ok))
            })
            .try_flatten()
            .try_filter_map(|sub| async move {
                if sub
                    .transport
                    .as_webhook()
                    .is_some_and(|w| w.callback == website)
                    && sub.version == "1"
                    && sub.type_ == EventType::StreamOnline
                {
                    Ok(Some((
                        sub.condition
                            .get("broadcaster_user_id")
                            .and_then(|b| b.as_str().map(ToString::to_string))
                            .context("stream.online did not contain broadcaster")?,
                        sub.id,
                    )))
                } else {
                    Ok(None)
                }
            })
            .try_collect()
            .await?;

        tracing::info!("got existing subs");

        let expected_subs: HashSet<String> = Subscriptions::entries_async(&db)
            .query()
            .await?
            .into_iter()
            .map(|e| e.key)
            .collect();

        let transport = twitch_eventsub::Transport::webhook(website, sign_secret.to_owned());

        let to_subscribe = expected_subs.iter().filter(|e| !subs.contains_key(*e));

        for sub in to_subscribe.cloned() {
            client
                .helix
                .create_eventsub_subscription(
                    StreamOnlineV1::broadcaster_user_id(TwitchUserId::from(sub)),
                    transport.clone(),
                    &*token.read().await,
                )
                .await
                .with_context(|| "when registering online event")?;
        }

        let to_unsubscribe = subs
            .into_iter()
            .filter_map(|(e, id)| expected_subs.contains(&e).then_some(id));

        for sub in to_unsubscribe {
            client
                .helix
                .delete_eventsub_subscription(sub, &*token.read().await)
                .await
                .with_context(|| "when registering online event")?;
        }
    }
    #[allow(unreachable_code)]
    Ok(())
}

pub async fn twitch_eventsub(
    sign_secret: &str,
    cache: Arc<retainer::Cache<http::HeaderValue, ()>>,
    live_notifications: UnboundedSender<StreamOnlineV1Payload>,
    request: http::Request<axum::body::Body>,
) -> Result<String, (StatusCode, String)> {
    const MAX_ALLOWED_RESPONSE_SIZE: u64 = 64 * 1024;

    let (parts, body) = request.into_parts();
    let response_content_length = match body.size_hint().upper() {
        Some(v) => v,
        None => MAX_ALLOWED_RESPONSE_SIZE + 1, /* Just to protect ourselves from a malicious
                                                * response */
    };
    let body = if response_content_length < MAX_ALLOWED_RESPONSE_SIZE {
        hyper::body::to_bytes(body).await.map_err(|e| {
            tracing::error!("reading msg bytes: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, String::new())
        })?
    } else {
        return Err((StatusCode::BAD_REQUEST, "too big data given".to_string()));
    };

    let request = http::Request::from_parts(parts, &*body);

    tracing::debug!("got event {}", std::str::from_utf8(request.body()).unwrap());
    tracing::debug!("got event headers {:?}", request.headers());
    if !Event::verify_payload(&request, sign_secret.as_bytes()) {
        return Err((StatusCode::BAD_REQUEST, "Invalid signature".to_string()));
    }

    if let Some(id) = request.headers().get("Twitch-Eventsub-Message-Id") {
        if cache.get(id).await.is_none() {
            cache.insert(id.clone(), (), 400).await;
        } else {
            tracing::debug!("got already seen event");
            return Ok(String::new());
        }
    }

    // Event is verified, now do stuff.
    let event = Event::parse_http(&request).unwrap();
    // let event =
    // Event::parse(std::str::from_utf8(request.body()).unwrap()).unwrap();
    tracing::info_span!("valid_event", event=?event);
    tracing::info!("got event!");

    if let Some(ver) = event.get_verification_request() {
        tracing::info!("subscription was verified");
        return Ok(ver.challenge.clone());
    }

    if event.is_revocation() {
        tracing::info!("subscription was revoked");
        return Ok(String::new());
    }

    if let Event::StreamOnlineV1(Payload {
        message: Message::Notification(notification),
        ..
    }) = event
    {
        tracing::info!(broadcaster_id=?notification.broadcaster_user_id, "sending live status to clients");
        live_notifications
            .unbounded_send(notification)
            .expect("should be able to send to live_notifications channel");
    }
    Ok(String::new())
}

pub async fn refresher(
    client: &TwitchClient,
    token: Arc<RwLock<AppAccessToken>>,
    client_id: ClientId,
    client_secret: ClientSecret,
) -> Result {
    loop {
        tracing::info!("hello!");
        tokio::time::sleep(token.read().await.expires_in() - tokio::time::Duration::from_secs(20))
            .await;
        let t = &mut *token.write().await;
        *t = AppAccessToken::get_app_access_token(
            client.get_client(),
            client_id.clone(),
            client_secret.clone(),
            vec![],
        )
        .await?;
    }
}

// https://github.com/twitch-rs/twitch_api/blob/main/examples/eventsub/src/twitch.rs#L131C10-L131C10
pub async fn run(
    client: &TwitchClient,
    new_subscriptions: UnboundedReceiver<TwitchUserId>,
    live_notifications: UnboundedSender<StreamOnlineV1Payload>,
    db: AsyncDatabase,
) -> Result {
    let port = env::var("TWITCH_WEBHOOK_PORT").context("getting $TWITCH_WEBHOOK_PORT")?;
    let path = env::var("TWITCH_WEBHOOK_PATH").context("getting $TWITCH_WEBHOOK_PATH")?;
    let url = env::var("TWITCH_WEBHOOK_URL").context("getting $TWITCH_WEBHOOK_URL")?;
    let client_id: ClientId = env::var("TWITCH_CLIENT_ID")
        .context("getting $TWITCH_CLIENT_ID")?
        .parse()?;
    let client_secret: ClientSecret = env::var("TWITCH_CLIENT_SECRET")
        .context("getting $TWITCH_CLIENT_SECRET")?
        .parse()?;
    let sign_secret = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), 64);

    let token = Arc::new(RwLock::new(
        AppAccessToken::get_app_access_token(
            client.get_client(),
            client_id.clone(),
            client_secret.clone(),
            vec![],
        )
        .await?,
    ));

    let server = {
        let cache = Arc::new(retainer::Cache::new());
        let sign_secret = sign_secret.clone();
        axum::Server::bind(
            &SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                port.parse().context("parsing $WEBHOOK_PORT")?,
            )
            .into(),
        )
        .serve(
            Router::new()
                .route(
                    &path,
                    post(move |request: http::Request<axum::body::Body>| async move {
                        twitch_eventsub(&sign_secret, cache, live_notifications, request).await
                    }),
                )
                .into_make_service(),
        )
        .map_err(anyhow::Error::from)
    };

    let new_subscriptions = {
        let token = token.clone();
        let url = &url;
        let sign_secret = &sign_secret;
        new_subscriptions
            .then(move |sub| {
                let token = token.clone();
                let transport = twitch_eventsub::Transport::webhook(url, sign_secret.to_owned());
                async move {
                    client
                        .helix
                        .create_eventsub_subscription(
                            StreamOnlineV1::broadcaster_user_id(sub),
                            transport,
                            &*token.read().await,
                        )
                        .await
                        .with_context(|| "when registering online event")
                        .map(|_| ())
                }
            })
            .try_collect()
    };

    select! {
        server = server => server,
        reciever = refresher(client, token.clone(), client_id, client_secret) => reciever,
        registrer = eventsub_register(token, client, &url, &sign_secret, db) => registrer,
        new_subscriptions = new_subscriptions => new_subscriptions,
    }
}
