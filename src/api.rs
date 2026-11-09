//! Minimal client for the IKEA DIRIGERA hub's local REST API (https://<hub>:8443/v1).

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Map, Value};
use std::time::Duration;

fn http() -> Result<reqwest::blocking::Client> {
    // The hub uses a self-signed certificate, so verification has to be off.
    Ok(reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(10))
        .build()?)
}

fn base_url(host: &str) -> String {
    if host.contains(':') {
        format!("https://{host}/v1")
    } else {
        format!("https://{host}:8443/v1")
    }
}

/// Run the OAuth PKCE pairing flow. Requires pressing the hub's action button.
pub fn pair(host: &str) -> Result<String> {
    use base64::Engine;
    use rand::{distributions::Alphanumeric, Rng};
    use sha2::{Digest, Sha256};

    let http = http()?;
    let base = base_url(host);
    let verifier: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(128)
        .map(char::from)
        .collect();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));

    #[derive(Deserialize)]
    struct CodeResp {
        code: String,
    }
    let code: CodeResp = http
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("audience", "homesmart.local"),
            ("response_type", "code"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .with_context(|| format!("could not reach hub at {host}"))?
        .error_for_status()?
        .json()?;

    println!("Press the action button on the bottom of your DIRIGERA hub,");
    println!("then press Enter here (within ~60 seconds)...");
    std::io::stdin().read_line(&mut String::new())?;

    #[derive(Deserialize)]
    struct TokenResp {
        access_token: String,
    }
    let name = std::env::var("HOSTNAME").unwrap_or_else(|_| "dirigera-tui".into());
    let tok: TokenResp = http
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("code", code.code.as_str()),
            ("name", name.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ])
        .send()?
        .error_for_status()
        .context("token request rejected (was the button pressed?)")?
        .json()?;
    Ok(tok.access_token)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Room {
    pub id: String,
    pub name: String,
}

/// A device set ("combined light"), e.g. a ceiling fixture with several bulbs.
/// The hub lists it on each member device under `deviceSet`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetRef {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    #[serde(default)]
    pub device_type: String,
    #[serde(default)]
    pub is_reachable: bool,
    #[serde(default)]
    pub room: Option<Room>,
    #[serde(default)]
    pub device_set: Option<Vec<SetRef>>,
    #[serde(default)]
    pub attributes: Map<String, Value>,
}

impl Device {
    pub fn attr(&self, k: &str) -> Option<&Value> {
        self.attributes.get(k)
    }
    pub fn b(&self, k: &str) -> Option<bool> {
        self.attr(k)?.as_bool()
    }
    pub fn n(&self, k: &str) -> Option<f64> {
        self.attr(k)?.as_f64()
    }
    pub fn s(&self, k: &str) -> Option<&str> {
        self.attr(k)?.as_str()
    }
    pub fn name(&self) -> String {
        [self.s("customName"), self.s("model")]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
            .unwrap_or(&self.device_type)
            .to_string()
    }
    /// The set this device belongs to, if any.
    pub fn set(&self) -> Option<&SetRef> {
        self.device_set.as_ref()?.first()
    }
    pub fn room_name(&self) -> &str {
        self.room.as_ref().map(|r| r.name.as_str()).unwrap_or("—")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SceneInfo {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scene {
    pub id: String,
    pub info: SceneInfo,
}

pub struct Client {
    http: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl Client {
    pub fn new(host: &str, token: &str) -> Result<Self> {
        Ok(Self {
            http: http()?,
            base: base_url(host),
            token: token.to_string(),
        })
    }

    fn check(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("401 Unauthorized — token invalid, re-run `dirigera-tui pair <ip>`");
        }
        Ok(resp.error_for_status()?)
    }

    /// Fetch a list, skipping entries that don't match the expected shape.
    fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()?;
        let items: Vec<Value> = Self::check(resp)?.json()?;
        Ok(items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect())
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        self.list("/devices")
    }

    pub fn scenes(&self) -> Result<Vec<Scene>> {
        self.list("/scenes")
    }

    pub fn set_attributes(&self, id: &str, attrs: &Map<String, Value>) -> Result<()> {
        let resp = self
            .http
            .patch(format!("{}/devices/{id}", self.base))
            .bearer_auth(&self.token)
            .json(&json!([{ "attributes": attrs }]))
            .send()?;
        Self::check(resp)?;
        Ok(())
    }

    pub fn trigger_scene(&self, id: &str) -> Result<()> {
        let resp = self
            .http
            .post(format!("{}/scenes/{id}/trigger", self.base))
            .bearer_auth(&self.token)
            .send()?;
        Self::check(resp)?;
        Ok(())
    }
}
