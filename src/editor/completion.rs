use ropey::Rope;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(super) struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub(super) struct Item {
    pub label: String,
    pub edits: Vec<Edit>,
}

pub(super) struct Request {
    pub path: PathBuf,
    pub rope: Rope,
    pub line: usize,
    pub col: usize,
}

pub(super) struct Task {
    pub receiver: Receiver<Result<Vec<Item>, String>>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn language(path: &Path) -> Result<(&'static str, &'static str), String> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Ok(("rust-analyzer", "rust")),
        Some("c" | "h") => Ok(("clangd", "c")),
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "h++" | "c++") => Ok(("clangd", "cpp")),
        Some("d" | "di") => Ok(("serve-d", "d")),
        Some("sh") => Ok(("", "sh")),
        _ => Err("Completion is available for Rust, C
, C++ and D files."
            .to_string()),
    }
}

pub(super) fn supports_auto_completion(path: &Path) -> bool {
    language(path).is_ok_and(|(binary, _)| !binary.is_empty())
}

fn root(path: &Path, language: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let markers: &[&str] = match language {
        "rust" => &["Cargo.toml", "rust-project.json"],
        "d" => &["dub.json", "dub.sdl"],
        _ => &["compile_commands.json", "compile_flags.txt"],
    };
    parent
        .ancestors()
        .find(|dir| markers.iter().any(|marker| dir.join(marker).is_file()))
        .or_else(|| parent.ancestors().find(|dir| dir.join(".git").exists()))
        .unwrap_or(parent)
        .to_path_buf()
}

fn uri(path: &Path) -> Result<String, String> {
    let path = path.to_str().ok_or("LSP requires a UTF-8 file path.")?;
    let mut result = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(result)
}

fn write_message(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).map_err(|error| error.to_string())?;
    writer.write_all(&body).map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

type Writer = Sender<(Value, Sender<Result<(), String>>)>;

fn send(
    writer: &Writer,
    value: Value,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::channel();
    writer
        .send((value, sender))
        .map_err(|_| "Language server stdin closed.".to_string())?;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Completion cancelled.".to_string());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Language server timed out.")?;
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Language server stdin closed.".to_string());
            }
        }
    }
}

