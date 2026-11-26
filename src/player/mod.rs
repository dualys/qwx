//! Local music player and background library indexing.

mod playback;

use self::playback::{Command as PlaybackCommand, Playback, Repeat};
use blake3::Hasher;
use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{KeyCode, KeyModifiers, MediaKeyCode};
use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use kpack::{Opcode, execute};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, TryRecvError},
};
use std::thread;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use walkdir::WalkDir;

#[derive(Clone, Deserialize, Serialize)]
pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub children: Vec<TreeNode>,
    pub expanded: bool,
    pub is_favorite: bool,
    pub track_count: usize,
}

impl TreeNode {
    pub fn new(name: String, path: PathBuf, is_dir: bool) -> Self {
        Self {
            name,
            path,
            is_dir,
            children: Vec::new(),
            expanded: false,
            is_favorite: false,
            track_count: 0,
        }
    }

    fn find_mut(&mut self, path: &Path) -> Option<&mut Self> {
        if self.path == path {
            return Some(self);
        }
        self.children
            .iter_mut()
            .find_map(|child| child.find_mut(path))
    }

    fn collapse_all(&mut self) {
        self.expanded = false;
        for child in &mut self.children {
            child.collapse_all();
        }
    }
}

pub(crate) struct LocalLibrary {
    tree: TreeNode,
    tracks: Vec<PathBuf>,
    notice: Option<String>,
}

pub(crate) enum PlayerLoadEvent {
    Ready,
    Playlist(io::Result<(PathBuf, Vec<PathBuf>)>),
    Library(io::Result<LocalLibrary>),
}

pub(crate) struct PlayerLoader {
    receiver: Receiver<PlayerLoadEvent>,
    cancelled: Arc<AtomicBool>,
}

