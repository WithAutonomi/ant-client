//! Following a node's log files as they are written and rotated.
//!
//! ant-node rotates **daily by filename** (`ant-node.YYYY-MM-DD.log`) and prunes to
//! `--log-max-files`, so "rotation" here is a new file appearing beside the old one rather than a
//! cursor moving — which makes the tailer's job mostly bookkeeping over a sorted file list.
//!
//! Two behaviours are worth knowing about before reading the code:
//!
//! - **A partially written line is never emitted.** Only bytes up to the last newline in a chunk
//!   are consumed, so an event that is still being written is picked up whole on the next poll.
//! - **A multi-line event is not split across polls.** The last event of a chunk is held back and
//!   the stored offset stays at its first byte, so its continuation lines — a panic and its
//!   backtrace, most importantly — join it rather than becoming orphans. It is released once the
//!   file stops growing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::config::LogLevel;
use super::offsets::OffsetStore;
use super::parse::{parse_line, LogEvent};
use crate::error::Result;

/// Filename prefix ant-node's rolling appender uses.
const LOG_FILENAME_PREFIX: &str = "ant-node.";
/// Filename suffix ant-node's rolling appender uses.
const LOG_FILENAME_SUFFIX: &str = ".log";

/// Most bytes read from one file in one poll, bounding the forwarder's memory when it is catching
/// up on a node that logged heavily while the daemon was down.
const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// An event together with where it came from — everything the sink needs to build a stable `_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailedEvent {
    pub node_id: u32,
    /// Bare log filename, e.g. `ant-node.2026-08-19.log`.
    pub file_name: String,
    /// Byte position of the event's first line within that file.
    pub byte_offset: u64,
    pub event: LogEvent,
}

impl TailedEvent {
    /// The document `_id` this event will be written under.
    ///
    /// Deterministic in the three things that identify the event's position in the world — which
    /// node, which file, which byte — so that replaying a batch after a transport failure lands on
    /// the same `_id` and is rejected as a duplicate instead of writing a second copy.
    #[must_use]
    pub fn document_id(&self) -> String {
        format!("{}-{}-{}", self.node_id, self.file_name, self.byte_offset)
    }
}

/// Follows one node's log directory.
pub struct LogTailer {
    node_id: u32,
    log_dir: PathBuf,
    /// False until the first poll has run. On that first poll, files already on disk are joined at
    /// their end: enabling forwarding is a forward-looking consent, not a request to upload up to a
    /// week of retained history.
    primed: bool,
    /// Length each file had at the previous poll, used to tell "still being written" from
    /// "finished", so a held-back multi-line event is released once the file goes quiet.
    last_seen_len: HashMap<String, u64>,
}

impl LogTailer {
    #[must_use]
    pub fn new(node_id: u32, log_dir: PathBuf) -> Self {
        Self {
            node_id,
            log_dir,
            primed: false,
            last_seen_len: HashMap::new(),
        }
    }

    #[must_use]
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    #[must_use]
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// Adopt existing offsets rather than joining at the end.
    ///
    /// Called when the persisted offsets already mention this node, i.e. the daemon is restarting
    /// rather than the user enabling forwarding for the first time.
    pub fn mark_primed(&mut self) {
        self.primed = true;
    }

    /// Read whatever has been appended since the last poll.
    ///
    /// Events below `min_level` are dropped here rather than downstream, so they never occupy queue
    /// space: the endpoint discards them on arrival anyway, and shipping them would spend the
    /// user's bandwidth for nothing.
    pub async fn poll(
        &mut self,
        offsets: &mut OffsetStore,
        min_level: LogLevel,
    ) -> Result<PollOutcome> {
        let files = self.discover_files().await?;
        let mut outcome = PollOutcome::default();

        for path in &files {
            match self.poll_file(path, offsets, min_level).await {
                Ok(mut file_outcome) => {
                    outcome.events.append(&mut file_outcome.events);
                    outcome.dropped_by_level += file_outcome.dropped_by_level;
                }
                // A file vanishing mid-poll is retention doing its job, not an error worth
                // stopping the whole forwarder for.
                Err(error) => {
                    tracing::debug!(
                        "log forwarding: skipping {} this poll: {error}",
                        path.display()
                    );
                }
            }
        }

        let live: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        offsets.prune(&live);
        self.last_seen_len.retain(|key, _| live.contains(key));
        self.primed = true;

        Ok(outcome)
    }

