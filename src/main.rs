#![deny(clippy::all, clippy::pedantic)]
use anyhow::Result;
use mpris::{Metadata, Player, PlayerFinder};
use rustfm_scrobble::{Scrobble, Scrobbler};
use std::sync::{Arc, Mutex};
use std::{
    convert::TryFrom,
    io::Read,
    thread::sleep,
    time::{Duration, Instant},
};
use thiserror::Error;

const LAST_FM_API_KEY: &str = "401615b0bba90b796964290b7c9ecc36";
const LAST_FM_API_SECRET: &str = "353a68a2d4dfa9a0378e01be16efbaf5";

/// Interval to push backed up scrobbles.
const PUSH_QUEUE_INTERVAL: Duration = Duration::from_secs(60);

/// Amount of time to sleep in between checking for an active player.
const WAIT_FOR_PLAYER_TIME: Duration = Duration::from_secs(5);

/// Amount of time to sleep whilst watching for events from an active player.
const LOOP_TIME: Duration = Duration::from_secs(1);

/// Amount of time to wait before scrobbling a track.
const SCROBBLE_THRESHOLD: Duration = Duration::from_secs(10);

lazy_static::lazy_static! {
    static ref STORAGE_DIR: std::path::PathBuf = {
        let mut path = dirs::data_local_dir().unwrap();
        path.push("dobble");
        path
    };

    static ref SCROBBLE_QUEUE: Mutex<Vec<Track>> = Mutex::<Vec<Track>>::default();
}

#[derive(Error, Debug)]
enum Error {
    #[error("missing metadata field {}", .0)]
    MissingMetadata(&'static str),
}

#[derive(Clone, Debug, Eq)]
struct Track {
    artist: String,
    track: String,
    album: String,
    scrobbled: bool,
    playing_for: Duration,
}

impl Track {
    pub fn as_scrobble(&self) -> Scrobble {
        Scrobble::new(&self.artist, &self.track, &self.album)
    }
}

impl PartialEq<Track> for Track {
    fn eq(&self, other: &Track) -> bool {
        self.track == other.track && self.artist == other.artist
    }
}

impl TryFrom<Metadata> for Track {
    type Error = anyhow::Error;

