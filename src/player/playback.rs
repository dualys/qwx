use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Repeat {
    #[default]
    Off,
    Track,
    Playlist,
}

impl Repeat {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Off => Self::Track,
            Self::Track => Self::Playlist,
            Self::Playlist => Self::Off,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Track => "track",
            Self::Playlist => "playlist",
        }
    }
}

pub(super) enum Command {
    Play {
        path: PathBuf,
        tracks: Vec<PathBuf>,
        playlist: bool,
    },
    Playlist(Vec<PathBuf>),
    Tree(Vec<PathBuf>),
    Next,
    Previous,
    TogglePause,
    Volume(u32),
    Repeat(Repeat),
}

pub(super) enum Event {
    Playing(PathBuf),
    Finished,
    Status(String),
    Error(String),
}

pub(super) struct Playback {
    pub(super) sender: Sender<Command>,
    pub(super) state: Arc<Mutex<State>>,
    pub(super) thread: thread::JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct State {
    pub(super) revision: u64,
    pub(super) current: Option<PathBuf>,
    pub(super) status: Option<String>,
}

fn report(state: &Mutex<State>, event: Event) -> bool {
    let Ok(mut state) = state.lock() else {
        return false;
    };
    match event {
        Event::Playing(path) => {
            let name = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            state.status = Some(format!("Playing: {name}"));
            state.current = Some(path);
        }
        Event::Finished => {
            state.current = None;
            state.status = Some("Playback finished.".to_string());
        }
        Event::Status(message) | Event::Error(message) => state.status = Some(message),
    }
    state.revision = state.revision.wrapping_add(1);
    true
}

struct Audio {
    sink: rodio::Sink,
    _stream: rodio::OutputStream,
}

impl Audio {
    fn open(path: &Path, volume: u32) -> Result<Self, String> {
        let file = File::open(path).map_err(|error| error.to_string())?;
        let source =
            rodio::Decoder::new(BufReader::new(file)).map_err(|error| error.to_string())?;
        let (stream, handle) =
            rodio::OutputStream::try_default().map_err(|error| error.to_string())?;
        let sink = rodio::Sink::try_new(&handle).map_err(|error| error.to_string())?;
        sink.set_volume(volume as f32 / 100.0);
        sink.append(source);
        Ok(Self {
            sink,
            _stream: stream,
        })
    }
}

fn neighbor(tracks: &[PathBuf], current: Option<&Path>, next: bool, wrap: bool) -> Option<PathBuf> {
    if tracks.is_empty() {
        return None;
    }
    let index = current.and_then(|path| tracks.iter().position(|track| track == path));
    let target = match (index, next) {
        (Some(index), true) if index + 1 < tracks.len() => Some(index + 1),
        (Some(index), false) if index > 0 => Some(index - 1),
        (_, true) if index.is_none() || wrap => Some(0),
        (_, false) if wrap => Some(tracks.len() - 1),
        _ => None,
    };
    target.map(|index| tracks[index].clone())
}

fn completion_target(
    tracks: &[PathBuf],
    current: Option<&Path>,
    playlist: bool,
    repeat: Repeat,
) -> Option<PathBuf> {
    match repeat {
        Repeat::Track => current.map(Path::to_path_buf),
        Repeat::Playlist if playlist => neighbor(tracks, current, true, true),
        _ if !playlist && !current.is_some_and(|path| tracks.iter().any(|track| track == path)) => {
            None
        }
        _ => neighbor(tracks, current, true, false),
    }
}

impl Playback {
    pub(super) fn start(volume: u32, repeat: Repeat) -> std::io::Result<Self> {
        let (sender, commands) = mpsc::channel();
        let state = Arc::new(Mutex::new(State::default()));
        let worker_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("qwx-audio".to_string())
            .spawn(move || {
                let mut audio: Option<Audio> = None;
                let mut current: Option<PathBuf> = None;
                let mut tracks = Vec::new();
                let mut in_playlist = false;
                let mut volume = volume;
                let mut repeat = repeat;

                loop {
                    let mut requested = None;
                    let mut automatic = false;
                    match commands.recv_timeout(Duration::from_millis(100)) {
                        Ok(Command::Play {
                            path,
                            tracks: new_tracks,
                            playlist,
                        }) => {
                            requested = Some((path, Some((new_tracks, playlist))));
                        }
                        Ok(Command::Playlist(new_tracks)) if in_playlist => tracks = new_tracks,
                        Ok(Command::Playlist(_)) => {}
                        Ok(Command::Tree(new_tracks)) if !in_playlist => tracks = new_tracks,
                        Ok(Command::Tree(_)) => {}
                        Ok(command @ (Command::Next | Command::Previous)) => {
                            let next = matches!(command, Command::Next);
                            if let Some(path) = neighbor(
                                &tracks,
                                current.as_deref(),
                                next,
                                in_playlist && repeat == Repeat::Playlist,
                            ) {
                                requested = Some((path, None));
                            } else {
                                let message = if next {
                                    "No next track."
                                } else {
                                    "No previous track."
                                };
                                if !report(&worker_state, Event::Status(message.to_string())) {
                                    break;
                                }
                            }
                        }
                        Ok(Command::TogglePause) => {
                            if let Some(audio) = &audio {
                                let message = if audio.sink.is_paused() {
                                    audio.sink.play();
                                    "Playback resumed."
                                } else {
                                    audio.sink.pause();
                                    "Playback paused."
                                };
                                if !report(&worker_state, Event::Status(message.to_string())) {
                                    break;
                                }
                            } else {
                                if !report(
                                    &worker_state,
                                    Event::Status("Select a track to play.".to_string()),
                                ) {
                                    break;
                                }
                            }
                        }
                        Ok(Command::Volume(new_volume)) => {
                            volume = new_volume;
                            if let Some(audio) = &audio {
                                audio.sink.set_volume(volume as f32 / 100.0);
                            }
                            if !report(&worker_state, Event::Status(format!("Volume: {volume}%"))) {
                                break;
                            }
                        }
                        Ok(Command::Repeat(new_repeat)) => repeat = new_repeat,
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if audio
                                .as_ref()
                                .is_some_and(|audio| audio.sink.empty() && !audio.sink.is_paused())
                            {
                                let next = completion_target(
                                    &tracks,
                                    current.as_deref(),
                                    in_playlist,
                                    repeat,
                                );
                                if let Some(path) = next {
                                    requested = Some((path, None));
                                    automatic = true;
                                } else {
                                    audio = None;
                                    current = None;
                                    if !report(&worker_state, Event::Finished) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if let Some((path, new_context)) = requested {
                        match Audio::open(&path, volume) {
                            Ok(new_audio) => {
                                if let Some(old) = &audio {
                                    old.sink.stop();
                                }
                                audio = Some(new_audio);
                                current = Some(path.clone());
                                if let Some((new_tracks, playlist)) = new_context {
                                    tracks = new_tracks;
                                    in_playlist = playlist;
                                }
                                if !report(&worker_state, Event::Playing(path)) {
                                    break;
                                }
                            }
                            Err(error) => {
                                if automatic {
                                    audio = None;
                                    current = None;
                                    if !report(&worker_state, Event::Finished) {
                                        break;
                                    }
                                }
                                if !report(
                                    &worker_state,
                                    Event::Error(format!("Local play error: {error}")),
                                ) {
                                    break;
                                }
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            sender,
            state,
            thread,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbor_handles_boundaries_and_repeat() {
        let tracks = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(
            neighbor(&tracks, Some(Path::new("a")), true, false),
            Some(tracks[1].clone())
        );
        assert_eq!(neighbor(&tracks, Some(Path::new("b")), true, false), None);
        assert_eq!(
            neighbor(&tracks, Some(Path::new("b")), true, true),
            Some(tracks[0].clone())
        );
        assert_eq!(
            neighbor(&tracks, Some(Path::new("a")), false, true),
            Some(tracks[1].clone())
        );
        assert_eq!(neighbor(&[], None, true, true), None);
    }

    #[test]
    fn completion_advances_and_repeats_the_requested_context() {
        let tracks = vec![PathBuf::from("a"), PathBuf::from("b")];
        let last = Some(Path::new("b"));
        assert_eq!(completion_target(&tracks, last, true, Repeat::Off), None);
        assert_eq!(
            completion_target(&tracks, last, true, Repeat::Track),
            Some(tracks[1].clone())
        );
        assert_eq!(
            completion_target(&tracks, last, true, Repeat::Playlist),
            Some(tracks[0].clone())
        );
        assert_eq!(
            completion_target(&tracks, Some(Path::new("a")), true, Repeat::Off),
            Some(tracks[1].clone())
        );
        assert_eq!(
            completion_target(&tracks, last, false, Repeat::Playlist),
            None
        );
        assert_eq!(
            completion_target(&tracks, Some(Path::new("a")), false, Repeat::Off),
            Some(tracks[1].clone())
        );
        assert_eq!(
            completion_target(&tracks, Some(Path::new("missing")), false, Repeat::Off),
            None
        );
        assert_eq!(completion_target(&tracks, last, false, Repeat::Off), None);
    }
}
