Notepush
========

A high performance Nostr relay for sending out push notifications using the Apple Push Notification Service (APNS).

⚠️🔥WIP! Experimental!⚠️🔥

## Installation

1. Get a build or build it yourself using `cargo build --release`
2. On the working directory from which you start this relay, create an `.env` file with the following contents:

```env
APNS_TOPIC="com.your_org.your_app"        # Your app's bundle ID
APNS_AUTH_PRIVATE_KEY_FILE_PATH=./AuthKey_1234567890.p8	# Path to the private key file used to generate JWT tokens with the Apple APNS server. You can obtain this from https://developer.apple.com/account/resources/authkeys/list
APNS_AUTH_PRIVATE_KEY_ID=1234567890 # The ID of the private key used to generate JWT tokens with the Apple APNS server. You can obtain this from https://developer.apple.com/account/resources/authkeys/list
APNS_ENVIRONMENT="development"    # The environment to use with the APNS server. Can be "development" or "production"
APPLE_TEAM_ID=1248163264        # The ID of the team. Can be found in AppStore Connect.
DB_PATH=./apns_notifications.db         # Path to the SQLite database file that will be used to store data about sent notifications, relative to the working directory
RELAY_URL=wss://relay.damus.io           # URL to the relay server which will be consulted to get information such as mute lists.
RELAY_HOST="0.0.0.0"                          # The host to bind the server to (Defaults to 0.0.0.0 to bind to all interfaces)
RELAY_PORT=9001                               # The port to bind the server to. Defaults to 9001
API_HOST="0.0.0.0"                            # The host to bind the API server to (Defaults to 0.0.0.0 to bind to all interfaces)
API_PORT=8000                                 # The port to bind the API server to. Defaults to 8000
API_BASE_URL=http://localhost:8000      # Base URL from the API is allowed access (used by the server to perform NIP-98 authentication)
```

6. Run this relay using the built binary or the `cargo run` command. If you want to change the log level, you can set the `RUST_LOG` environment variable to `DEBUG` or `INFO` before running the relay.

Example:
```sh
$ RUST_LOG=DEBUG cargo run
```

## Contributions

For contribution guidelines, please check [this](https://github.com/damus-io/damus/blob/master/docs/CONTRIBUTING.md) document.

## Development setup

1. Install the Rust toolchain
2. Clone this repository
3. Run `cargo build` to build the project
4. Run `cargo test` to run the tests
5. Run `cargo run` to run the project

## Testing utilities

You can use `test/test-inputs` with a websockets test tool such as `websocat` to play around with the relay. If you have Nix installed, you can run:

```sh
$ nix-shell
[nix-shell] $ websocat ws://localhost:9001
<ENTER_FULL_JSON_PAYLOAD_HERE_AND_PRESS_ENTER>
```