    /// List this node's log files, oldest first.
    ///
    /// The rolling appender's `ant-node.YYYY-MM-DD.log` names sort chronologically under a plain
    /// lexicographic sort, so no date parsing is needed to get the order right.
    async fn discover_files(&self) -> Result<Vec<PathBuf>> {
        let mut entries = match tokio::fs::read_dir(&self.log_dir).await {
            Ok(entries) => entries,
            // The directory not existing yet is normal: it is created when the node first starts.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(LOG_FILENAME_PREFIX) && name.ends_with(LOG_FILENAME_SUFFIX) {
                files.push(entry.path());
            }
        }
        files.sort();
        Ok(files)
    }

    async fn poll_file(
        &mut self,
        path: &Path,
        offsets: &mut OffsetStore,
        min_level: LogLevel,
    ) -> Result<PollOutcome> {
        let key = path.display().to_string();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| key.clone());

        let mut file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();

        let mut start = match offsets.get(&key) {
            Some(offset) => offset,
            // A file we have never read: join at the end on the very first poll after enabling,
            // otherwise (a new day's file, or a node added later) read it from the beginning.
            None if !self.primed => len,
            None => 0,
        };

        // The file shrank, so it was truncated or replaced under us. Anything we thought we had
        // read is gone; the only safe cursor is the beginning.
        if len < start {
            tracing::debug!("log forwarding: {key} shrank; restarting from the beginning");
            start = 0;
        }

        let was_growing = self.last_seen_len.insert(key.clone(), len) != Some(len);

        if len == start {
            offsets.set(&key, start);
            return Ok(PollOutcome::default());
        }

        let to_read = usize::try_from(len - start)
            .unwrap_or(MAX_CHUNK_BYTES)
            .min(MAX_CHUNK_BYTES);
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut buffer = vec![0u8; to_read];
        let read = file.read_exact(&mut buffer).await.map(|_| to_read)?;
        buffer.truncate(read);

        let reached_eof = start + read as u64 >= len;

        // Never emit a half-written line. If the chunk has no newline at all we are either mid-line
        // or looking at a single line longer than the read cap; in the latter case, waiting forever
        // would stall the file, so an over-long line is taken as-is.
        let usable = match buffer.iter().rposition(|b| *b == b'\n') {
            Some(index) => index + 1,
            None if read == MAX_CHUNK_BYTES => read,
            None => {
                offsets.set(&key, start);
                return Ok(PollOutcome::default());
            }
        };

        let text = String::from_utf8_lossy(&buffer[..usable]).to_string();
        let outcome = self.collect_events(
            &text,
            start,
            &file_name,
            min_level,
            // Hold the final event back while the file is still growing, so its continuation lines
            // can join it on the next poll.
            reached_eof && !was_growing,
        );

        offsets.set(&key, outcome.next_offset);
        Ok(outcome.into_poll_outcome())
    }

    /// Split a chunk into events, attaching continuation lines to the event above them.
    fn collect_events(
        &self,
        text: &str,
        chunk_start: u64,
        file_name: &str,
        min_level: LogLevel,
        release_final_event: bool,
    ) -> CollectOutcome {
        let mut outcome = CollectOutcome {
            next_offset: chunk_start,
            ..CollectOutcome::default()
        };
        let mut pending: Option<TailedEvent> = None;
        let mut cursor = chunk_start;

        for line in text.split_inclusive('\n') {
            let line_start = cursor;
            cursor += line.len() as u64;
            let content = line.trim_end_matches(['\n', '\r']);

            match parse_line(content) {
                Some(event) => {
                    if let Some(previous) = pending.take() {
                        outcome.push(previous, min_level);
                    }
                    // Everything before this event has now been emitted, so the cursor may safely
                    // advance to its first byte — and no further, until it too is released.
                    outcome.next_offset = line_start;
                    pending = Some(TailedEvent {
                        node_id: self.node_id,
                        file_name: file_name.to_string(),
                        byte_offset: line_start,
                        event,
                    });
                }
                None => match pending.as_mut() {
                    Some(held) => held.event.push_continuation(content),
                    // A continuation with nothing above it: the parent was emitted in an earlier
                    // poll, or the file began mid-event. Nothing useful to attach it to.
                    None => outcome.next_offset = cursor,
                },
            }
        }

        match pending {
            Some(event) if release_final_event => {
                outcome.push(event, min_level);
                outcome.next_offset = cursor;
            }
            // Left pending: `next_offset` still points at its first byte, so the next poll re-reads
            // it along with whatever continuation lines have since arrived.
            Some(_) => {}
            None => outcome.next_offset = cursor,
        }

        outcome
    }
}

