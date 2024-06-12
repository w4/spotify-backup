mod authentication;

use anyhow::{Context, Result};
use clap::Parser;
use hyper::HeaderMap;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
pub enum Args {
    /// Prints playlist to stdout as JSON
    Playlist {
        /// Playlist ID (eg. 3cEYpjA9oz9GiPac4AsH4n)
        id: String,
    },
    /// Prints liked songs to stdout as JSON
    Liked,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let token = authentication::authenticate()
        .await
        .context("Failed to authenticate with Spotify API")?;

    let mut headers = HeaderMap::new();
    headers.insert("Authorization", format!("Bearer {token}").parse()?);

    let client = reqwest::ClientBuilder::default()
        .default_headers(headers)
        .build()?;
    let mut next_url = Some(match args {
        Args::Playlist { id } => {
            format!("https://api.spotify.com/v1/playlists/{id}/tracks?offset=0&limit=50")
        }
        Args::Liked => "https://api.spotify.com/v1/me/tracks?offset=0&limit=50".to_string(),
    });

    let mut out = Vec::new();

    while let Some(curr_url) = next_url.take() {
        eprintln!("Fetching {curr_url}...");

        let data: GetPlaylistTracksResponse = client
            .get(curr_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        out.extend(data.items.into_iter().map(|v| {
            Output {
                album: OutputAlbum {
                    art: v
                        .track
                        .album
                        .images
                        .first()
                        .map(|v| v.url.to_string())
                        .unwrap_or_default(),
                    name: v.track.album.name,
                },
                name: v.track.name,
                artists: v.track.artists.into_iter().map(|v| v.name).collect(),
                uri: v.track.uri,
            }
        }));

        next_url = data.next;
    }

    println!("{}", serde_json::to_string(&out)?);

    Ok(())
}

#[derive(Serialize)]
pub struct Output {
    album: OutputAlbum,
    name: String,
    artists: Vec<String>,
    uri: String,
}

#[derive(Serialize)]
pub struct OutputAlbum {
    art: String,
    name: String,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponse {
    next: Option<String>,
    items: Vec<GetPlaylistTracksResponseItem>,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponseItem {
    track: GetPlaylistTracksResponseItemTrack,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponseItemTrack {
    artists: Vec<GetPlaylistTracksResponseItemTrackArtist>,
    name: String,
    album: GetPlaylistTracksResponseItemTrackAlbum,
    uri: String,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponseItemTrackAlbum {
    images: Vec<GetPlaylistTracksResponseItemTrackAlbumImage>,
    name: String,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponseItemTrackAlbumImage {
    url: String,
}

#[derive(Deserialize, Debug)]
pub struct GetPlaylistTracksResponseItemTrackArtist {
    name: String,
}
