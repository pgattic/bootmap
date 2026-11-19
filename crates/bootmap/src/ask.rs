//! systemd password agent: answers passphrase requests (such as a LUKS
//! unlock) from the keyboard, with the prompt drawn over the graph.
//!
//! Protocol: <https://systemd.io/PASSWORD_AGENTS/>. Whoever needs a password
//! drops an `ask.*` file into /run/systemd/ask-password naming a socket; an
//! agent sends "+<password>" to that socket. The request file disappears
//! once it is answered or has timed out.

use bootmap_core::render::Prompt;
use std::collections::HashSet;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ASK_DIR: &str = "/run/systemd/ask-password";
const SCAN_EVERY: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq)]
struct Request {
    path: PathBuf,
    socket: PathBuf,
    message: String,
    echo: bool,
}

/// Parse an `ask.*` file. `None` if it is not (yet) a complete request.
fn parse(path: &Path, text: &str) -> Option<Request> {
    let mut socket = None;
    let mut message = String::new();
    let mut echo = false;
    let mut in_ask = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_ask = line == "[Ask]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !in_ask {
            continue;
        }
        match key {
            "Socket" => socket = Some(PathBuf::from(value)),
            "Message" => message = value.to_string(),
            "Echo" => echo = value == "1",
            _ => {}
        }
    }
    Some(Request {
        path: path.to_path_buf(),
        socket: socket?,
        message,
        echo,
    })
}

/// Is anything asking for a passphrase right now?
pub fn pending() -> bool {
    std::fs::read_dir(ASK_DIR)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ask."))
        })
        .unwrap_or(false)
}

pub struct Agent {
    dir: PathBuf,
    /// The request being shown.
    current: Option<Request>,
    /// What has been typed so far, as the terminal delivered it (UTF-8).
    typed: Vec<u8>,
    /// Requests we answered; ignored until systemd removes them.
    answered: HashSet<PathBuf>,
    /// Message of the request answered last: the same question again means
    /// the answer was wrong.
    last_message: Option<String>,
    retry: bool,
    last_scan: Option<Instant>,
}

impl Agent {
    pub fn new() -> Self {
        Self::watching(ASK_DIR)
    }

