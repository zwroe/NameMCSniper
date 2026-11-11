use std::{
    env,
    fmt::Display,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, Result, bail};
use chrono::{SecondsFormat, Utc};

pub struct RunLog {
    file: File,
    path: PathBuf,
}

impl RunLog {
    pub fn open() -> Result<Self> {
        let directory = log_directory().unwrap_or_else(|| PathBuf::from("logs"));
        Self::open_in(&directory)
    }

    fn open_in(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory).with_context(|| {
            format!("Could not create the log folder at {}", directory.display())
        })?;
        let time = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let pid = process::id();

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        for sequence in 0..100 {
            let path = directory.join(format!("live-{time}-{pid}-{sequence}.log"));
            match options.open(&path) {
                Ok(file) => return Ok(Self { file, path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Could not create a log at {}", path.display()));
                }
            }
        }
        bail!(
            "Could not choose a unique log file in {}",
            directory.display()
        );
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn event(&mut self, message: impl Display) -> io::Result<()> {
        let message: String = message
            .to_string()
            .chars()
            .map(|character| {
                if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        let time = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        self.file
            .write_all(format!("{time} {message}\n").as_bytes())
    }
}

#[cfg(target_os = "windows")]
fn log_directory() -> Option<PathBuf> {
    let local = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::home_dir().map(|home| home.join("AppData").join("Local")))?;
    Some(local.join("NameMCSniper").join("logs"))
}

#[cfg(target_os = "macos")]
fn log_directory() -> Option<PathBuf> {
    Some(env::home_dir()?.join("Library/Logs/NameMCSniper"))
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn log_directory() -> Option<PathBuf> {
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::home_dir().map(|home| home.join(".local/state")))?;
    Some(state.join("namemcsniper/logs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    #[test]
    fn creates_separate_files_and_keeps_events_on_one_line() {
        let directory = env::temp_dir().join(format!(
            "namemcsniper-run-log-{}-{}",
            process::id(),
            Utc::now().timestamp_micros()
        ));
        let mut first = RunLog::open_in(&directory).unwrap();
        let mut second = RunLog::open_in(&directory).unwrap();
        first.event("Started\r\nready\x1b\u{2028}end").unwrap();
        second.event("Another run").unwrap();
        let first_path = first.path().to_owned();
        let second_path = second.path().to_owned();
        drop(first);
        drop(second);
        let first_text = fs::read_to_string(&first_path).unwrap();
        let second_text = fs::read_to_string(&second_path).unwrap();
        fs::remove_file(&first_path).unwrap();
        fs::remove_file(&second_path).unwrap();
        fs::remove_dir(&directory).unwrap();

        let (timestamp, message) = first_text.trim_end().split_once(' ').unwrap();
        assert!(DateTime::parse_from_rfc3339(timestamp).is_ok());
        assert_eq!(message, "Started  ready  end");
        assert!(second_text.ends_with(" Another run\n"));
    }
}
