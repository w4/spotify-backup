use std::{
    collections::HashMap,
    io::ErrorKind,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL_SAFE_NO_PAD, Engine};
use http_body_util::Full;
use hyper::{
    body::{self, Bytes},
    server::conn::http1,
    service::service_fn,
    Method, Request, StatusCode,
};
use hyper_util::rt::TokioIo;
use rand::distributions::{Alphanumeric, DistString};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{digest::FixedOutput, Digest, Sha256};
use tokio::net::TcpListener;

const AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SCOPES: &str = "playlist-read-private user-library-read";
const CLIENT_ID: &str = "b6146c081df54ae79e42258a8619f570";

pub async fn authenticate() -> Result<String> {
    let access_token = match read_token_state().await? {
        CurrentTokenState::Expired(refresh_token) => {
            fetch_access_token_from_refresh(&refresh_token).await?
        }
        CurrentTokenState::Valid(token) => token,
        CurrentTokenState::Missing => fetch_fresh_access_token().await?,
    };

    tokio::fs::create_dir_all(build_state_dir_path()?).await?;

    let serialized_state =
        serde_json::to_string(&access_token).context("Failed to serialize token state")?;
    tokio::fs::write(build_token_state_path()?, serialized_state)
        .await
        .context("Failed to write token state")?;

    Ok(access_token.access_token)
}

async fn read_token_state() -> Result<CurrentTokenState> {
    let data = match tokio::fs::read(build_token_state_path()?).await {
        Ok(v) => v,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(CurrentTokenState::Missing),
        Err(e) => return Err(e).context("Failed to read token state"),
    };

    let data: TokenState = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to read token state ({e}), invalidating...");
            return Ok(CurrentTokenState::Missing);
        }
    };

    let current_timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("the end of days is nigh")?
        .as_secs();

    if data.expires_at < (current_timestamp + 300) {
        Ok(CurrentTokenState::Expired(data.refresh_token))
    } else {
        Ok(CurrentTokenState::Valid(data))
    }
}

fn build_token_state_path() -> Result<PathBuf> {
    Ok(build_state_dir_path()?.join("token.json"))
}

fn build_state_dir_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir().context("Unsupported operating system, no data dir")?;
    Ok(base.join("spotify-backup"))
}

async fn fetch_access_token_from_refresh(refresh_token: &str) -> Result<TokenState> {
    eprintln!("Refreshing token...");

    let mut params = HashMap::new();
    params.insert("grant_type", "refresh_token");
    params.insert("refresh_token", refresh_token);
    params.insert("client_id", CLIENT_ID);

    reqwest::Client::default()
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("Failed to send access token request")?
        .error_for_status()
        .context("Got non-200 response when requesting access token")?
        .json::<AccessTokenResponse>()
        .await
        .context("Failed to deserialize access token response")?
        .try_into()
        .context("Failed to convert to internal state")
}

async fn fetch_fresh_access_token() -> Result<TokenState> {
    let tcp_listener = TcpListener::bind("127.0.0.1:8888")
        .await
        .context("Failed to open TCP listener")?;
    let local_addr = tcp_listener
        .local_addr()
        .context("Failed to read local socket address")?;

    let redirect_url = format!("http://{local_addr}/");

    let (code_verifier, code_challenge) = generate_code_challenge();

    eprintln!("Opening Spotify for authentication...");
    webbrowser::open(build_spotify_auth_url(&code_challenge, &redirect_url)?.as_str())
        .context("Failed to open browser")?;

    eprintln!("Waiting for callback...");
    let code = spawn_http_server_wait_for_callback(tcp_listener)
        .await
        .context("Failed to wait for callback")?;
    eprintln!("Successfully received Spotify callback, fetching access token...");

    fetch_access_token(&code, &code_verifier, &redirect_url)
        .await
        .context("Failed to fetch access token")?
        .try_into()
        .context("Failed to convert to internal state")
}

async fn fetch_access_token(
    code: &str,
    code_verifier: &str,
    redirect_url: &str,
) -> Result<AccessTokenResponse> {
    let mut params = HashMap::new();
    params.insert("grant_type", "authorization_code");
    params.insert("code", code);
    params.insert("redirect_uri", redirect_url);
    params.insert("client_id", CLIENT_ID);
    params.insert("code_verifier", code_verifier);

    let resp = reqwest::Client::default()
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("Failed to send access token request")?
        .error_for_status()
        .context("Got non-200 response when requesting access token")?
        .json()
        .await
        .context("Failed to deserialize access token response")?;

    Ok(resp)
}

async fn spawn_http_server_wait_for_callback(tcp_listener: TcpListener) -> Result<String> {
    let mut http = http1::Builder::new();
    http.keep_alive(false);

    loop {
        let (stream, _) = tcp_listener
            .accept()
            .await
            .context("Failed to accept TCP connection")?;

        let out = Mutex::new(None);

        let out2 = &out;
        let service = service_fn(|req: Request<body::Incoming>| async move {
            let (Method::GET, "/", Some(query)) =
                (req.method().clone(), req.uri().path(), req.uri().query())
            else {
                let mut resp = hyper::Response::new(Full::<Bytes>::from(
                    "Invalid request, bad method/path/query params",
                ));
                *resp.status_mut() = StatusCode::NOT_FOUND;
                return Ok(resp);
            };

            let Some((_, value)) =
                form_urlencoded::parse(query.as_bytes()).find(|(key, _value)| key == "code")
            else {
                let mut resp = hyper::Response::new(Full::<Bytes>::from(
                    "Invalid request, missing code query parameter",
                ));
                *resp.status_mut() = StatusCode::NOT_FOUND;
                return Ok(resp);
            };

            *out2.lock().unwrap() = Some(value.into_owned());

            Ok::<_, anyhow::Error>(hyper::Response::new(Full::<Bytes>::from(
                "Successfully authenticated, please return to your terminal",
            )))
        });

        if let Err(e) = http.serve_connection(TokioIo::new(stream), service).await {
            eprintln!("Failed to serve HTTP request: {e}");
        }

        let Some(v) = out.lock().unwrap().take() else {
            continue;
        };

        break Ok(v);
    }
}

fn build_spotify_auth_url(code_challenge: &str, redirect_url: &str) -> Result<Url> {
    let mut base = Url::parse(AUTH_URL).context("Failed to parse base URL")?;

    base.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", code_challenge)
        .append_pair("redirect_uri", redirect_url);

    Ok(base)
}

fn generate_code_challenge() -> (String, String) {
    let code_verifier = Alphanumeric.sample_string(&mut rand::thread_rng(), 128);
    let code_hashed = Sha256::default()
        .chain_update(code_verifier.as_bytes())
        .finalize_fixed();
    let code_challenge = BASE64_URL_SAFE_NO_PAD.encode(code_hashed);

    (code_verifier, code_challenge)
}

pub enum CurrentTokenState {
    Expired(String),
    Valid(TokenState),
    Missing,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TokenState {
    access_token: String,
    expires_at: u64,
    refresh_token: String,
}

#[derive(Deserialize)]
pub struct AccessTokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: String,
}

impl TryFrom<AccessTokenResponse> for TokenState {
    type Error = anyhow::Error;

    fn try_from(
        AccessTokenResponse {
            access_token,
            expires_in,
            refresh_token,
        }: AccessTokenResponse,
    ) -> Result<Self> {
        let expires_at = (SystemTime::now() + Duration::from_secs(expires_in))
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("the end of days in nigh")?
            .as_secs();

        Ok(TokenState {
            access_token,
            refresh_token,
            expires_at,
        })
    }
}
