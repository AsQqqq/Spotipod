//! Throwaway REPL for exercising spotipod-core. Not the real UI.

use anyhow::{Context, Result, anyhow, bail};
use spotipod_core::{Event, OutputDevice, Playlist, Repeat, Shuffle, Spotipod, Track};
use tokio::io::{AsyncBufReadExt, BufReader};

const PAGE: u32 = 20;

const HELP: &str = "\
auth:      login | logout | whoami | start
library:   playlists [page] | open <n> [page] | liked [page] | albums [page]
play:      play <n> | uri <spotify:uri> | likedsongs
control:   toggle | pause | resume | next | prev | seek <sec> | vol <0-100>
           now
settings:  settings | about | shuffle off|songs | repeat off|one|all | reset
output:    outputs | output <n>
other:     devices | help | quit
<n> is an index from the last printed list.";

/// What numbers in the last printed list refer to.
enum Listing {
    None,
    Playlists(Vec<Playlist>),
    Tracks {
        tracks: Vec<Track>,
        context: Option<String>,
    },
    Albums(Vec<(String, String)>),
    Outputs(Vec<OutputDevice>),
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,spotipod_core=info"),
    )
    .init();

    let mut sp = Spotipod::from_default_paths()?;
    println!("config: {}", sp.paths().config_file.display());

    spawn_event_printer(&sp);

    if sp.has_cached_credentials() {
        println!("cached credentials found, starting...");
        if let Err(e) = sp.start().await {
            println!("start failed: {e} (try `login`)");
        } else {
            println!("ready as {}", sp.username().unwrap_or_default());
        }
    } else {
        println!("not logged in, type `login`");
    }
    println!("type `help` for commands");

    let mut listing = Listing::None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        use std::io::Write;
        std::io::stdout().flush().ok();

        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, arg) = line
            .split_once(' ')
            .map_or((line, ""), |(c, a)| (c, a.trim()));
        if cmd == "quit" || cmd == "exit" {
            break;
        }
        let cmd = match cmd {
            "pl" | "playlist" => "playlists",
            "q" => "quit",
            c => c,
        };
        if cmd == "quit" {
            break;
        }
        if let Err(e) = run(&mut sp, &mut listing, cmd, arg).await {
            println!("error: {e:#}");
        }
    }

    sp.stop().await;
    Ok(())
}

fn spawn_event_printer(sp: &Spotipod) {
    let mut rx = sp.subscribe();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            match ev {
                Event::SpotifyLaunching => println!("\n[starting Spotify]"),
                Event::SpotifyReady(d) => println!("\n[controlling Spotify on {d}]"),
                Event::SpotifyMissing => println!("\n[Spotify app not found]"),
                Event::CommandFailed(e) => println!("\n[command failed] {e}"),
                other => log::debug!("{other:?}"),
            }
        }
    });
}

