use anyhow::anyhow;
use chrono::{DateTime, Local};
use log::{debug, error, info, warn, LevelFilter};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::{Joined, Room};
use matrix_sdk::ruma::events::reaction::{ReactionEventContent, Relation};
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::ruma::exports::http::StatusCode;
use matrix_sdk::ruma::{OwnedEventId, OwnedRoomId};
use matrix_sdk::Client as MatrixClient;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde::Serialize;
use std::env;
use std::fs::File;
use std::io::Read;
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

// Based on example at: https://github.com/matrix-org/matrix-rust-sdk/tree/main/examples/command_bot

const CACHE_DIR: &str = "matrix-firefly-bot";
const BOT_NAME: &str = "firefly bot";

const FIREFLY_GENERAL_EXPENSE: &str = "General expense";
const FIREFLY_TRANSACTIONS_API: &str = "api/v1/transactions";

const ADD_CMD: &str = "!add";
const HELP_CMD: &str = "!help";
const PING_CMD: &str = "!ping";

const ADD_USAGE: &str = "!add <Category>: <Amount>";
const INVALID_ARGS: &str = "Invalid arguments.";

#[derive(Debug)]
struct AddArgs {
    category: String,
    amount: f64,
}

#[derive(Debug)]
enum Cmd {
    Ping,
    Help,
    Add(AddArgs),
}

#[derive(Serialize, Deserialize, Debug)]
struct Transaction {
    #[serde(rename = "type")]
    transaction_type: String,
    date: DateTime<Local>,
    amount: f64,
    description: String,
    category_name: String,
    source_id: i64,
    destination_name: String,
    tags: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Transactions {
    transactions: Vec<Transaction>,
}

impl Transaction {
    fn withdrawal(
        category: String,
        amount: f64,
        date: DateTime<Local>,
        source_id: i64,
        destination_name: String,
        person: String,
    ) -> Self {
        Self {
            transaction_type: "withdrawal".to_string(),
            date,
            amount,
            description: format!("Expense {category} by {person}"),
            category_name: category,
            source_id,
            destination_name,
            tags: vec![person],
        }
    }
}

impl Transactions {
    fn new(transaction: Transaction) -> Self {
        Self {
            transactions: vec![transaction],
        }
    }
}

#[derive(Deserialize, Debug)]
struct Config {
    matrix_homeserver_url: String,
    matrix_username: String,
    matrix_password: String,
    matrix_room_id: String,
    firefly_url: String,
    firefly_api_key: String,
    firefly_source_account_id: i64,
}

struct MatrixFireflyBot {
    config: Config,
    http_client: HttpClient,
}

impl MatrixFireflyBot {
    fn new(config: Config) -> Self {
        Self {
            config,
            http_client: reqwest::Client::new(),
        }
    }

    async fn start(self) -> anyhow::Result<()> {
        info!("Initializing...");

        let home = dirs::data_dir().unwrap().join(CACHE_DIR);

        let client = MatrixClient::builder()
            .homeserver_url(&self.config.matrix_homeserver_url)
            .sled_store(home, None)?
            .build()
            .await?;

        client
            .login_username(&self.config.matrix_username, &self.config.matrix_password)
            .initial_device_display_name(BOT_NAME)
            .send()
            .await?;

        let response = client.sync_once(SyncSettings::default()).await?;

        let room_id = OwnedRoomId::try_from(self.config.matrix_room_id.as_str())?;

        let self_arc = Arc::new(self);
        client.add_room_event_handler(&room_id, {
            let self_arc = Arc::clone(&self_arc);
            move |event: OriginalSyncRoomMessageEvent, room: Room| {
                let self_arc = Arc::clone(&self_arc);
                async move {
                    if let Err(e) = self_arc.on_room_message(event, room).await {
                        error!("Failed to process message: {e}");
                    }
                }
            }
        });

        info!("Listening for messages...");

        let settings = SyncSettings::default().token(response.next_batch);
        client.sync(settings).await?;

        Ok(())
    }

