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
const FIREFLY_CATEGORIES_API: &str = "api/v1/categories";

const ADD_CMD: &str = "!add";
const CATEGORIES_CMD: &str = "!categories";
const HELP_CMD: &str = "!help";
const PING_CMD: &str = "!ping";

const ADD_USAGE: &str = "!add <Category>: <Amount> [Note] [#Tag...]";
const INVALID_ARGS: &str = "Invalid arguments.";

#[derive(Debug, PartialEq)]
struct AddArgs {
    category: String,
    amount: f64,
    note: Option<String>,
    tags: Vec<String>,
}

#[derive(Debug)]
enum Cmd {
    Ping,
    Help,
    Add(AddArgs),
    Categories,
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
    notes: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Transactions {
    transactions: Vec<Transaction>,
}

#[derive(Serialize, Deserialize)]
struct Pagination {
    pub total: i64,
    pub count: i64,
    pub per_page: i64,
    pub current_page: i64,
    pub total_pages: i64,
}

#[derive(Serialize, Deserialize, Debug)]
struct Attributes {
    name: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Category {
    id: String,
    attributes: Attributes,
}

#[derive(Serialize, Deserialize, Debug)]
struct ListCategories {
    data: Vec<Category>,
}

impl Transaction {
    #[allow(clippy::too_many_arguments)]
    fn withdrawal(
        category: String,
        amount: f64,
        date: DateTime<Local>,
        source_id: i64,
        destination_name: String,
        person: String,
        notes: Option<String>,
        mut tags: Vec<String>,
    ) -> Self {
        tags.push(person.clone());
        Self {
            transaction_type: "withdrawal".to_string(),
            date,
            amount,
            description: format!("{category} by {person}"),
            category_name: category,
            source_id,
            destination_name,
            notes,
            tags,
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
                            "Available commands:\n - {ADD_USAGE}\n - {CATEGORIES_CMD}\n - {HELP_CMD}\n - {PING_CMD}"
                        ),
                        &room,
                    )
                    .await?;
                }
                Cmd::Categories => match self.list_categories().await {
                    Ok(categories) => {
                        let mut response = String::new();
                        response.push_str("Categories:");

                        if !categories.is_empty() {
                            response.push_str("\n - ");
                            response.push_str(&categories.join("\n - "));
                        }

                        send_message(response, &room).await?;
                    }
                    Err(e) => {
                        error!("Failed to list categories: {}", e);
                        send_message("Failed to list categories".to_string(), &room).await?;
                    }
                },
                Cmd::Add(AddArgs {
                    category,
                    amount,
                    note,
                    tags,
                }) => {
                    match self
                        .add_expense(&category, amount, username, timestamp, note, tags)
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
        note: Option<String>,
        tags: Vec<String>,
    ) -> anyhow::Result<()> {
        let transaction = Transactions::new(Transaction::withdrawal(
            category.to_string(),
            amount,
            timestamp.into(),
            self.config.firefly_source_account_id,
            FIREFLY_GENERAL_EXPENSE.to_string(),
            username.to_string(),
            note,
            tags,
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

    async fn list_categories(&self) -> anyhow::Result<Vec<String>> {
        let response: ListCategories = self
            .http_client
            .get(format!(
                "{}/{FIREFLY_CATEGORIES_API}",
                self.config.firefly_url
            ))
            .header(
                "Authorization",
                format!("Bearer {}", self.config.firefly_api_key),
            )
            .send()
            .await?
            .json()
            .await?;

        Ok(response
            .data
            .into_iter()
            .map(|cat| cat.attributes.name)
            .collect())
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
            CATEGORIES_CMD => Ok(Cmd::Categories),
            ADD_CMD => Ok(Cmd::Add(AddArgs::parse(cmd_args)?)),
            _ => Err(anyhow!("Unknown command: {cmd_str}")),
        }
    }
}

impl AddArgs {
    fn parse(args: &str) -> anyhow::Result<Self> {
        if let Some((category, rest)) = args.split_once(':') {
            let (amount, rest) = rest
                .trim()
                .split_once(' ')
                .map(|(amount, rest)| {
                    let trimmed = rest.trim();
                    (
                        amount,
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        },
                    )
                })
                .unwrap_or((rest, None));

            let has_note = rest.map(|rest| !rest.starts_with('#')).unwrap_or(false);

            let text_parts = rest.map(|rest| {
                rest.split('#')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
            });

            let note = text_parts.as_ref().and_then(|parts| {
                if has_note {
                    parts.get(0).cloned()
                } else {
                    None
                }
            });

            let tags = if has_note {
                Vec::from(&text_parts.unwrap()[1..])
            } else {
                text_parts.unwrap_or_default()
            };

            let mut amount_str = amount.trim();
            if amount_str.starts_with('$') {
                amount_str = &amount_str[1..];
            }

            let category = category.trim();

            if category.is_empty() || amount_str.is_empty() {
                return Err(anyhow!("{INVALID_ARGS} Usage: {ADD_USAGE}"));
            }

            let Ok(amount) = f64::from_str(amount_str) else {
                return Err(anyhow!("Invalid amount: {amount_str}"))
            };

            Ok(Self {
                category: category.to_string(),
                amount,
                note,
                tags,
            })
        } else {
            Err(anyhow!("{INVALID_ARGS} Usage: {ADD_USAGE}"))
        }
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

#[cfg(test)]
mod tests {
    use crate::AddArgs;

    #[test]
    fn test_parse_add() {
        assert_add_arg(parse_add("Test: 1.23"), "Test", 1.23, None, vec![]);
        assert_add_arg(
            parse_add("multi word cat: $100"),
            "multi word cat",
            100.00,
            None,
            vec![],
        );
        assert_add_arg(
            parse_add("cat : 0.25 this is a note"),
            "cat",
            0.25,
            Some("this is a note"),
            vec![],
        );
        assert_add_arg(
            parse_add("test: 1.25 #tag"),
            "test",
            1.25,
            None,
            vec!["tag"],
        );
        assert_add_arg(
            parse_add("test: 1 this is a note #one #two #three"),
            "test",
            1.00,
            Some("this is a note"),
            vec!["one", "two", "three"],
        );
        assert_add_arg(
            parse_add(
                "   weird spacing   :   $1.01    this one has  extra   spacing    #one    #  two  ",
            ),
            "weird spacing",
            1.01,
            Some("this one has  extra   spacing"),
            vec!["one", "two"],
        );
    }

    fn parse_add(args: &str) -> AddArgs {
        AddArgs::parse(args).unwrap()
    }

    fn assert_add_arg(
        actual: AddArgs,
        category: &str,
        amount: f64,
        note: Option<&str>,
        tags: Vec<&str>,
    ) {
        assert_eq!(
            AddArgs {
                category: category.to_string(),
                amount,
                note: note.map(|note| note.to_string()),
                tags: tags.into_iter().map(|tag| tag.to_string()).collect(),
            },
            actual
        );
    }
}
