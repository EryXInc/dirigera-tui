# AGENTS.md

Notes for AI coding agents (and humans) working on this repo.

## What this is

`dirigera-tui` is a terminal UI, written in Rust, for controlling IKEA smart home devices
through a DIRIGERA hub's **local** REST API. It talks only to the hub on the LAN, with no cloud.
Built on `ratatui` 0.29 (using `ratatui::crossterm`) and blocking `reqwest`.

## Commands

```sh
cargo build                      # dev build
cargo build --release
cargo clippy -- -D warnings      # keep this clean
cargo fmt
cargo run -- pair <hub-ip>       # pair with a real hub (press its action button)
cargo run                        # start the TUI
```

Config lives in `~/.config/dirigera-tui/config.toml` (`host`, `token`).
The env vars `DIRIGERA_HOST` and `DIRIGERA_TOKEN` override it.

## Testing without a hub

There are no automated tests yet. `tools/mock_hub.py` is a fake hub (stdlib Python, HTTPS on
127.0.0.1:18443, token `TOK`). Its docstring has the setup steps. It logs PATCH bodies to stderr.
That makes it easy to check what the TUI actually sends.

To check the UI non-interactively, run the binary in a detached `tmux` session. Then use
`tmux send-keys` and `tmux capture-pane -p`.

The mock's device data is **invented**. If you change how attributes are parsed, mention that
it hasn't been verified against a real hub. Better still, ask the user to paste the
raw attributes (press `i` in the TUI).

## Layout

- `src/main.rs`: CLI argument handling (`pair`, `--help`), config loading and saving.
- `src/api.rs`: hub client. PKCE pairing, `Device` and `Scene` types, and the
  `devices()`, `scenes()`, `set_attributes()` and `trigger_scene()` calls.
- `src/app.rs`: everything in the TUI, in sections: background worker, `App` state and key
  handling, rendering, and the `run()` loop.

## Architecture

- **All HTTP runs on one worker thread.** The UI thread sends `Cmd`s over an mpsc channel and
  gets `Msg`s back. Never make network calls from the UI thread.
- **Optimistic updates.** `App::set()` writes attributes into the local `Device` straight away,
  then sends `Cmd::Set`.
- **Coalescing.** The worker drains queued commands and merges `Set`s for the same device into
  one PATCH. This is why holding a key down doesn't flood the hub.
- **Stale-data guard.** Each `Set` carries an increasing `seq`, and `Msg::Data` reports the
  highest `seq` applied before the fetch. `App::on_data` ignores data older than the latest local
  edit, so optimistic state doesn't flicker back.
- The UI refreshes every 5 s (`REFRESH_EVERY`) and after every change.
- `Device.attributes` is a raw `serde_json::Map` on purpose. Device types differ a lot, so read
  values with the `b()`, `n()` and `s()` helpers instead of adding typed fields. Unknown
  device types should still show up and not crash anything.
- Lists are fetched as `Vec<Value>` and then parsed one entry at a time, so a single odd device
  can't break the whole list.

## DIRIGERA API notes

- Base URL `https://<hub>:8443/v1`. The certificate is self-signed, so verification is turned
  off on purpose. Don't "fix" this.
- Authentication: `Authorization: Bearer <token>`. The token comes from the PKCE flow:
  `GET /oauth/authorize`, then the user presses the hub button, then `POST /oauth/token`.
- `GET /devices`, `GET /scenes`, `POST /scenes/{id}/trigger`.
- To change a device: `PATCH /devices/{id}` with body `[{"attributes": {...}}]` (note the
  array).
- Common attributes: `customName`, `model`, `firmwareVersion`, `isOn`, `lightLevel` (1–100),
  `colorTemperature` with `colorTemperatureMin`/`Max` (in Kelvin; min can be **greater** than
  max, so always take min/max of the pair), `colorHue` (0–360), `colorSaturation` (0–1),
  `blindsCurrentLevel`/`blindsTargetLevel`, `fanMode`, `batteryPercentage`,
  `currentTemperature`, `currentRH`, `currentPM25`, `isDetected`, `isOpen`, `waterLeakDetected`.
- `deviceType` values handled: `light`, `outlet`, `blinds`, `airPurifier`,
  `environmentSensor`, `motionSensor`, `openCloseSensor`, `waterSensor`. `gateway` is hidden.
- There is no official public API documentation. Behaviour has been worked out from community
  projects (for example the Python `dirigera` library).

## Known gaps and uncertainties

- Not yet tested against real hardware for blinds, air purifiers or color bulbs.
- Blinds direction (whether 100 means closed or open) is assumed, not verified.
- No support for speakers, device sets, or editing rooms or scenes.
- Pairing requires a hub IP. There is no mDNS discovery.

## Conventions

- Keep the dependency list small. The project is meant to stay a "lil" tool.
- Rendering must work in an 80×24 terminal. Avoid emoji, since their width varies between
  terminals. Use simple Unicode symbols like the ones already in `summary()`.
- When adding a keybinding, update `HELP` in `app.rs`, the footer hint if relevant, and the
  README.
- Never log or print the token.