impl PlayerLoader {
    pub(crate) fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        thread::Builder::new()
            .name("qwx-player-loader".to_string())
            .spawn(move || {
                if sender.send(PlayerLoadEvent::Ready).is_err()
                    || worker_cancelled.load(Ordering::Relaxed)
                {
                    return;
                }
                let playlist = dirs::config_dir()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "Config directory not found")
                    })
                    .and_then(|dir| {
                        let path = dir.join("qwx").join("playlist.json");
                        load_playlist(&path).map(|tracks| (path, tracks))
                    });
                if sender.send(PlayerLoadEvent::Playlist(playlist)).is_err()
                    || worker_cancelled.load(Ordering::Relaxed)
                {
                    return;
                }
                let result = match dirs::audio_dir().or_else(dirs::home_dir) {
                    Some(dir) => {
                        let cache_dir = dirs::config_dir().map(|dir| dir.join("qwx"));
                        MusicPlayer::scan_library(&dir, cache_dir.as_deref(), &worker_cancelled)
                    }
                    None => Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "Music directory not found",
                    )),
                };
                let _ = sender.send(PlayerLoadEvent::Library(result));
            })?;
        Ok(Self {
            receiver,
            cancelled,
        })
    }

    pub(crate) fn try_recv(&self) -> Result<PlayerLoadEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for PlayerLoader {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn load_playlist(path: &Path) -> io::Result<Vec<PathBuf>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    serde_json::from_reader(file).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn save_playlist(path: &Path, tracks: &[PathBuf]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("playlist has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temp, tracks).map_err(io::Error::other)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Clone)]
struct VisibleNode {
    name: String,
    path: PathBuf,
    depth: usize,
    is_dir: bool,
    expanded: bool,
    is_favorite: bool,
    track_count: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlayerView {
    Tree,
    Playlist,
}

pub struct MusicPlayer {
    playback: Option<Playback>,
    playback_revision: u64,
    current_path: Option<PathBuf>,
    volume: u32,
    repeat: Repeat,
    view: PlayerView,
    playlist: Vec<PathBuf>,
    playlist_path: Option<PathBuf>,
    playlist_error: Option<String>,
    tree: Option<TreeNode>,
    tracks: Vec<PathBuf>,
    visible: Vec<VisibleNode>,
    cursor: usize,
    scroll: usize,
    filter: String,
    editing_filter: bool,
    status_message: Option<String>,
}

impl Default for MusicPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl MusicPlayer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            playback: None,
            playback_revision: 0,
            current_path: None,
            volume: 100,
            repeat: Repeat::Off,
            view: PlayerView::Tree,
            playlist: Vec::new(),
            playlist_path: None,
            playlist_error: None,
            tree: None,
            tracks: Vec::new(),
            visible: Vec::new(),
            cursor: 0,
            scroll: 0,
            filter: String::new(),
            editing_filter: false,
            status_message: Some("Loading music library...".to_string()),
        }
    }

    fn is_audio(path: &Path) -> bool {
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                matches!(
                    ext.to_lowercase().as_str(),
                    "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac"
                )
            })
            .unwrap_or(false)
    }

    fn cache_path(cache_dir: &Path, fingerprint: &[u8; 32]) -> PathBuf {
        let hex: String = fingerprint
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        cache_dir.join(format!("music-tree-{hex}.kpack"))
    }

    fn save_tree_kpack(tree: &TreeNode, path: &Path) -> io::Result<()> {
        let bytes = bincode::serialize(tree).map_err(io::Error::other)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache has no parent"))?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(b"QWX2")?;
        for chunk in bytes.chunks(u16::MAX as usize) {
            temp.write_all(&(chunk.len() as u16).to_le_bytes())?;
            temp.write_all(chunk)?;
        }
        temp.flush()?;
        temp.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    fn load_tree_kpack(path: &Path) -> io::Result<Option<TreeNode>> {
        let recipe = match fs::read(path) {
            Ok(recipe) => recipe,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut rest = recipe.strip_prefix(b"QWX2").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid music cache header")
        })?;
        let mut decoded = Vec::new();
        while !rest.is_empty() {
            if rest.len() < 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated music cache",
                ));
            }
            let len = u16::from_le_bytes([rest[0], rest[1]]) as usize;
            rest = &rest[2..];
            if len == 0 || rest.len() < len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid music cache chunk",
                ));
            }
            let start = decoded.len();
            decoded.resize(start + len, 0);
            execute(
                &Opcode::Lit,
                len as u32,
                &rest[..len],
                &mut decoded[start..],
                None,
            );
            rest = &rest[len..];
        }
        bincode::deserialize(&decoded)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn scan_library(
        dir: &Path,
        cache_dir: Option<&Path>,
        cancelled: &AtomicBool,
    ) -> io::Result<LocalLibrary> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "music scan cancelled",
            ));
        }
        let root = dir.canonicalize()?;
        let name = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
        let mut root_node = TreeNode::new(name, root.clone(), true);
        root_node.expanded = true;
        let mut stack = vec![root_node];
        let mut tracks = Vec::new();
        let mut hasher = Hasher::new();
        hasher.update(root.to_string_lossy().as_bytes());

        for entry in WalkDir::new(&root).min_depth(1).sort_by_file_name() {
            if cancelled.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "music scan cancelled",
                ));
            }
            let entry = entry.map_err(io::Error::other)?;
            let path = entry.path();
            let is_dir = entry.file_type().is_dir();
            if !is_dir && !(entry.file_type().is_file() && Self::is_audio(path)) {
                continue;
            }
            while stack.len() > entry.depth() {
                let child = stack.pop().expect("non-root tree node");
                let parent = stack.last_mut().expect("root tree node");
                parent.track_count += child.track_count;
                parent.children.push(child);
            }
            let metadata = entry.metadata().map_err(io::Error::other)?;
            let path_text = path.to_string_lossy();
            hasher.update(&(path_text.len() as u64).to_le_bytes());
            hasher.update(path_text.as_bytes());
            hasher.update(&[u8::from(is_dir)]);
            hasher.update(&metadata.len().to_le_bytes());
            let timestamp = metadata
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?;
            hasher.update(&timestamp.as_nanos().to_le_bytes());
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir {
                stack.push(TreeNode::new(name, path.to_path_buf(), true));
            } else {
                let parent = stack.last_mut().expect("root tree node");
                parent.track_count += 1;
                parent
                    .children
                    .push(TreeNode::new(name, path.to_path_buf(), false));
                tracks.push(path.to_path_buf());
            }
        }
        while stack.len() > 1 {
            let child = stack.pop().expect("non-root tree node");
            let parent = stack.last_mut().expect("root tree node");
            parent.track_count += child.track_count;
            parent.children.push(child);
        }
        let mut tree = stack.pop().expect("root tree node");
        let mut notice = None;
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "music scan cancelled",
            ));
        }
        if let Some(cache_dir) = cache_dir {
            let path = Self::cache_path(cache_dir, hasher.finalize().as_bytes());
            match Self::load_tree_kpack(&path) {
                Ok(Some(cached)) => tree = cached,
                Ok(None) => {
                    if let Err(error) = Self::save_tree_kpack(&tree, &path) {
                        notice = Some(format!("Music cache write failed: {error}"));
                    }
                }
                Err(error) => notice = Some(format!("Music cache read failed: {error}")),
            }
        }
        Ok(LocalLibrary {
            tree,
            tracks,
            notice,
        })
    }

    pub(crate) fn apply_library(&mut self, library: LocalLibrary) {
        self.tree = Some(library.tree);
        self.tracks = library.tracks;
        let message = library
            .notice
            .unwrap_or_else(|| format!("Loaded {} local tracks.", self.tracks.len()));
        self.set_library_status(message);
        self.rebuild_visible();
    }

    pub(crate) fn set_library_status(&mut self, message: String) {
        self.set_status(match &self.playlist_error {
            Some(error) => format!("{error} | {message}"),
            None => message,
        });
    }

    pub(crate) fn apply_playlist(&mut self, result: io::Result<(PathBuf, Vec<PathBuf>)>) {
        match result {
            Ok((path, tracks)) => {
                self.playlist_path = Some(path);
                self.playlist = tracks;
                self.rebuild_visible();
            }
            Err(error) => {
                let message = format!("Playlist load failed: {error}");
                self.playlist_error = Some(message.clone());
                self.set_status(message);
            }
        }
    }

    /// Reloads the local library synchronously; qwx uses the background loader.
    pub fn load_local_music(&mut self) {
        let Some(dir) = dirs::audio_dir().or_else(dirs::home_dir) else {
            self.set_status("Music directory not found.");
            return;
        };
        let cache_dir = dirs::config_dir().map(|dir| dir.join("qwx"));
        match Self::scan_library(&dir, cache_dir.as_deref(), &AtomicBool::new(false)) {
            Ok(library) => self.apply_library(library),
            Err(error) => self.set_status(format!("Music library error: {error}")),
        }
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some(message.into());
    }

    fn append_visible(
        node: &TreeNode,
        depth: usize,
        filter: &str,
        parent_matches: bool,
        visible: &mut Vec<VisibleNode>,
    ) -> bool {
        let matching = parent_matches || node.name.to_lowercase().contains(filter);
        let start = visible.len();
        visible.push(VisibleNode {
            name: node.name.clone(),
            path: node.path.clone(),
            depth,
            is_dir: node.is_dir,
            expanded: node.expanded || !filter.is_empty(),
            is_favorite: node.is_favorite,
            track_count: node.track_count,
        });
        if node.is_dir && (node.expanded || !filter.is_empty()) {
            for child in &node.children {
                Self::append_visible(child, depth + 1, filter, matching, visible);
            }
        }
        if !filter.is_empty() && !matching && visible.len() == start + 1 {
            visible.pop();
            false
        } else {
            true
        }
    }

    fn rebuild_visible(&mut self) {
        self.visible.clear();
        match self.view {
            PlayerView::Tree => {
                if let Some(tree) = &self.tree {
                    Self::append_visible(
                        tree,
                        0,
                        &self.filter.to_lowercase(),
                        false,
                        &mut self.visible,
                    );
                }
            }
            PlayerView::Playlist => {
                let filter = self.filter.to_lowercase();
                for path in &self.playlist {
                    let name = path
                        .file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned();
                    if name.to_lowercase().contains(&filter) {
                        self.visible.push(VisibleNode {
                            name,
                            path: path.clone(),
                            depth: 0,
                            is_dir: false,
                            expanded: false,
                            is_favorite: false,
                            track_count: 0,
                        });
                    }
                }
            }
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
        if self.view == PlayerView::Tree && self.playback.is_some() {
            self.send_playback(PlaybackCommand::Tree(self.visible_tracks()));
        }
    }

    fn visible_tracks(&self) -> Vec<PathBuf> {
        self.visible
            .iter()
            .filter(|node| !node.is_dir)
            .map(|node| node.path.clone())
            .collect()
    }

    fn filter_changed(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
        self.rebuild_visible();
    }

    fn play_path(&mut self, path: &Path) {
        if self.playback.is_none() {
            match Playback::start(self.volume, self.repeat) {
                Ok(playback) => self.playback = Some(playback),
                Err(error) => {
                    self.set_status(format!("Audio worker failed: {error}"));
                    return;
                }
            }
        }
        let playlist = self.view == PlayerView::Playlist;
        let tracks = if playlist {
            self.playlist.clone()
        } else {
            self.visible_tracks()
        };
        self.send_playback(PlaybackCommand::Play {
            path: path.to_path_buf(),
            tracks,
            playlist,
        });
    }

    fn send_playback(&mut self, command: PlaybackCommand) -> bool {
        match &self.playback {
            Some(playback) => {
                if playback.sender.send(command).is_err() {
                    self.playback = None;
                    self.current_path = None;
                    self.set_status("Audio worker stopped unexpectedly.");
                    false
                } else {
                    true
                }
            }
            None => {
                self.set_status("Select a track to play.");
                false
            }
        }
    }

    fn poll_playback(&mut self) {
        let Some(playback) = &self.playback else {
            return;
        };
        if playback.thread.is_finished() {
            self.playback = None;
            self.current_path = None;
            self.set_status("Audio worker stopped unexpectedly.");
            return;
        }
        let update = playback
            .state
            .lock()
            .map(|state| (state.revision, state.current.clone(), state.status.clone()))
            .map_err(|error| error.to_string());
        match update {
            Ok((revision, current, status)) if revision != self.playback_revision => {
                self.playback_revision = revision;
                self.current_path = current;
                self.status_message = status;
            }
            Ok(_) => {}
            Err(error) => {
                self.set_status(format!("Audio state error: {error}"));
            }
        }
    }

    fn adjust_volume(&mut self, delta: i32) {
        self.volume = (self.volume as i32 + delta).clamp(0, 100) as u32;
        if self.playback.is_some() {
            self.send_playback(PlaybackCommand::Volume(self.volume));
        } else {
            self.set_status(format!("Volume: {}%", self.volume));
        }
    }

    fn change_playlist(&mut self, add: bool) {
        let Some(path) = self.playlist_path.clone() else {
            self.set_status(
                self.playlist_error
                    .clone()
                    .unwrap_or_else(|| "Playlist is still loading.".to_string()),
            );
            return;
        };
        let Some(node) = self.visible.get(self.cursor) else {
            self.set_status("No track selected.");
            return;
        };
        if node.is_dir {
            self.set_status("Select a file, not a directory.");
            return;
        }
        let selected = node.path.clone();
        let mut tracks = self.playlist.clone();
        if add {
            if tracks.contains(&selected) {
                self.set_status("Track is already in the playlist.");
                return;
            }
            tracks.push(selected);
        } else if let Some(index) = tracks.iter().position(|track| track == &selected) {
            tracks.remove(index);
        } else {
            self.set_status("Track is not in the playlist.");
            return;
        }
        match save_playlist(&path, &tracks) {
            Ok(()) => {
                self.playlist = tracks;
                let notified = self.playback.is_none()
                    || self.send_playback(PlaybackCommand::Playlist(self.playlist.clone()));
                self.rebuild_visible();
                if notified {
                    self.set_status(if add {
                        "Added to playlist."
                    } else {
                        "Removed from playlist."
                    });
                }
            }
            Err(error) => self.set_status(format!("Playlist save failed: {error}")),
        }
    }

    fn set_expanded(&mut self, expanded: bool) {
        if !self.filter.is_empty() {
            return;
        }
        let Some(selected) = self.visible.get(self.cursor) else {
            return;
        };
        let path = selected.path.clone();
        if let Some(node) = self.tree.as_mut().and_then(|tree| tree.find_mut(&path))
            && node.is_dir
        {
            node.expanded = expanded;
            self.rebuild_visible();
            if expanded
                && self.cursor + 1 < self.visible.len()
                && self.visible[self.cursor + 1].depth > self.visible[self.cursor].depth
            {
                self.cursor += 1;
            }
        }
    }

    pub fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        if self.editing_filter {
            match (modifiers, code) {
                (KeyModifiers::NONE, KeyCode::Esc) => {
                    self.editing_filter = false;
                    self.filter.clear();
                    self.filter_changed();
                }
                (KeyModifiers::NONE, KeyCode::Enter) => self.editing_filter = false,
                (KeyModifiers::NONE, KeyCode::Backspace) => {
                    self.filter.pop();
                    self.filter_changed();
                }
                (m, KeyCode::Char(c)) if m.is_empty() || m == KeyModifiers::SHIFT => {
                    self.filter.push(c);
                    self.filter_changed();
                }
                _ => {}
            }
            return true;
        }

        match (modifiers, code) {
            (KeyModifiers::NONE, KeyCode::Esc) if !self.filter.is_empty() => {
                self.filter.clear();
                self.filter_changed();
            }
            (KeyModifiers::NONE, KeyCode::Esc | KeyCode::Char('q')) => return false,
            (KeyModifiers::NONE, KeyCode::Char('/')) => {
                self.editing_filter = true;
                self.filter.clear();
                self.filter_changed();
            }
            (KeyModifiers::NONE, KeyCode::Char('t')) => {
                self.view = if self.view == PlayerView::Tree {
                    PlayerView::Playlist
                } else {
                    PlayerView::Tree
                };
                self.cursor = 0;
                self.scroll = 0;
                self.rebuild_visible();
            }
            (KeyModifiers::NONE, KeyCode::Char('r')) => {
                self.repeat = self.repeat.next();
                if self.playback.is_none()
                    || self.send_playback(PlaybackCommand::Repeat(self.repeat))
                {
                    self.set_status(format!("Repeat: {}", self.repeat.label()));
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('a')) if self.view == PlayerView::Tree => {
                self.change_playlist(true)
            }
            (KeyModifiers::NONE, KeyCode::Char('d') | KeyCode::Delete)
                if self.view == PlayerView::Playlist =>
            {
                self.change_playlist(false)
            }
            (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                self.cursor = (self.cursor + 1).min(self.visible.len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Char('g') | KeyCode::Home) => self.cursor = 0,
            (KeyModifiers::SHIFT, KeyCode::Char('G')) | (KeyModifiers::NONE, KeyCode::End) => {
                self.cursor = self.visible.len().saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                self.cursor = (self.cursor + 5).min(self.visible.len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => self.cursor = self.cursor.saturating_sub(5),
            (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right) => self.set_expanded(true),
            (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left) => self.set_expanded(false),
            (KeyModifiers::NONE, KeyCode::Char('x')) if self.filter.is_empty() => {
                if let Some(tree) = &mut self.tree {
                    tree.collapse_all();
                    self.cursor = 0;
                    self.scroll = 0;
                    self.rebuild_visible();
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(node) = self.visible.get(self.cursor) {
                    if node.is_dir {
                        self.set_expanded(!node.expanded);
                    } else {
                        let path = node.path.clone();
                        self.play_path(&path);
                    }
                }
            }
            (KeyModifiers::NONE, KeyCode::Char(' ')) => {
                self.send_playback(PlaybackCommand::TogglePause);
            }
            (
                KeyModifiers::NONE,
                KeyCode::Char('n' | '>') | KeyCode::Media(MediaKeyCode::TrackNext),
            ) => {
                if self.playback.is_none() {
                    let path = if self.view == PlayerView::Playlist {
                        self.playlist.first().cloned()
                    } else {
                        self.visible_tracks().into_iter().next()
                    };
                    if let Some(path) = path {
                        self.play_path(&path);
                    } else {
                        self.set_status("No next track.");
                    }
                } else {
                    self.send_playback(PlaybackCommand::Next);
                }
            }
            (
                KeyModifiers::NONE,
                KeyCode::Char('p' | '<') | KeyCode::Media(MediaKeyCode::TrackPrevious),
            ) => {
                self.send_playback(PlaybackCommand::Previous);
            }
            (KeyModifiers::NONE, KeyCode::Char('+' | '=' | ']')) => self.adjust_volume(5),
            (KeyModifiers::NONE, KeyCode::Char('-' | '_' | '[')) => self.adjust_volume(-5),
            _ => {}
        }
        true
    }

    pub fn draw_local_player<W: Write>(
        &mut self,
        writer: &mut W,
        w: u16,
        h: u16,
    ) -> io::Result<()> {
        self.poll_playback();
        queue!(
            writer,
            Clear(ClearType::All),
            SetBackgroundColor(Color::Black),
            SetForegroundColor(Color::White),
            Hide
        )?;
        let show_filter = self.editing_filter || !self.filter.is_empty();
        let tree_height = h.saturating_sub(if show_filter { 3 } else { 2 }) as usize;
        if tree_height > 0 {
            if self.cursor < self.scroll {
                self.scroll = self.cursor;
            } else if self.cursor >= self.scroll + tree_height {
                self.scroll = self.cursor + 1 - tree_height;
            }
            for (row, node) in self
                .visible
                .iter()
                .skip(self.scroll)
                .take(tree_height)
                .enumerate()
            {
                let index = self.scroll + row;
                let marker = if node.is_dir {
                    if node.expanded {
                        "[D] ▼ "
                    } else {
                        "[D] ▸ "
                    }
                } else {
                    "[F] "
                };
                let cursor = if index == self.cursor { "▶ " } else { "  " };
                let playing = if self.current_path.as_ref() == Some(&node.path) {
                    "~ "
                } else {
                    ""
                };
                let favorite = if node.is_favorite { "* " } else { "" };
                let count = if node.is_dir && node.track_count > 0 {
                    format!(" ({})", node.track_count)
                } else {
                    String::new()
                };
                let text = format!(
                    "{}{}{}{}{}{}{}",
                    "  ".repeat(node.depth),
                    cursor,
                    marker,
                    favorite,
                    playing,
                    node.name,
                    count
                );
                let text = truncate_width(&text, w as usize);
                queue!(writer, MoveTo(0, row as u16))?;
                if index == self.cursor {
                    queue!(
                        writer,
                        SetBackgroundColor(Color::White),
                        SetForegroundColor(Color::Black)
                    )?;
                } else {
                    queue!(
                        writer,
                        SetBackgroundColor(Color::Black),
                        SetForegroundColor(if node.is_dir {
                            Color::Blue
                        } else {
                            Color::White
                        })
                    )?;
                }
                queue!(writer, Print(text), ResetColor)?;
            }
            if self.visible.is_empty() && (self.tree.is_some() || self.view == PlayerView::Playlist)
            {
                let message = if self.view == PlayerView::Playlist && self.filter.is_empty() {
                    if self.playlist_path.is_some() {
                        "Playlist empty. Add tracks with a in the tree."
                    } else {
                        "Playlist unavailable or loading."
                    }
                } else {
                    "No matches."
                };
                queue!(
                    writer,
                    MoveTo(0, 0),
                    SetForegroundColor(Color::White),
                    Print(truncate_width(message, w as usize)),
                    ResetColor
                )?;
            }
        }
        if show_filter && h >= 3 {
            let prefix = "Filter: ";
            let remaining = (w as usize).saturating_sub(prefix.len());
            let mut used = 0;
            let tail: String = self
                .filter
                .chars()
                .rev()
                .take_while(|character| {
                    used += character.width().unwrap_or(0);
                    used <= remaining
                })
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let prompt = format!("{prefix}{tail}");
            queue!(
                writer,
                MoveTo(0, h - 3),
                SetBackgroundColor(Color::Black),
                SetForegroundColor(Color::White),
                Print(truncate_width(&prompt, w as usize)),
                ResetColor
            )?;
        }
        if h >= 2 {
            let view = if self.view == PlayerView::Tree {
                "Tree"
            } else {
                "Playlist"
            };
            let heading = format!(
                "{view} | Repeat: {} | t: switch | r: repeat",
                self.repeat.label()
            );
            queue!(
                writer,
                MoveTo(0, h - 2),
                SetBackgroundColor(Color::Black),
                SetForegroundColor(Color::White),
                Print(truncate_width(&heading, w as usize)),
                ResetColor
            )?;
        }
        if h > 0
            && let Some(status) = &self.status_message
        {
            queue!(
                writer,
                MoveTo(0, h - 1),
                SetBackgroundColor(Color::Black),
                SetForegroundColor(Color::White),
                Print(truncate_width(status, w as usize)),
                ResetColor
            )?;
        }
        writer.flush()
    }
}

fn truncate_width(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let dots = width.min(3);
    let limit = width - dots;
    let mut used = 0;
    let prefix: String = text
        .chars()
        .take_while(|character| {
            used += character.width().unwrap_or(0);
            used <= limit
        })
        .collect();
    format!("{prefix}{}", ".".repeat(dots))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_status(player: &mut MusicPlayer, prefix: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            player.poll_playback();
            if player
                .status_message
                .as_deref()
                .is_some_and(|message| message.starts_with(prefix))
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("missing status {prefix}: {:?}", player.status_message);
    }

    #[test]
    fn scan_indexes_nested_music_and_refreshes_cache() {
        let temp = tempfile::tempdir().unwrap();
        let music = temp.path().join("music");
        let album = music.join("artist").join("album");
        let cache = temp.path().join("cache");
        fs::create_dir_all(&album).unwrap();
        fs::write(music.join("top.mp3"), b"").unwrap();
        fs::write(album.join("song.FLAC"), b"").unwrap();
        fs::write(album.join("notes.txt"), b"").unwrap();
        let cancelled = AtomicBool::new(false);
        let library = MusicPlayer::scan_library(&music, Some(&cache), &cancelled).unwrap();
        assert_eq!(library.tracks.len(), 2);
        assert_eq!(library.tree.track_count, 2);
        let artist = library
            .tree
            .children
            .iter()
            .find(|child| child.name == "artist")
            .unwrap();
        assert_eq!(artist.track_count, 1);
        assert_eq!(artist.children[0].children[0].name, "song.FLAC");
        assert!(library.notice.is_none());
        let cached = MusicPlayer::scan_library(&music, Some(&cache), &cancelled).unwrap();
        assert_eq!(cached.tree.track_count, 2);
        fs::write(album.join("new.ogg"), b"").unwrap();
        let updated = MusicPlayer::scan_library(&music, Some(&cache), &cancelled).unwrap();
        assert_eq!(updated.tree.track_count, 3);
        assert_eq!(updated.tracks.len(), 3);
    }

    #[test]
    fn music_cache_round_trips_large_trees_and_rejects_truncation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("music.kpack");
        let mut tree = TreeNode::new("root".to_string(), temp.path().to_path_buf(), true);
        for index in 0..2000 {
            tree.children.push(TreeNode::new(
                format!("song-{index:04}-long-name.mp3"),
                PathBuf::from(format!("path-{index}")),
                false,
            ));
        }
        assert!(bincode::serialize(&tree).unwrap().len() > u16::MAX as usize);
        MusicPlayer::save_tree_kpack(&tree, &path).unwrap();
        let loaded = MusicPlayer::load_tree_kpack(&path).unwrap().unwrap();
        assert_eq!(loaded.children.len(), tree.children.len());
        assert_eq!(loaded.children[1999].name, tree.children[1999].name);
        fs::write(&path, b"QWX2\x05\0bad").unwrap();
        assert_eq!(
            MusicPlayer::load_tree_kpack(&path).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn cancelled_music_scan_stops_without_creating_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let result = MusicPlayer::scan_library(temp.path(), Some(&cache), &AtomicBool::new(true));
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Interrupted);
        assert!(!cache.exists());
    }

    #[test]
    fn invalid_audio_returns_an_error_without_claiming_playback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad.flac");
        fs::write(&path, b"not audio").unwrap();
        let mut player = MusicPlayer::new();
        player.play_path(&path);
        wait_for_status(&mut player, "Local play error:");
        assert!(player.current_path.is_none());
    }

    #[test]
    fn playlist_is_persistent_and_switches_views() {
        let temp = tempfile::tempdir().unwrap();
        let music = temp.path().join("music");
        fs::create_dir(&music).unwrap();
        let track = music.join("song.flac");
        fs::write(&track, b"").unwrap();
        let path = temp.path().join("config").join("playlist.json");
        let mut player = MusicPlayer::new();
        player.apply_playlist(Ok((path.clone(), load_playlist(&path).unwrap())));
        player.apply_library(
            MusicPlayer::scan_library(&music, None, &AtomicBool::new(false)).unwrap(),
        );
        player.handle_key(KeyCode::Down, KeyModifiers::NONE);
        player.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(load_playlist(&path).unwrap(), vec![track.clone()]);

        let mut reopened = MusicPlayer::new();
        reopened.apply_playlist(Ok((path.clone(), load_playlist(&path).unwrap())));
        reopened.handle_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(reopened.visible[0].path, track);
        reopened.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(load_playlist(&path).unwrap().is_empty());
        assert!(reopened.visible.is_empty());
        reopened.handle_key(KeyCode::Char('t'), KeyModifiers::NONE);
        assert!(reopened.view == PlayerView::Tree);
    }

    #[test]
    fn playlist_load_and_save_errors_do_not_discard_data() {
        let temp = tempfile::tempdir().unwrap();
        let music = temp.path().join("music");
        fs::create_dir(&music).unwrap();
        fs::write(music.join("song.flac"), b"").unwrap();
        let path = temp.path().join("playlist.json");
        fs::write(&path, "invalid json").unwrap();
        let mut player = MusicPlayer::new();
        player.apply_playlist(load_playlist(&path).map(|tracks| (path.clone(), tracks)));
        player.apply_library(
            MusicPlayer::scan_library(&music, None, &AtomicBool::new(false)).unwrap(),
        );
        player.handle_key(KeyCode::Down, KeyModifiers::NONE);
        player.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(
            player
                .status_message
                .as_deref()
                .unwrap()
                .starts_with("Playlist load failed:")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "invalid json");

        player.apply_playlist(Ok((
            temp.path().join("missing-file").join("playlist.json"),
            Vec::new(),
        )));
        fs::write(temp.path().join("missing-file"), b"not a directory").unwrap();
        player.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(
            player
                .status_message
                .as_deref()
                .unwrap()
                .starts_with("Playlist save failed:")
        );
        assert!(player.playlist.is_empty());
    }

    #[test]
    fn repeat_cycles_through_track_and_playlist() {
        let mut player = MusicPlayer::new();
        for expected in [Repeat::Track, Repeat::Playlist, Repeat::Off] {
            player.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
            assert_eq!(player.repeat, expected);
        }
    }

    #[test]
    fn tree_visibly_distinguishes_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("album")).unwrap();
        fs::write(temp.path().join("song.flac"), b"").unwrap();
        let mut player = MusicPlayer::new();
        player.apply_library(
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap(),
        );
        let mut output = Vec::new();
        player.draw_local_player(&mut output, 80, 20).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("[D] ▸ album"));
        assert!(output.contains("[F] song.flac"));
    }

    #[test]
    fn tree_playback_order_follows_expansion_and_filter() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("album");
        fs::create_dir(&album).unwrap();
        let first = album.join("first.flac");
        let second = album.join("second.flac");
        let top = temp.path().join("top.flac");
        for path in [&first, &second, &top] {
            fs::write(path, b"").unwrap();
        }
        let mut player = MusicPlayer::new();
        player.apply_library(
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap(),
        );
        assert_eq!(player.visible_tracks(), vec![top.clone()]);
        player.handle_key(KeyCode::Down, KeyModifiers::NONE);
        player.handle_key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(
            player.visible_tracks(),
            vec![first.clone(), second.clone(), top.clone()]
        );
        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        for c in "second".chars() {
            player.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(player.visible_tracks(), vec![second]);
        player.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            player.visible_tracks(),
            vec![first, album.join("second.flac"), top]
        );
    }

    #[test]
    fn tree_changes_update_active_playback_order() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.flac");
        let second = temp.path().join("second.flac");
        fs::write(&first, b"").unwrap();
        fs::write(&second, b"").unwrap();
        let mut player = MusicPlayer::new();
        player.apply_library(
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap(),
        );
        let (sender, receiver) = mpsc::channel();
        player.playback = Some(Playback {
            sender,
            state: Arc::new(std::sync::Mutex::new(playback::State::default())),
            thread: std::thread::spawn(|| {}),
        });

        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(
            matches!(receiver.recv().unwrap(), PlaybackCommand::Tree(paths) if paths == vec![first.clone(), second.clone()])
        );
        for c in "second".chars() {
            player.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let mut last = None;
        while let Ok(command) = receiver.try_recv() {
            last = Some(command);
        }
        assert!(matches!(last, Some(PlaybackCommand::Tree(paths)) if paths == vec![second]));
    }

    #[test]
    fn filter_updates_tree_navigation_and_rendering_live() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("Artist").join("Album");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("Blue.flac"), b"invalid").unwrap();
        fs::write(album.join("Red.flac"), b"invalid").unwrap();
        let library =
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap();
        let mut player = MusicPlayer::new();
        player.apply_library(library);
        assert_eq!(player.visible.len(), 2);
        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        player.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        player.handle_key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(player.filter, "bl");
        assert!(player.visible.iter().any(|node| node.name == "Blue.flac"));
        assert!(!player.visible.iter().any(|node| node.name == "Red.flac"));
        assert_eq!(player.visible.last().unwrap().depth, 3);
        let mut output = Vec::new();
        player.draw_local_player(&mut output, 80, 20).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Filter: bl"));
        assert!(output.contains("\u{1b}[38;5;15mFilter: bl"));
        assert!(output.contains("Blue.flac"));
        assert!(!output.contains("Red.flac"));
        player.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        player.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(player.visible[player.cursor].name, "Blue.flac");
        player.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        wait_for_status(&mut player, "Local play error:");
        player.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(player.filter.is_empty());
        assert_eq!(player.visible.len(), 2);
    }

    #[test]
    fn filter_matches_directory_names_and_backspace_updates_results() {
        let temp = tempfile::tempdir().unwrap();
        let album = temp.path().join("Jazz");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("Song.flac"), b"").unwrap();
        let library =
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap();
        let mut player = MusicPlayer::new();
        player.apply_library(library);
        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        for c in "jazzx".chars() {
            player.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(player.visible.is_empty());
        player.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(player.visible.iter().any(|node| node.name == "Song.flac"));
        player.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(player.filter.is_empty());
        assert!(!player.editing_filter);
    }

    #[test]
    fn filtering_handles_loading_and_scrolls_to_the_selection() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..10 {
            fs::write(temp.path().join(format!("song-{index}.flac")), b"").unwrap();
        }
        let library =
            MusicPlayer::scan_library(temp.path(), None, &AtomicBool::new(false)).unwrap();
        let mut player = MusicPlayer::new();
        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        for c in "song".chars() {
            player.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        player.apply_library(library);
        assert_eq!(player.visible.len(), 11);
        player.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        player.handle_key(KeyCode::End, KeyModifiers::NONE);
        let mut output = Vec::new();
        player.draw_local_player(&mut output, 40, 5).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("song-9.flac"));
        assert!(!output.contains("song-0.flac"));
        assert_eq!(player.visible[player.cursor].name, "song-9.flac");
    }

    #[test]
    fn long_filter_shows_the_latest_text_in_white() {
        let mut player = MusicPlayer::new();
        player.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        for c in "123456789".chars() {
            player.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let mut output = Vec::new();
        player.draw_local_player(&mut output, 12, 4).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\u{1b}[38;5;15mFilter: 6789"));
    }
}