/// What one poll produced.
#[derive(Debug, Default)]
pub struct PollOutcome {
    pub events: Vec<TailedEvent>,
    /// Events discarded for being below the configured minimum level.
    pub dropped_by_level: u64,
}

#[derive(Debug, Default)]
struct CollectOutcome {
    events: Vec<TailedEvent>,
    dropped_by_level: u64,
    next_offset: u64,
}

impl CollectOutcome {
    fn push(&mut self, event: TailedEvent, min_level: LogLevel) {
        if event.event.level < min_level {
            self.dropped_by_level += 1;
        } else {
            self.events.push(event);
        }
    }

    fn into_poll_outcome(self) -> PollOutcome {
        PollOutcome {
            events: self.events,
            dropped_by_level: self.dropped_by_level,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(level: &str, message: &str) -> String {
        format!("2026-08-19T20:50:00.123456Z  {level} ant_node::node: {message}\n")
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        log_dir: PathBuf,
        offsets_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log_dir = dir.path().join("logs");
            std::fs::create_dir_all(&log_dir).unwrap();
            let offsets_path = dir.path().join("offsets.json");
            Self {
                _dir: dir,
                log_dir,
                offsets_path,
            }
        }

        fn append(&self, file_name: &str, contents: &str) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.log_dir.join(file_name))
                .unwrap();
            file.write_all(contents.as_bytes()).unwrap();
        }

        fn offsets(&self) -> OffsetStore {
            OffsetStore::load(&self.offsets_path)
        }