fn read_message(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "language server closed stdout",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
            }
        }
    }
    let length = length.ok_or_else(|| io::Error::other("missing LSP Content-Length"))?;
    if length > 16 * 1024 * 1024 {
        return Err(io::Error::other("language server message too large"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(io::Error::other)
}

fn position(rope: &Rope, line: usize, col: usize) -> Result<Value, String> {
    if line >= rope.len_lines() || col > rope.line(line).len_chars() {
        return Err("Invalid cursor position.".to_string());
    }
    let utf16: usize = rope.line(line).chars().take(col).map(char::len_utf16).sum();
    Ok(json!({"line": line, "character": utf16}))
}

fn char_offset(rope: &Rope, pos: &Value) -> Option<usize> {
    let line = usize::try_from(pos.get("line")?.as_u64()?).ok()?;
    let target = usize::try_from(pos.get("character")?.as_u64()?).ok()?;
    if line >= rope.len_lines() {
        return None;
    }
    let mut units = 0;
    let mut col = 0;
    for ch in rope.line(line).chars() {
        if units == target {
            break;
        }
        if ch == '\r' || ch == '\n' {
            return None;
        }
        units += ch.len_utf16();
        col += 1;
    }
    (units == target).then(|| rope.line_to_char(line) + col)
}

fn parse_edit(rope: &Rope, value: &Value) -> Option<Edit> {
    let range = value.get("range").or_else(|| value.get("insert"))?;
    let start = char_offset(rope, range.get("start")?)?;
    let end = char_offset(rope, range.get("end")?)?;
    if start > end {
        return None;
    }
    Some(Edit {
        start,
        end,
        text: value.get("newText")?.as_str()?.to_string(),
    })
}

pub(super) fn prefix_start(rope: &Rope, line: usize, cursor: usize) -> usize {
    let line_start = rope.line_to_char(line);
    let mut start = cursor;
    while start > line_start {
        let ch = rope.char(start - 1);
        if ch.is_alphanumeric() || ch == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

fn parse_items(result: &Value, request: &Request) -> Vec<Item> {
    let values = result
        .as_array()
        .or_else(|| result.get("items").and_then(Value::as_array));
    let Some(values) = values else {
        return Vec::new();
    };
    let cursor = request.rope.line_to_char(request.line) + request.col;
    let start = prefix_start(&request.rope, request.line, cursor);
    let prefix = request.rope.slice(start..cursor).to_string().to_lowercase();
    let mut matched: Vec<_> = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            if value.get("insertTextFormat").and_then(Value::as_u64) == Some(2)
                || value.get("command").is_some()
            {
                return None;
            }
            let label = value.get("label")?.as_str()?.to_string();
            let filter = value
                .get("filterText")
                .and_then(Value::as_str)
                .unwrap_or(&label)
                .trim_start()
                .to_lowercase();
            let score = if prefix.is_empty() || filter.starts_with(&prefix) {
                0
            } else if filter.contains(&prefix) {
                1
            } else {
                return None;
            };
            let main = if let Some(edit) = value.get("textEdit") {
                parse_edit(&request.rope, edit)?
            } else {
                Edit {
                    start,
                    end: cursor,
                    text: value
                        .get("insertText")
                        .and_then(Value::as_str)
                        .unwrap_or(&label)
                        .to_string(),
                }
            };
            let mut edits = vec![main];
            if let Some(extra) = value.get("additionalTextEdits") {
                for value in extra.as_array()? {
                    edits.push(parse_edit(&request.rope, value)?);
                }
            }
            let mut ranges: Vec<_> = edits.iter().map(|edit| (edit.start, edit.end)).collect();
            ranges.sort_unstable();
            if ranges
                .windows(2)
                .any(|pair| pair[0].1 > pair[1].0 || pair[0].0 == pair[1].0)
            {
                return None;
            }
            let sort = value
                .get("sortText")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((score, sort, index, Item { label, edits }))
        })
        .collect();
    matched.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    matched
        .into_iter()
        .take(100)
        .map(|(_, _, _, item)| item)
        .collect()
}

fn wait_response(
    receiver: &Receiver<Result<Value, String>>,
    writer: &Writer,
    wanted: i64,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Value, String> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Completion cancelled.".to_string());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("Language server timed out.")?;
        let message = match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(message) => message?,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Language server stopped.".to_string());
            }
        };
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if let Some(id) = message.get("id") {
                let response = if method == "workspace/configuration" {
                    let len = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    json!({"jsonrpc":"2.0","id":id,"result":vec![Value::Null; len]})
                } else {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported"}})
                };
                send(writer, response, deadline, cancelled)?;
            }
        } else if message.get("id").and_then(Value::as_i64) == Some(wanted) {
            if let Some(error) = message.get("error") {
                return Err(format!("Language server: {error}"));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

fn complete(request: Request, cancelled: &AtomicBool) -> Result<Vec<Item>, String> {
    let (binary, lang) = language(&request.path)?;
    let root = root(&request.path, lang);
    let file_uri = uri(&request.path)?;
    let root_uri = uri(&root)?;
    let mut child = Command::new(binary)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Cannot start {binary}: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Language server stdin unavailable.")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Language server stdout unavailable.")?;
    let _server = Server { child };
    let (writer, writes) = mpsc::channel::<(Value, Sender<Result<(), String>>)>();
    thread::spawn(move || {
        while let Ok((value, sender)) = writes.recv() {
            let result = write_message(&mut stdin, &value);
            let failed = result.is_err();
            let _ = sender.send(result);
            if failed {
                break;
            }
        }
    });
    let (sender, receiver) = mpsc::sync_channel(32);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let result = read_message(&mut reader).map_err(|error| error.to_string());
            let failed = result.is_err();
            if sender.send(result).is_err() || failed {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    send(
        &writer,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "processId":std::process::id(),"rootUri":root_uri,
            "capabilities":{"general":{"positionEncodings":["utf-16"]},"textDocument":{"completion":{"completionItem":{"snippetSupport":false}}}},
            "workspaceFolders":[{"uri":root_uri,"name":"qwx"}]
        }}),
        deadline,
        cancelled,
    )?;
    let initialize = wait_response(&receiver, &writer, 1, deadline, cancelled)?;
    if initialize
        .pointer("/capabilities/positionEncoding")
        .and_then(Value::as_str)
        .is_some_and(|encoding| encoding != "utf-16")
    {
        return Err("Language server selected an unsupported position encoding.".to_string());
    }
    send(
        &writer,
        json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        deadline,
        cancelled,
    )?;
    send(
        &writer,
        json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{
            "uri":file_uri,"languageId":lang,"version":1,"text":request.rope.to_string()
        }}}),
        deadline,
        cancelled,
    )?;
    for attempt in 0..21 {
        let id = 2 + attempt;
        let cursor = request.rope.line_to_char(request.line) + request.col;
        let completion_col = if lang == "d" {
            prefix_start(&request.rope, request.line, cursor)
                - request.rope.line_to_char(request.line)
        } else {
            request.col
        };
        send(
            &writer,
            json!({"jsonrpc":"2.0","id":id,"method":"textDocument/completion","params":{
                "textDocument":{"uri":file_uri},
                "position":position(&request.rope, request.line, completion_col)?,
                "context":{"triggerKind":1}
            }}),
            deadline,
            cancelled,
        )?;
        let result = wait_response(&receiver, &writer, id, deadline, cancelled)?;
        if result.get("itemDefaults").is_some() {
            return Err("Language server returned unsupported completion defaults.".to_string());
        }
        let items = parse_items(&result, &request);
        if !items.is_empty() || !matches!(lang, "rust" | "d") || attempt == 20 {
            return Ok(items);
        }
        thread::sleep(Duration::from_millis(250));
    }
    unreachable!()
}

