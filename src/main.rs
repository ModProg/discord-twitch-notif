#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::too_many_lines)]

use std::collections::HashSet;
use std::convert::AsRef;
use std::env;
use std::pin::Pin;

use anyhow::Context as _;
use bonsaidb::core::document::{CollectionDocument, Emit};
use bonsaidb::core::schema::{
    Collection, CollectionMapReduce, SerializedCollection, SerializedView, View, ViewMapResult,
    ViewSchema,
};
use bonsaidb::local::config::{Builder, StorageConfiguration};
use bonsaidb::local::AsyncDatabase;
use clap::Parser;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::{Future, StreamExt};
use poise::serenity_prelude::{self as serenity, UserId as DiscordUserId};
use serde::{Deserialize, Serialize};
use tokio::select;
use twitch_api::eventsub::stream::StreamOnlineV1Payload;
use twitch_api::twitch_oauth2::AppAccessToken;
use twitch_api::types::{UserId as TwitchUserId, UserIdRef, VideoType};

use crate::websocket::WebsocketClient;

type Result<T = (), E = anyhow::Error> = std::result::Result<T, E>;
type TwitchClient = twitch_api::TwitchClient<'static, reqwest::Client>;

mod webhook;
mod websocket;

#[derive(Collection, Deserialize, Serialize, Debug, Clone)]
#[collection(name = "user_data", views = [Subscriptions])]
struct UserData {
    #[natural_id]
    key: u64,
    subscriptions: HashSet<TwitchUserId>,
}

#[derive(View, ViewSchema, Clone, Copy)]
// TODO ask ecton for a better way
#[view(collection=UserData, key=String, value=Vec<u64>)]
struct Subscriptions;

impl CollectionMapReduce for Subscriptions {
    fn map<'doc>(
        &self,
        document: CollectionDocument<<Self::View as View>::Collection>,
    ) -> ViewMapResult<'doc, Self>
    where
        CollectionDocument<<Self::View as View>::Collection>: 'doc,
    {
        document
            .contents
            .subscriptions
            .into_iter()
            .map(|user_id| {
                document
                    .header
                    .emit_key_and_value(user_id.take(), vec![document.contents.key])
            })
            .collect()
    }

    fn reduce(
        &self,
        mappings: &[bonsaidb::core::schema::ViewMappedValue<'_, Self>],
        _rereduce: bool,
    ) -> bonsaidb::core::schema::ReduceResult<Self::View> {
        Ok(mappings.iter().flat_map(|e| e.value.clone()).collect())
    }
}