        fn tailer(&self) -> LogTailer {
            LogTailer::new(7, self.log_dir.clone())
        }
    }

    /// Poll until the file stops growing, merging what comes out.
    ///
    /// The tailer holds a growing file's final event back for one poll so that continuation lines
    /// written just after it can join it, so observing an event that was only just appended takes
    /// two polls. That is the intended trade — one poll interval of latency on the tail of a burst,
    /// in exchange for panics arriving as one document instead of twenty.
    async fn drain(
        tailer: &mut LogTailer,
        offsets: &mut OffsetStore,
        min_level: LogLevel,
    ) -> PollOutcome {
        let mut merged = tailer.poll(offsets, min_level).await.unwrap();
        let mut second = tailer.poll(offsets, min_level).await.unwrap();
        merged.events.append(&mut second.events);
        merged.dropped_by_level += second.dropped_by_level;
        merged
    }

    /// Enabling forwarding must not upload the retained backlog: the first poll joins at the end.
    #[tokio::test]
    async fn the_first_poll_joins_existing_files_at_their_end() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "historic"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        assert!(outcome.events.is_empty(), "history must not be shipped");

        fixture.append("ant-node.2026-08-19.log", &line("INFO", "fresh"));
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "fresh");
    }

    #[tokio::test]
    async fn a_restart_resumes_from_the_persisted_offset_without_duplicating() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "first"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "second"));
        let before = drain(&mut tailer, &mut offsets, LogLevel::Info).await;
        assert_eq!(before.events.len(), 1);
        offsets.save().unwrap();

        // A new daemon: fresh tailer, offsets reloaded from disk.
        let mut restarted = fixture.tailer();
        restarted.mark_primed();
        let mut reloaded = fixture.offsets();

        let outcome = drain(&mut restarted, &mut reloaded, LogLevel::Info).await;
        assert!(outcome.events.is_empty(), "nothing new, nothing re-sent");

        fixture.append("ant-node.2026-08-19.log", &line("INFO", "third"));
        let outcome = drain(&mut restarted, &mut reloaded, LogLevel::Info).await;
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "third");
    }

    /// The gap half of "no duplication and no large gaps": lines written while the daemon was down
    /// are still delivered, because the offset is behind them.
    #[tokio::test]
    async fn lines_written_while_the_daemon_was_down_are_not_lost() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "before"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        offsets.save().unwrap();

        fixture.append("ant-node.2026-08-19.log", &line("INFO", "during downtime"));

        let mut restarted = fixture.tailer();
        restarted.mark_primed();
        let mut reloaded = fixture.offsets();
        let outcome = drain(&mut restarted, &mut reloaded, LogLevel::Info).await;

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "during downtime");
    }

    /// Enabling forwarding *before* the node starts captures its whole first log file, including
    /// the startup line carrying version, commit and peer id.
    ///
    /// The end-join rule only applies to files that already existed when forwarding was switched
    /// on. A node that has not run yet has no files, so nothing is joined at the end, and the file
    /// it later creates is read from byte zero like any other new file.
    #[tokio::test]
    async fn a_node_started_after_enabling_is_captured_from_its_first_line() {
        let fixture = Fixture::new();

        // Forwarding is enabled while the node has never run: the log directory is empty.
        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        assert!(outcome.events.is_empty());

        // The node now starts and writes its startup line.
        fixture.append(
            "ant-node.2026-08-19.log",
            &format!(
                "{}{}",
                line("INFO", "starting version=0.17.2 commit=abc1234"),
                line("INFO", "listening for connections"),
            ),
        );
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;

        let messages: Vec<&str> = outcome
            .events
            .iter()
            .map(|e| e.event.message.as_str())
            .collect();
        assert_eq!(
            messages,
            vec![
                "starting version=0.17.2 commit=abc1234",
                "listening for connections"
            ],
            "the node's first line must not be skipped"
        );
        assert_eq!(outcome.events[0].byte_offset, 0);
        assert_eq!(outcome.events[0].event.version.as_deref(), Some("0.17.2"));
        assert_eq!(outcome.events[0].event.commit.as_deref(), Some("abc1234"));
    }

    /// The converse: enabling *after* the node is already running skips whatever it logged before
    /// consent — including its startup line, and so the version/commit fields that come with it.
    #[tokio::test]
    async fn enabling_after_the_node_started_skips_its_startup_line() {
        let fixture = Fixture::new();
        fixture.append(
            "ant-node.2026-08-19.log",
            &line("INFO", "starting version=0.17.2 commit=abc1234"),
        );

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;
        assert!(
            outcome.events.is_empty(),
            "pre-consent lines are not uploaded"
        );

        fixture.append("ant-node.2026-08-19.log", &line("INFO", "later activity"));
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;

        let messages: Vec<&str> = outcome
            .events
            .iter()
            .map(|e| e.event.message.as_str())
            .collect();
        assert_eq!(messages, vec!["later activity"]);
    }

    #[tokio::test]
    async fn a_new_days_file_is_read_from_the_beginning() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "yesterday"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        fixture.append("ant-node.2026-08-20.log", &line("INFO", "today"));
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "today");
        assert_eq!(outcome.events[0].file_name, "ant-node.2026-08-20.log");
    }

    #[tokio::test]
    async fn a_truncated_file_restarts_from_the_beginning() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "original content"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        std::fs::write(
            fixture.log_dir.join("ant-node.2026-08-19.log"),
            line("INFO", "new"),
        )
        .unwrap();
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "new");
    }

    #[tokio::test]
    async fn a_partially_written_line_is_held_until_it_is_complete() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "complete"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        fixture.append(
            "ant-node.2026-08-19.log",
            "2026-08-19T20:50:01.000000Z  INFO ant_node::node: half a li",
        );
        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        assert!(outcome.events.is_empty(), "a partial line is not an event");

        fixture.append("ant-node.2026-08-19.log", "ne here\n");
        // One poll observes the new length; the next releases the now-quiet final event.
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event.message, "half a line here");
    }

    #[tokio::test]
    async fn a_panic_and_its_backtrace_stay_one_event() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "before the panic"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        fixture.append(
            "ant-node.2026-08-19.log",
            &format!(
                "{}thread 'main' panicked\n  at src/node.rs:42\n",
                line("ERROR", "it broke")
            ),
        );
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        assert_eq!(outcome.events.len(), 1);
        assert_eq!(
            outcome.events[0].event.message,
            "it broke\nthread 'main' panicked\n  at src/node.rs:42"
        );
    }

    #[tokio::test]
    async fn events_below_the_minimum_level_are_dropped_and_counted() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "seed"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        fixture.append(
            "ant-node.2026-08-19.log",
            &format!(
                "{}{}{}",
                line("DEBUG", "chatter"),
                line("TRACE", "more chatter"),
                line("WARN", "worth keeping")
            ),
        );
        let outcome = drain(&mut tailer, &mut offsets, LogLevel::Info).await;

        let messages: Vec<&str> = outcome
            .events
            .iter()
            .map(|e| e.event.message.as_str())
            .collect();
        assert_eq!(messages, vec!["worth keeping"]);
        assert_eq!(outcome.dropped_by_level, 2);
    }

    #[tokio::test]
    async fn a_missing_log_directory_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut tailer = LogTailer::new(1, dir.path().join("never-created"));
        let mut offsets = OffsetStore::load(&dir.path().join("offsets.json"));

        let outcome = tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        assert!(outcome.events.is_empty());
    }

    #[tokio::test]
    async fn unrelated_files_in_the_log_directory_are_ignored() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "seed"));
        fixture.append("notes.txt", "not a log file\n");
        fixture.append("ant-node.2026-08-19.log.gz", "compressed\n");

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        assert_eq!(offsets.len(), 1, "only the rolling log file is tracked");
    }

    #[tokio::test]
    async fn offsets_for_retention_deleted_files_are_pruned() {
        let fixture = Fixture::new();
        fixture.append("ant-node.2026-08-18.log", &line("INFO", "old"));
        fixture.append("ant-node.2026-08-19.log", &line("INFO", "current"));

        let mut tailer = fixture.tailer();
        let mut offsets = fixture.offsets();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();
        assert_eq!(offsets.len(), 2);

        std::fs::remove_file(fixture.log_dir.join("ant-node.2026-08-18.log")).unwrap();
        tailer.poll(&mut offsets, LogLevel::Info).await.unwrap();

        assert_eq!(offsets.len(), 1);
    }

    #[test]
    fn the_document_id_is_stable_and_position_derived() {
        let event = TailedEvent {
            node_id: 7,
            file_name: "ant-node.2026-08-19.log".to_string(),
            byte_offset: 104_857,
            event: parse_line(&line("INFO", "hello")).unwrap(),
        };

        assert_eq!(event.document_id(), "7-ant-node.2026-08-19.log-104857");
        assert_eq!(event.document_id(), event.clone().document_id());
        assert!(
            event.document_id().len() <= 512,
            "Elasticsearch caps _id at 512 bytes"
        );
    }

    #[test]
    fn document_ids_differ_across_nodes_files_and_positions() {
        let base = TailedEvent {
            node_id: 7,
            file_name: "ant-node.2026-08-19.log".to_string(),
            byte_offset: 100,
            event: parse_line(&line("INFO", "hello")).unwrap(),
        };
        let other_node = TailedEvent {
            node_id: 8,
            ..base.clone()
        };
        let other_file = TailedEvent {
            file_name: "ant-node.2026-08-20.log".to_string(),
            ..base.clone()
        };
        let other_offset = TailedEvent {
            byte_offset: 200,
            ..base.clone()
        };

        let ids = [
            base.document_id(),
            other_node.document_id(),
            other_file.document_id(),
            other_offset.document_id(),
        ];
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), 4);
    }
}