pub(super) fn start(request: Request) -> io::Result<Task> {
    let (sender, receiver) = mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    thread::Builder::new()
        .name("qwx-completion".to_string())
        .spawn(move || {
            let result = complete(request, &worker_cancelled);
            let _ = sender.send(result);
        })?;
    Ok(Task {
        receiver,
        cancelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn real_servers_complete_local_variables() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname=\"completion-test\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let d_src = dir.path().join("d-project");
        std::fs::create_dir_all(d_src.join("source")).unwrap();
        std::fs::write(
            d_src.join("dub.json"),
            r#"{"name":"completion-test","targetType":"executable"}"#,
        )
        .unwrap();
        for (name, source) in [
            ("main.rs", "fn main() { let alphabet = 1; alph"),
            ("main.c", "int main() { int alphabet = 1; alph"),
            ("main.cpp", "int main() { int alphabet = 1; alph"),
            ("main.d", "void main() { int alphabet = 1; alph"),
        ] {
            let path = if name.ends_with(".d") {
                d_src.join("source/app.d")
            } else {
                src.join(name)
            };
            std::fs::write(&path, if name.ends_with(".d") { source } else { "" }).unwrap();
            let items = complete(
                Request {
                    path,
                    rope: Rope::from_str(source),
                    line: 0,
                    col: source.chars().count(),
                },
                &AtomicBool::new(false),
            )
            .unwrap();
            let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
            assert!(
                labels.iter().any(|label| label.contains("alphabet")),
                "{name}: {labels:?}"
            );
        }
    }

    #[test]
    fn converts_utf16_positions_and_rejects_split_surrogates() {
        let rope = Rope::from_str("é😀abc\n");
        assert_eq!(position(&rope, 0, 2).unwrap()["character"], 3);
        assert_eq!(
            char_offset(&rope, &json!({"line":0,"character":3})),
            Some(2)
        );
        assert_eq!(char_offset(&rope, &json!({"line":0,"character":2})), None);
    }

    #[test]
    fn parses_edits_and_rejects_snippets_or_overlaps() {
        let request = Request {
            path: PathBuf::from("/tmp/a.rs"),
            rope: Rope::from_str("fo"),
            line: 0,
            col: 2,
        };
        let result = json!([
            {"label":"foo","textEdit":{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"newText":"foo"}},
            {"label":"fo_snippet","insertTextFormat":2,"insertText":"x($0)"},
            {"label":"fo_bad","textEdit":{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"newText":"x"},"additionalTextEdits":[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":2}},"newText":"x"}]}
        ]);
        let items = parse_items(&result, &request);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].edits[0].text, "foo");
        assert_eq!(items[0].edits[0].start, 0);
    }

    #[test]
    fn filters_and_prioritizes_suggestions_for_typed_prefix() {
        let request = Request {
            path: PathBuf::from("/tmp/a.rs"),
            rope: Rope::from_str("alph"),
            line: 0,
            col: 4,
        };
        let items = parse_items(
            &json!([
                {"label":"self::"},
                {"label":"myalphabet"},
                {"label":"alphabet"},
                {"label":"alias","filterText":"alpha_alias"}
            ]),
            &request,
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec!["alphabet", "alias", "myalphabet"]
        );
    }

    #[test]
    fn stalled_write_respects_deadline_and_cancellation() {
        let (writer, _writes) = mpsc::channel();
        let cancelled = AtomicBool::new(false);
        assert_eq!(
            send(
                &writer,
                json!({}),
                Instant::now() + Duration::from_millis(20),
                &cancelled
            ),
            Err("Language server timed out.".to_string())
        );
        cancelled.store(true, Ordering::Relaxed);
        assert_eq!(
            send(
                &writer,
                json!({}),
                Instant::now() + Duration::from_secs(1),
                &cancelled
            ),
            Err("Completion cancelled.".to_string())
        );
    }
}