struct Data {
    db: AsyncDatabase,
    twitch_client: TwitchClient,
    twitch_token: AppAccessToken,
    new_subscriptions: UnboundedSender<TwitchUserId>,
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

/// Subscribe to twich streamer's live notifications
#[poise::command(slash_command, prefix_command, ephemeral)]
async fn subscribe(
    ctx: Context<'_>,
    #[description = "Twitch Streamer"] streamer: String,
) -> Result<(), Error> {
    let Some(streamer) = ctx
        .data()
        .twitch_client
        .helix
        .get_channel_from_login(&streamer, &ctx.data().twitch_token)
        .await?
    else {
        ctx.say(format!("404: No such streamer: `{streamer}`"))
            .await?;
        return Ok(());
    };

    let response = format!("Subscribed to {}", streamer.broadcaster_name);

    let _ = UserData {
        key: ctx.author().id.0,
        subscriptions: HashSet::default(),
    }
    .push_into_async(&ctx.data().db)
    .await;
    let mut data = UserData::get_async(&ctx.author().id.0, &ctx.data().db)
        .await?
        .expect("I shouldn't delete those I think");

    {
        let broadcaster_id = streamer.broadcaster_id.clone();
        data.modify_async(&ctx.data().db, move |data| {
            data.contents.subscriptions.insert(broadcaster_id.clone());
        })
        .await?;
    }

    ctx.data()
        .new_subscriptions
        .unbounded_send(streamer.broadcaster_id)?;

    ctx.say(response).await?;
    Ok(())
}

/// List current subscriptions
#[poise::command(slash_command, prefix_command, ephemeral)]
async fn subscriptions(ctx: Context<'_>) -> Result<(), Error> {
    let Some(subscriptions) = UserData::get_async(&ctx.author().id.0, &ctx.data().db)
        .await?
        .map(|data| data.contents.subscriptions)
    else {
        ctx.say("Currently not subscribed anyone.").await?;
        return Ok(());
    };

    let subscriptions: Vec<&UserIdRef> = subscriptions.iter().map(AsRef::as_ref).collect();
    let subscriptions = ctx
        .data()
        .twitch_client
        .helix
        .get_channels_from_ids(&subscriptions, &ctx.data().twitch_token)
        .await?;

    let response = format!(
        "Currently subscribed to: {}",
        subscriptions
            .into_iter()
            .map(|channel| channel.broadcaster_name)
            .collect::<Vec<_>>()
            .join(", ")
    );

    ctx.say(response).await?;
    Ok(())
}

#[derive(Parser)]
enum Mode {
    Websocket,
    Webhook,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt().init();
    let mode = Mode::parse();
    let db =
        AsyncDatabase::open::<UserData>(StorageConfiguration::new("user_data.bonsaidb")).await?;
    let twitch_client = TwitchClient::default();
    // TODO actually use the refresher :D
    let twitch_token = AppAccessToken::get_app_access_token(
        &twitch_client,
        env::var("TWITCH_CLIENT_ID")
            .context("$TWITCH_CLIENT_ID")?
            .into(),
        env::var("TWITCH_CLIENT_SECRET")
            .context("$TWITCH_CLIENT_SECRET")?
            .into(),
        vec![],
    )
    .await?;

    let new_subscriptions = unbounded();
    let mut live_notifications = unbounded();
    let ws: Pin<Box<dyn Future<Output = Result>>> = {
        match mode {
            Mode::Websocket => {
                let subscriptions = Subscriptions::entries_async(&db)
                    .query()
                    .await?
                    .into_iter()
                    .map(|e| TwitchUserId::from(e.key))
                    .collect();

                Box::pin(
                    WebsocketClient::new(
                        &twitch_client,
                        new_subscriptions.1,
                        live_notifications.0,
                        subscriptions,
                    )
                    .await?
                    .run(),
                )
            }
            Mode::Webhook => Box::pin(webhook::run(
                &twitch_client,
                new_subscriptions.1,
                live_notifications.0,
                db.clone(),
            )),
        }
    };

    let framework = {
        let db = db.clone();
        let twitch_client = twitch_client.clone();
        poise::Framework::builder()
            .options(poise::FrameworkOptions {
                commands: vec![subscribe(), subscriptions()],
                ..Default::default()
            })
            .token(env::var("DISCORD_TOKEN").expect("missing DISCORD_TOKEN"))
            .intents(serenity::GatewayIntents::non_privileged())
            .setup(|ctx, _ready, framework| {
                Box::pin(async move {
                    poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                    Ok(Data {
                        db,
                        new_subscriptions: new_subscriptions.0,
                        twitch_client,
                        twitch_token,
                    })
                })
            })
            .build()
            .await?
    };
    let discord = framework.client().cache_and_http.clone();
    let discord = &discord;

    select!(
        ws = ws => ws?,
        framework = framework.start() => framework?,
        Some(StreamOnlineV1Payload{ broadcaster_user_id, broadcaster_user_login, broadcaster_user_name, type_, started_at, .. }) = live_notifications.1.next() => {
            if type_ == VideoType::Live {
                for subscriber in Subscriptions::entries_async(&db)
                    .with_key(&broadcaster_user_id.take())
                    .query()
                    .await?
                    .into_iter()
                    .flat_map(|k| k.value)
                {
                    DiscordUserId(subscriber)
                        .create_dm_channel(&discord)
                        .await?
                        .say(
                            &discord.http,
                            format!(
                                "[{broadcaster_user_name}](https://twitch.tv/{broadcaster_user_login}) \
                                 went online <t:{}:R>.",
                                started_at.to_utc().unix_timestamp()
                            ),
                        )
                        .await?;
                }
            }
        }
    );
    Ok(())
}
