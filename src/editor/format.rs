use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| format!("Cannot resolve file path: {error}"))
    }
}

fn rust_edition(path: &Path) -> Result<String, String> {
    for dir in path.parent().into_iter().flat_map(Path::ancestors) {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let contents = std::fs::read_to_string(&manifest)
            .map_err(|error| format!("Cannot read {}: {error}", manifest.display()))?;
        let value: toml::Value = toml::from_str(&contents)
            .map_err(|error| format!("Cannot parse {}: {error}", manifest.display()))?;
        let edition = value
            .get("package")
            .and_then(|package| package.get("edition"))
            .and_then(toml::Value::as_str)
            .or_else(|| {
                value
                    .get("workspace")
                    .and_then(|workspace| workspace.get("package"))
                    .and_then(|package| package.get("edition"))
                    .and_then(toml::Value::as_str)
            });
        if let Some(edition) = edition {
            return Ok(edition.to_string());
        }
    }
    Ok("2015".to_string())
}

fn config_dir(path: &Path) -> PathBuf {
    path.parent()
        .and_then(|parent| {
            parent
                .ancestors()
                .find(|dir| dir.join(".editorconfig").is_file())
        })
        .map(Path::to_path_buf)
        .unwrap_or_else(|| {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        })
}

fn preserve_line_endings(formatted: String, original: &str) -> String {
    let crlf = original.matches("\r\n").count();
    let lf = original.matches('\n').count().saturating_sub(crlf);
    if crlf <= lf {
        return formatted;
    }
    formatted.replace("\r\n", "\n").replace('\n', "\r\n")
}

pub(super) fn format(path: &Path, source: &str) -> Result<Option<String>, String> {
    let path = absolute_path(path)?;
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return Ok(None);
    };

    let mut command = match extension {
        "rs" => {
            let edition = rust_edition(&path)?;
            let mut command = Command::new("rustfmt");
            command.args(["--emit", "stdout", "--edition", &edition]);
            command
        }
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "h++" | "c++" => {
            let mut command = Command::new("clang-format");
            command.arg(format!("--assume-filename={}", path.display()));
            command
        }
        "d" | "di" => {
            let mut command = Command::new("dfmt");
            command.arg("--config").arg(config_dir(&path));
            command
        }
        _ => return Ok(None),
    };

    command
        .current_dir(path.parent().unwrap_or_else(|| Path::new(".")))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let formatter = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot start {formatter}: {error}"))?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| format!("{formatter} stdin unavailable."))?
        .write_all(source.as_bytes());
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("Cannot send source to {formatter}: {error}"));
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("{formatter} failed: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("{formatter} exited with status {}.", output.status)
        } else {
            format!("{formatter}: {stderr}")
        });
    }
    let formatted = String::from_utf8(output.stdout)
        .map_err(|error| format!("{formatter} returned invalid UTF-8: {error}"))?;
    Ok(Some(preserve_line_endings(formatted, source)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_crlf_when_formatting_output() {
        assert_eq!(
            preserve_line_endings("one\ntwo\n".to_string(), "one\r\ntwo\r\n"),
            "one\r\ntwo\r\n"
        );
        assert_eq!(
            preserve_line_endings("one\ntwo\n".to_string(), "one\ntwo\n"),
            "one\ntwo\n"
        );
    }
}
