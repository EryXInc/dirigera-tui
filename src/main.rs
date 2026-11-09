mod api;
mod app;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
struct Config {
    host: String,
    token: String,
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dirigera-tui")
        .join("config.toml")
}

fn load_config() -> Result<Config> {
    if let (Ok(host), Ok(token)) = (
        std::env::var("DIRIGERA_HOST"),
        std::env::var("DIRIGERA_TOKEN"),
    ) {
        return Ok(Config { host, token });
    }
    let p = config_path();
    let s = std::fs::read_to_string(&p).with_context(|| {
        format!(
            "no config at {} — run `dirigera-tui pair <hub-ip>` first",
            p.display()
        )
    })?;
    Ok(toml::from_str(&s)?)
}

const USAGE: &str = "\
dirigera-tui — control IKEA DIRIGERA devices from the terminal

USAGE:
    dirigera-tui              start the TUI
    dirigera-tui pair <ip>    pair with a hub and save the token

Config: ~/.config/dirigera-tui/config.toml
        (or set DIRIGERA_HOST and DIRIGERA_TOKEN)";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {
            let cfg = load_config()?;
            app::run(api::Client::new(&cfg.host, &cfg.token)?)
        }
        Some("pair") => {
            let host = args.get(1).context("usage: dirigera-tui pair <hub-ip>")?;
            let token = api::pair(host)?;
            let p = config_path();
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(
                &p,
                toml::to_string(&Config {
                    host: host.clone(),
                    token,
                })?,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
            }
            println!("Paired! Token saved to {}", p.display());
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}