    fn try_from(metadata: Metadata) -> Result<Self> {
        let mut track = metadata.title().ok_or(Error::MissingMetadata("title"))?;

        let mut artist = metadata
            .artists()
            .map_or_else(|| "".to_string(), |v| v.join(", "));

        if artist == "" {
            let mut split = track.splitn(2, " - ");

            artist = match split.next() {
                Some(v) if v.starts_with("\u{25b6} ") => v["\u{25b6} ".len()..].to_string(), // quick fix for plex
                Some(v) => v.to_string(),
                None => return Err(Error::MissingMetadata("artist split from title").into()),
            };

            track = split
                .next()
                .ok_or(Error::MissingMetadata("artist split from title"))?;
        }

        Ok(Self {
            track: track.to_string(),
            artist,
            album: metadata.album_name().unwrap_or("").to_string(),
            scrobbled: false,
            playing_for: Duration::from_secs(0),
        })
    }
}

/// Blocks while waiting for a player.
fn get_player(finder: &PlayerFinder) -> Player {
    loop {
        if let Ok(player) = finder.find_active() {
            return player;
        } else {
            sleep(WAIT_FOR_PLAYER_TIME);
        }
    }
}

/// Sets the given track as now playing
fn now_playing(scrobbler: &Scrobbler, track: &Track) -> Result<()> {
    scrobbler.now_playing(&track.as_scrobble())?;
    Ok(())
}

/// Scrobbles the given track or places it in the queue if scrobbling failed.
fn scrobble(scrobbler: &Scrobbler, track: &Track) {
    if let Err(e) = scrobbler.scrobble(&track.as_scrobble()) {
        // scrobbling failed, lets queue it for later
        eprintln!("Failed to scrobble track, adding to queue: {:?}", e);
        SCROBBLE_QUEUE.lock().unwrap().push(track.clone());
    }
}

/// Batches any queued scrobbles and pushes them to Last.fm
///
/// SCROBBLE_QUEUE will be locked while pushing to Last.fm, so any scrobbles
/// that need to be placed in the queue (ie. due to bad network conditions)
/// will be blocked, and may possibly be lost if the push takes longer than
/// the length of the track.
fn push_queued_scrobbles(scrobbler: Arc<Scrobbler>) {
    let should_run = !SCROBBLE_QUEUE.lock().unwrap().is_empty();

    if should_run {
        std::thread::spawn(move || {
            let mut queue = SCROBBLE_QUEUE.lock().unwrap();

            if queue.len() == 1 {
                if let Some(track) = queue.get(0) {
                    match scrobbler.scrobble(&track.as_scrobble()) {
                        Ok(_) => queue.clear(),
                        Err(e) => eprintln!("Failed to push queued track: {}", e),
                    }
                }
            } else {
                let batch = queue
                    .iter()
                    .map(Track::as_scrobble)
                    .collect::<Vec<Scrobble>>()
                    .into();

                match scrobbler.scrobble_batch(&batch) {
                    Ok(_) => queue.clear(),
                    Err(e) => eprintln!("Failed to push queued batch: {}", e),
                }
            }
        });
    }
}

#[derive(serde::Deserialize)]
struct AuthToken {
    token: String,
}

fn authenticate_lastfm(scrobbler: &mut Scrobbler) -> Result<()> {
    let key_file = STORAGE_DIR.join("session-key");

    // if the key file exists authenticate with that
    if let Ok(key) = std::fs::read_to_string(&key_file) {
        scrobbler.authenticate_with_session_key(&key);
        return Ok(());
    }

    // get a token from last.fm and ask the user to authenticate with it
    let token: AuthToken = reqwest::blocking::get(&format!(
        "https://ws.audioscrobbler.com/2.0/?method=auth.gettoken&format=json&api_key={}",
        LAST_FM_API_KEY
    ))?
    .json()?;
    println!("Please visit the following link and hit any key once allowed: http://www.last.fm/api/auth/?api_key={}&token={}", LAST_FM_API_KEY, token.token);
    std::io::stdin().read_exact(&mut [0])?;

    // authenticate using the token and write it to the key
    let session = scrobbler.authenticate_with_token(&token.token)?;
    std::fs::write(&key_file, session.key)?;

    println!(
        "Successfully authenticated with the Last.fm API and saved credentials to {}",
        key_file.display()
    );

    Ok(())
}

fn main() {
    if let Err(e) = std::fs::create_dir_all(&*STORAGE_DIR) {
        eprintln!(
            "Failed to create storage directory {}: {}",
            STORAGE_DIR.display(),
            e
        );
        std::process::exit(1);
    }

    let mut scrobbler = Scrobbler::new(LAST_FM_API_KEY, LAST_FM_API_SECRET);
    if let Err(e) = authenticate_lastfm(&mut scrobbler) {
        eprintln!("Failed to authenticate to Last.fm: {}", e);
        std::process::exit(1);
    }
    let scrobbler = Arc::new(scrobbler);

    let player_finder = PlayerFinder::new().expect("Could not connect to D-Bus");
    let mut player = get_player(&player_finder);

    let mut tune: Option<Track> = None;

    let mut last_check = Instant::now();
    let mut last_pushed_queue = Instant::now();

    loop {
        sleep(LOOP_TIME);

        // push any scrobbles that have been queued every PUSH_QUEUE_INTERVAL
        if last_pushed_queue.elapsed() >= PUSH_QUEUE_INTERVAL {
            last_pushed_queue = Instant::now();
            push_queued_scrobbles(scrobbler.clone());
        }

        // calculate the time since the last iteration
        let now = Instant::now();
        let duration_since_last_check = now.duration_since(last_check);
        last_check = now;

        // replace the player if the current one disconnected
        if !player.is_running() {
            player = get_player(&player_finder);
            tune = None;
        }

        // skip to the next iteration
        match player.get_playback_status() {
            Ok(mpris::PlaybackStatus::Playing) => {}
            Ok(mpris::PlaybackStatus::Stopped) => {
                tune = None;
                continue;
            }
            _ => continue,
        }

        // collect track metadata
        let metadata = match player.get_metadata() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Failed to collect track metadata: {:?}", e);
                continue;
            }
        };

        // convert the currently playing song to a `Track`
        let currently_playing = match Track::try_from(metadata) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // if the current tune is the same one playing in the last iteration,
        // increment the time playing and maybe scrobble. otherwise, replace the
        // playing tune.
        match &mut tune {
            Some(tune) if *tune == currently_playing => {
                tune.playing_for += duration_since_last_check;

                if tune.playing_for >= SCROBBLE_THRESHOLD && !tune.scrobbled {
                    scrobble(&scrobbler, &tune);
                    tune.scrobbled = true;
                }
            }
            _ => {
                if let Err(e) = now_playing(&scrobbler, &currently_playing) {
                    eprintln!("Setting now playing failed: {}", e);
                }

                tune = Some(currently_playing);
            }
        }
    }
}