    fn watching(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            current: None,
            typed: vec![],
            answered: HashSet::new(),
            last_message: None,
            retry: false,
            last_scan: None,
        }
    }

    /// Look for requests (cheap; rate-limited internally).
    pub fn poll(&mut self) {
        if self.last_scan.is_some_and(|t| t.elapsed() < SCAN_EVERY) {
            return;
        }
        self.last_scan = Some(Instant::now());
        self.scan();
    }

    fn scan(&mut self) {
        let mut pending: Vec<Request> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.file_name()
                            .is_some_and(|n| n.to_string_lossy().starts_with("ask."))
                    })
                    .filter_map(|p| parse(&p, &std::fs::read_to_string(&p).ok()?))
                    .collect()
            })
            .unwrap_or_default();
        pending.sort_by(|a, b| a.path.cmp(&b.path));
        self.answered
            .retain(|p| pending.iter().any(|r| &r.path == p));
        // Keep showing the current request while it is still pending.
        let still = self
            .current
            .as_ref()
            .is_some_and(|c| pending.iter().any(|r| r.path == c.path));
        if still {
            return;
        }
        if self.current.take().is_some() {
            // Gone without our answer: timed out, or another agent answered.
            self.clear_typed();
        }
        self.current = pending
            .into_iter()
            .find(|r| !self.answered.contains(&r.path));
        if let Some(c) = &self.current {
            eprintln!("bootmap: asking for a passphrase");
            self.retry = self.last_message.as_deref() == Some(c.message.as_str());
        }
    }

    /// Feed keys typed on the terminal.
    pub fn keys(&mut self, bytes: &[u8]) {
        if self.current.is_none() {
            return;
        }
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            match b {
                b'\r' | b'\n' => {
                    self.submit();
                    return;
                }
                // Backspace: drop one character, which may be several bytes.
                0x7f | 0x08 => while self.typed.pop().is_some_and(|b| b & 0xC0 == 0x80) {},
                // Ctrl-U: start over.
                0x15 => self.clear_typed(),
                // Escape alone starts over; as the start of a key sequence
                // (arrows, function keys) the whole sequence is ignored.
                0x1b => {
                    if i == bytes.len() {
                        self.clear_typed();
                    }
                    while i < bytes.len() && !bytes[i].is_ascii_alphabetic() && bytes[i] != b'~' {
                        i += 1;
                    }
                    i = (i + 1).min(bytes.len());
                }
                b if b < 0x20 => {}
                b => self.typed.push(b),
            }
        }
    }

    fn submit(&mut self) {
        let Some(req) = self.current.take() else {
            return;
        };
        let mut packet = Vec::with_capacity(self.typed.len() + 1);
        packet.push(b'+');
        packet.extend_from_slice(&self.typed);
        let sent = UnixDatagram::unbound().and_then(|s| s.send_to(&packet, &req.socket));
        packet.fill(0);
        self.clear_typed();
        match sent {
            Ok(_) => {
                eprintln!("bootmap: answered a passphrase request");
                self.last_message = Some(req.message.clone());
                self.answered.insert(req.path);
            }
            Err(e) => {
                eprintln!("bootmap: could not answer the password request: {e}");
                self.current = Some(req);
            }
        }
        // Look again right away: a wrong passphrase is asked for again.
        self.last_scan = None;
    }

    fn clear_typed(&mut self) {
        self.typed.fill(0);
        self.typed.clear();
    }

    /// What to draw, if a request is pending.
    pub fn prompt(&self) -> Option<Prompt> {
        let req = self.current.as_ref()?;
        let chars = self.typed.iter().filter(|b| *b & 0xC0 != 0x80).count();
        Some(Prompt {
            message: if req.message.is_empty() {
                "Password:".into()
            } else {
                req.message.clone()
            },
            entered: if req.echo {
                String::from_utf8_lossy(&self.typed).into_owned()
            } else {
                "•".repeat(chars)
            },
            note: self
                .retry
                .then(|| "That did not work. Try again.".to_string()),
        })
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.clear_typed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake requester: a directory with one ask file and its socket.
    struct Asker {
        dir: PathBuf,
        socket: UnixDatagram,
    }

    impl Asker {
        fn new(name: &str, message: &str, echo: bool) -> Self {
            let dir =
                std::env::temp_dir().join(format!("bootmap-ask-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("sck.1");
            let socket = UnixDatagram::bind(&sock).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let me = Self { dir, socket };
            me.ask("ask.1", message, echo);
            me
        }

        fn ask(&self, file: &str, message: &str, echo: bool) {
            let text = format!(
                "[Ask]\nPID=1\nSocket={}\nAcceptCached=0\nEcho={}\nNotAfter=0\nMessage={message}\nId=test\n",
                self.dir.join("sck.1").display(),
                echo as u8
            );
            std::fs::write(self.dir.join(file), text).unwrap();
        }

        fn received(&self) -> Vec<u8> {
            let mut buf = [0u8; 256];
            let n = self.socket.recv(&mut buf).unwrap();
            buf[..n].to_vec()
        }
    }

    impl Drop for Asker {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn answers_a_request_with_what_was_typed() {
        let asker = Asker::new("answer", "Please enter passphrase for disk secret:", false);
        let mut agent = Agent::watching(&asker.dir);
        assert!(agent.prompt().is_none());
        agent.scan();
        let p = agent.prompt().unwrap();
        assert_eq!(p.message, "Please enter passphrase for disk secret:");
        assert_eq!(p.entered, "");

        // Typo, backspace over a two-byte character, an arrow key, then Enter.
        agent.keys("hunteé".as_bytes());
        assert_eq!(agent.prompt().unwrap().entered, "••••••");
        agent.keys(&[0x7f]);
        agent.keys(b"\x1b[C");
        agent.keys(b"r2\n");
        assert_eq!(asker.received(), b"+hunter2");
        assert!(
            agent.prompt().is_none(),
            "answered request is not shown again"
        );
        agent.scan();
        assert!(agent.prompt().is_none(), "even while its file still exists");
    }

    #[test]
    fn asked_again_means_try_again() {
        let asker = Asker::new("retry", "Passphrase:", true);
        let mut agent = Agent::watching(&asker.dir);
        agent.scan();
        agent.keys(b"wrong\n");
        assert_eq!(asker.received(), b"+wrong");
        // systemd removes the request and asks again.
        std::fs::remove_file(asker.dir.join("ask.1")).unwrap();
        asker.ask("ask.2", "Passphrase:", true);
        agent.scan();
        let p = agent.prompt().unwrap();
        assert!(p.note.is_some());
        agent.keys(b"ab");
        assert_eq!(
            agent.prompt().unwrap().entered,
            "ab",
            "Echo=1 shows the text"
        );
        // Escape starts over.
        agent.keys(&[0x1b]);
        assert_eq!(agent.prompt().unwrap().entered, "");
    }

    #[test]
    fn a_request_that_goes_away_drops_what_was_typed() {
        let asker = Asker::new("gone", "Passphrase:", false);
        let mut agent = Agent::watching(&asker.dir);
        agent.scan();
        agent.keys(b"half");
        std::fs::remove_file(asker.dir.join("ask.1")).unwrap();
        agent.scan();
        assert!(agent.prompt().is_none());
        asker.ask("ask.3", "Other:", false);
        agent.scan();
        assert_eq!(agent.prompt().unwrap().entered, "");
    }
}
