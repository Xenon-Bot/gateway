use std::{env, error::Error};
use tokio::stream::StreamExt;
use twilight_gateway::{cluster::{Cluster, ShardScheme}, queue::{LargeBotQueue, Queue}, Event, EventType, EventTypeFlags};
use twilight_model::gateway::Intents;
use twilight_http::Client as HttpClient;
use std::sync::Arc;
use tracing_subscriber;
use redis::{aio::MultiplexedConnection as RedisConnection};
use serde_json::{Value as JSONValue};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct GatewayPayload {
    op: u32,
    s: u128,
    // String for OP 0; otherwise null
    t: JSONValue,
    d: JSONValue,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt::init();

    let redis_url = env::var("REDIS_URL")
        .expect("No redis_url given");

    let redis_client = redis::Client::open(redis_url)
        .expect("Failed to connect to redis");
    let (redis_con, redis_fut) = redis_client
        .create_multiplexed_tokio_connection()
        .await?;
    // I have no idea if that's how you are supposed to do it, but I think it works lol
    tokio::spawn(redis_fut);

    let token = env::var("DISCORD_TOKEN")
        .expect("No discord token given");
    let raw_intents = env::var("DISCORD_INTENTS")
        .expect("No discord intents given")
        .trim()
        .parse::<u64>()
        .expect("Invalid value for discord intents given");
    let intents = Intents::from_bits(raw_intents)
        .expect("Invalid intent value given");

    let http = HttpClient::new(&token);
    let gateway = http
        .gateway()
        .authed()
        .await
        .expect("Can't fetch gateway info");

    let queue: Arc<Box<dyn Queue>> = Arc::new(
        Box::new(
            LargeBotQueue::new(
                gateway
                    .session_start_limit
                    .max_concurrency as usize,
                &http,
            ).await
        )
    );
    println!("{:?}", gateway);

    let scheme = ShardScheme::Range {
        from: 0,
        to: gateway.shards - 1,
        total: gateway.shards,
    };
    let cluster = Cluster::builder(&token, intents)
        .shard_scheme(scheme)
        .queue(queue)
        .http_client(http)
        .build()
        .await?;

    let cluster_spawn = cluster.clone();
    tokio::spawn(async move {
        cluster_spawn.up().await;
    });

    let mut events = cluster
        .some_events(EventTypeFlags::from(EventType::ShardPayload));
    while let Some((shard_id, event)) = events.next().await {
        tokio::spawn(handle_event(redis_con.clone(), shard_id, event));
    }

    Ok(())
}

async fn cache_event(mut redis: RedisConnection, event: &str, data: &JSONValue, raw_data: &String) {
    // Just an example for how caching could work; I'm not actually planing to cache guilds
    match event {
        "GUILD_CREATE" => {
            if let Some(guild_id) = &data["id"].as_str() {
                let _ = redis::cmd("HSET")
                    .arg("guilds")
                    .arg(*guild_id)
                    .arg(raw_data)
                    .query_async::<RedisConnection, ()>(&mut redis)
                    .await;
            }
        }
        _ => {}
    }
}

async fn handle_event(mut redis: RedisConnection, _shard_id: u64, event: Event) {
    if let Event::ShardPayload(raw_payload) = event {
        let payload: GatewayPayload = match serde_json::from_slice(&raw_payload.bytes) {
            Ok(res) => res,
            Err(_) => return
        };
        let _ = redis::cmd("INCR")
            .arg(format!("gateway:op:{}", payload.op))
            .query_async::<RedisConnection, ()>(&mut redis)
            .await;

        if let Some(t) = payload.t.as_str() {
            let raw_data = payload.d.to_string();
            cache_event(redis.clone(), t, &payload.d, &raw_data).await;
            let _ = redis::cmd("PUBLISH")
                .arg(format!("gateway:events:{}", t.trim().to_lowercase()))
                .arg(&raw_data)
                .query_async::<RedisConnection, ()>(&mut redis)
                .await;
            let _ = redis::cmd("INCR")
                .arg(format!("gateway:events:{}", t.trim().to_lowercase()))
                .query_async::<RedisConnection, ()>(&mut redis)
                .await;
        };
    };
}