async fn run(sp: &mut Spotipod, listing: &mut Listing, cmd: &str, arg: &str) -> Result<()> {
    match cmd {
        "help" => println!("{HELP}"),

        "login" => {
            println!("opening browser for Spotify login...");
            sp.login(true).await?;
            println!("logged in as {}", sp.username().unwrap_or_default());
        }
        "start" => {
            sp.start().await?;
            println!("ready as {}", sp.username().unwrap_or_default());
        }
        "logout" => {
            sp.logout().await?;
            println!("logged out");
        }
        "whoami" => {
            println!(
                "{} (plan: {})",
                sp.username().unwrap_or_else(|| "not logged in".into()),
                sp.account_type().unwrap_or_else(|| "?".into())
            );
        }

        "playlists" => {
            let list = sp.library()?.playlists().await?;
            for (i, p) in list.iter().enumerate() {
                println!("{i:>3}. {} — {} [{}]", p.name, p.owner, p.track_count);
            }
            *listing = Listing::Playlists(list);
        }
        "open" => {
            let mut parts = arg.split_whitespace();
            let n = parse_index(parts.next().unwrap_or(""))?;
            let offset = page_offset(parts.next().unwrap_or(""))?;
            let (tracks, context) = match listing {
                Listing::Playlists(p) => {
                    let pl = p.get(n).ok_or_else(|| anyhow!("no such item"))?;
                    let page = sp.library()?.playlist_tracks(&pl.uri, offset, PAGE).await?;
                    (page.items, pl.uri.clone())
                }
                Listing::Albums(a) => {
                    let (_, uri) = a.get(n).ok_or_else(|| anyhow!("no such item"))?.clone();
                    let page = sp.library()?.album_tracks(&uri, offset, PAGE).await?;
                    (page.items, uri)
                }
                _ => bail!("list playlists or albums first"),
            };
            print_tracks(&tracks);
            *listing = Listing::Tracks {
                tracks,
                context: Some(context),
            };
        }
        "liked" => {
            let page = sp.library()?.liked_tracks(page_offset(arg)?, PAGE).await?;
            print_tracks(&page.items);
            *listing = Listing::Tracks {
                tracks: page.items,
                context: Some(sp.liked_songs_uri()?),
            };
        }
        "albums" => {
            let albums = sp.library()?.saved_albums().await?;
            for (i, a) in albums.iter().enumerate() {
                println!("{i:>3}. {} — {}", a.name, a.artists.join(", "));
            }
            *listing = Listing::Albums(albums.into_iter().map(|a| (a.id, a.uri)).collect());
        }
        "play" => {
            let n = parse_index(arg)?;
            let Listing::Tracks { tracks, context } = listing else {
                bail!("list tracks first");
            };
            let track = tracks.get(n).ok_or_else(|| anyhow!("no such item"))?;
            match context {
                Some(ctx) => sp.play_context(ctx, Some(&track.uri))?,
                None => {
                    let uris = tracks.iter().map(|t| t.uri.clone()).collect();
                    sp.play_tracks(uris, n as u32)?
                }
            }
        }
        "uri" => {
            if arg.starts_with("spotify:track:") {
                sp.play_tracks(vec![arg.to_owned()], 0)?
            } else {
                sp.play_context(arg, None)?
            }
        }
        "likedsongs" => sp.play_context(&sp.liked_songs_uri()?, None)?,

        "toggle" => sp.play_pause()?,
        "pause" => sp.pause()?,
        "resume" => sp.play()?,
        "next" => sp.next()?,
        "prev" => sp.prev()?,
        "seek" => {
            let sec: u32 = arg.parse().context("usage: seek <seconds>")?;
            sp.seek(sec * 1000)?
        }
        "vol" => {
            let v: u8 = arg.parse().context("usage: vol <0-100>")?;
            sp.set_volume(v.min(100))?
        }
        "repeat" => {
            let mut settings = sp.settings();
            settings.repeat = match arg {
                "one" => Repeat::Track,
                "all" => Repeat::Context,
                _ => Repeat::Off,
            };
            sp.set_settings(settings)?;
        }
        "shuffle" => {
            let mut settings = sp.settings();
            settings.shuffle = if arg == "songs" {
                Shuffle::Songs
            } else {
                Shuffle::Off
            };
            sp.set_settings(settings)?;
        }
        "about" => {
            let a = sp.about()?.await?;
            println!(
                "songs: {} | company: {} | author: {} | version: {}",
                a.songs, a.company, a.author, a.version
            );
        }
        "settings" => println!("{:?}", sp.settings()),
        "reset" => sp.reset_settings()?,
        "now" => {
            let s = sp.state();
            match &s.track {
                Some(t) => println!(
                    "{} | {:?} {}/{} | vol {}% | repeat {:?} | on {}",
                    fmt_track(t),
                    s.status,
                    fmt_ms(s.current_position_ms()),
                    fmt_ms(t.duration_ms),
                    s.volume,
                    s.repeat,
                    s.device.as_deref().unwrap_or("?")
                ),
                None => println!(
                    "nothing playing (Spotify: {})",
                    s.device.as_deref().unwrap_or("not found")
                ),
            }
        }
        "outputs" => {
            let devices = sp.output_devices()?;
            for (i, d) in devices.iter().enumerate() {
                let mark = if d.is_default { "*" } else { " " };
                println!("{mark}{i:>3}. {}", d.name);
            }
            *listing = Listing::Outputs(devices);
        }
        "output" => {
            let Listing::Outputs(devices) = listing else {
                bail!("run `outputs` first");
            };
            let d = devices
                .get(parse_index(arg)?)
                .ok_or_else(|| anyhow!("no such item"))?;
            sp.set_output_device(&d.id)?;
            println!("output: {}", d.name);
        }

        "devices" => {
            for d in sp.devices()? {
                let active = if d.is_active { "*" } else { " " };
                println!("{active} {} [{}]", d.name, d.kind);
            }
        }

        _ => println!("unknown command, type `help`"),
    }
    Ok(())
}

fn parse_index(s: &str) -> Result<usize> {
    s.parse().context("expected an item number")
}

/// Page number (from 1) -> offset.
fn page_offset(arg: &str) -> Result<u32> {
    if arg.is_empty() {
        return Ok(0);
    }
    let page: u32 = arg.parse().context("expected a page number")?;
    Ok(page.saturating_sub(1) * PAGE)
}

fn print_tracks(tracks: &[Track]) {
    for (i, t) in tracks.iter().enumerate() {
        println!("{i:>3}. {} [{}]", fmt_track(t), fmt_ms(t.duration_ms));
    }
}

fn fmt_track(t: &Track) -> String {
    format!("{} — {}", t.artists.join(", "), t.name)
}

fn fmt_ms(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
