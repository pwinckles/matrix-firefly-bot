# matrix-firefly-bot

Watches a Matrix room for commands to add expenses to [Firefly III](https://www.firefly-iii.org).

## Usage

```shell
matrix-firefly-bot <PATH_TO_CONFIG>
```

`PATH_TO_CONFIG` must be the path to a toml file that configures the bot.

## Config

```toml
# The URL to the Matrix homeserver
matrix_homeserver_url = ""
# The bot's username. eg: @example:matrix.org
matrix_username = ""
# The bot's password
matrix_password = ""
# The id of the room the bot should monitor
matrix_room_id = ""
# The URL to the Firefly server
firefly_url = ""
# The Firefly API key
firefly_api_key = ""
# The account id of the account to withdraw money from
firefly_source_account_id = 1
```

## Bot usage

```
Available commands:
 - !add <Category>: <Amount> [Note] [#Tag...]
 - !help
 - !ping
```

### Add

Adds an expense of the specified amount to the specified category.

## Raspberry Pi Build

```shell
cargo install cross
cross build --release --target armv7-unknown-linux-gnueabihf
```