    async fn on_room_message(
        &self,
        event: OriginalSyncRoomMessageEvent,
        room: Room,
    ) -> anyhow::Result<()> {
        debug!("Received event: {event:?}");

        if let Room::Joined(room) = room {
            let MessageType::Text(message) = event.content.msgtype else {
                return Ok(());
            };

            let content = message.body;

            if !content.starts_with('!') {
                return Ok(());
            }

            let username = event.sender.localpart();
            let timestamp = event
                .origin_server_ts
                .to_system_time()
                .ok_or_else(|| anyhow!("Failed to extract message timestamp"))?;

            let cmd = match Cmd::parse(&content) {
                Ok(cmd) => cmd,
                Err(e) => {
                    warn!("Failed to parse: '{content}'. {e}");
                    send_message(e.to_string(), &room).await?;
                    return Ok(());
                }
            };

            info!("Received command: {cmd:?}");

            match cmd {
                Cmd::Ping => send_message("pong".to_string(), &room).await?,
                Cmd::Help => {
                    send_message(
                        format!(
                            "Available commands:\n - {ADD_USAGE}\n - {HELP_CMD}\n - {PING_CMD}"
                        ),
                        &room,
                    )
                    .await?;
                }
                Cmd::Add(AddArgs { category, amount }) => {
                    match self
                        .add_expense(&category, amount, username, timestamp)
                        .await
                    {
                        Ok(_) => {
                            send_reaction("✅".to_owned(), event.event_id.clone(), &room).await?;
                        }
                        Err(e) => {
                            error!("{e}");
                            send_reaction("❌".to_owned(), event.event_id.clone(), &room).await?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn add_expense(
        &self,
        category: &str,
        amount: f64,
        username: &str,
        timestamp: SystemTime,
    ) -> anyhow::Result<()> {
        let transaction = Transactions::new(Transaction::withdrawal(
            category.to_string(),
            amount,
            timestamp.into(),
            self.config.firefly_source_account_id,
            FIREFLY_GENERAL_EXPENSE.to_string(),
            username.to_string(),
        ));

        let response = self
            .http_client
            .post(format!(
                "{}/{FIREFLY_TRANSACTIONS_API}",
                self.config.firefly_url
            ))
            .header(
                "Authorization",
                format!("Bearer {}", self.config.firefly_api_key),
            )
            .json(&transaction)
            .send()
            .await;

        match response {
            Ok(response) if response.status() != StatusCode::OK => {
                return Err(anyhow!(
                    "Failed to add transaction: [{:?}] {}",
                    response.status(),
                    response
                        .text()
                        .await
                        .unwrap_or_else(|_| { "failed to read response body".to_string() })
                ));
            }
            Err(e) => {
                return Err(anyhow!("Failed to execute HTTP request: {e}"));
            }
            _ => {}
        }

        Ok(())
    }
}

impl Cmd {
    fn parse(input: &str) -> anyhow::Result<Self> {
        let cmd_end = input.find(' ').unwrap_or(input.len());
        let cmd_str = &input[..cmd_end];
        let cmd_args = if cmd_end == input.len() {
            ""
        } else {
            &input[cmd_end + 1..]
        };

        match cmd_str {
            HELP_CMD => Ok(Cmd::Help),
            PING_CMD => Ok(Cmd::Ping),
            ADD_CMD => Ok(Cmd::Add(AddArgs::parse(cmd_args)?)),
            _ => Err(anyhow!("Unknown command: {cmd_str}")),
        }
    }
}

impl AddArgs {
    fn parse(args: &str) -> anyhow::Result<Self> {
        let Some((category, amount_str)) = args.split_once(':').map(|(category, amount)| {
            let mut a = amount.trim();
            if a.starts_with('$') {
                a = &a[1..];
            }
            (category.trim(), a)
        }) else {
            return Err(anyhow!("{INVALID_ARGS} Usage: {ADD_USAGE}"))
        };

        if category.is_empty() || amount_str.is_empty() {
            return Err(anyhow!("{INVALID_ARGS} Usage: {ADD_USAGE}"));
        }

        let Ok(amount) = f64::from_str(amount_str) else {
            return Err(anyhow!("Invalid amount: {amount_str}"))
        };

        Ok(Self {
            category: category.to_string(),
            amount,
        })
    }
}

async fn send_message(content: String, room: &Joined) -> anyhow::Result<()> {
    room.send(RoomMessageEventContent::text_plain(content), None)
        .await?;
    Ok(())
}

async fn send_reaction(
    reaction: String,
    event_id: OwnedEventId,
    room: &Joined,
) -> anyhow::Result<()> {
    room.send(
        ReactionEventContent::new(Relation::new(event_id, reaction)),
        None,
    )
    .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(LevelFilter::Info)
        .format_target(false)
        .init();

    if env::args().len() != 2 {
        error!("Usage: {} <PATH_TO_CONFIG>", env::args().next().unwrap());
        exit(1)
    }

    let mut config_file = File::open(env::args().nth(1).unwrap())?;
    let mut bytes = Vec::new();
    config_file.read_to_end(&mut bytes)?;

    let config = toml::from_slice(&bytes)?;

    MatrixFireflyBot::new(config).start().await?;

    info!("Exiting");

    Ok(())
}
