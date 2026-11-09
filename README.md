# dirigera-tui

A small terminal UI for controlling IKEA smart home devices through a **DIRIGERA** hub.

It talks directly to the hub on your local network, with no cloud and no IKEA account needed.

> Not affiliated with or endorsed by IKEA. The hub's local API isn't officially documented, so
> things may break with firmware updates.

## Features

- Lights: on/off, brightness, color temperature, color
- Outlets: on/off
- Blinds: open/close and position
- Air purifiers: fan mode
- Sensors: temperature, humidity, PM2.5, motion, door/window, water leak, battery
- Scenes: trigger them from a list
- Toggle all the lights in a room at once
- Device sets (combined lights, e.g. a fixture with several bulbs) show as one row
  that controls every bulb; press `e` to expand it and control bulbs one by one
- Changes show up instantly, and the device list refreshes every few seconds

## Install

You need a [Rust toolchain](https://rustup.rs) (`cargo`).

```sh
git clone https://github.com/<you>/dirigera-tui
cd dirigera-tui
cargo install --path .
```

This puts `dirigera-tui` in `~/.cargo/bin`. Alternatively, run `cargo build --release` and use
`target/release/dirigera-tui`.

## Setup

1. Find your hub's IP address. Your router's list of connected devices is the easiest place to
   look. Giving the hub a fixed IP there is a good idea.
2. Pair:
   ```sh
   dirigera-tui pair 192.168.1.50
   ```
   When it asks, press the **action button on the bottom of the hub**, then press Enter.
3. Run it:
   ```sh
   dirigera-tui
   ```

The hub address and token are saved to `~/.config/dirigera-tui/config.toml`. Keep that file
private, because anyone with the token can control your devices. You can also use the
`DIRIGERA_HOST` and `DIRIGERA_TOKEN` environment variables instead.

## Keys

| Key                | Action                                         |
|--------------------|------------------------------------------------|
| `j` `k` / `↑` `↓`  | Move                                           |
| `Tab`, `1`, `2`    | Switch between Devices and Scenes              |
| `Space` / `Enter`  | Toggle device / trigger scene                  |
| `h` `l` / `←` `→`  | Brightness or blinds position −/+10            |
| `H` `L`            | Brightness −/+1                                |
| `[` `]`            | Warmer / cooler light                          |
| `c` `C`            | Cycle color                                    |
| `a`                | Toggle all lights in the selected device's room |
| `e`                | Expand / collapse a device set                 |
| `i`                | Show the device's raw data from the hub        |
| `r`                | Refresh now                                    |
| `?`                | Help                                           |
| `q` / `Esc`        | Quit                                           |

## Troubleshooting

- **"could not reach hub"**: check the IP, and make sure you're on the same network.
- **401 Unauthorized**: the token is no longer valid. Run `dirigera-tui pair <ip>` again.
- **A device shows wrong values or can't be controlled**: select it and press `i`. Then open an
  issue with what it shows. Remove anything you consider private first.

## Contributing

Contributions are welcome. You don't need IKEA hardware: `tools/mock_hub.py` is a fake hub
for testing, and its file header explains how to run it. [`AGENTS.md`](AGENTS.md) describes
how the code is structured. It's written for AI coding assistants, but it works just as well
for humans.

## License

[MIT](LICENSE)
