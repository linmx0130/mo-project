//! Unit tests for the `tools::bash` module — production code lives in
//! `mo_worker/src/tools/bash.rs`. Wired from there with `#[cfg(test)] #[path = "../tests/bash_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;
use std::sync::Arc;

fn noop_sink() -> impl Fn(JournalEventKind) {
    |_: JournalEventKind| {}
}

/// A sink that collects the streamed `tool_output_delta` chunks. A
/// `Mutex` keeps the closure `Fn` (the shared-sink signature requires
/// it, not `FnMut`).
fn collecting_sink() -> (Arc<Mutex<Vec<String>>>, impl Fn(JournalEventKind)) {
    let collected = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = {
        let collected = Arc::clone(&collected);
        move |kind: JournalEventKind| {
            if let JournalEventKind::ToolOutputDelta { output, .. } = kind {
                collected
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(output);
            }
        }
    };
    (collected, sink)
}

#[tokio::test]
async fn runs_command_and_reports_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let out = bash(
        dir.path(),
        "echo hello && exit 3",
        Duration::from_secs(10),
        "call_1",
        &sink,
    )
    .await
    .unwrap();
    assert!(out.contains("hello"), "got: {out}");
    assert!(out.contains("exit code: 3"), "got: {out}");
}

#[tokio::test]
async fn captures_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let out = bash(
        dir.path(),
        "echo boom 1>&2",
        Duration::from_secs(10),
        "call_1",
        &sink,
    )
    .await
    .unwrap();
    assert!(out.contains("boom"), "got: {out}");
}

#[tokio::test]
async fn streams_output_chunks_live() {
    let dir = tempfile::tempdir().unwrap();
    let (collected, sink) = collecting_sink();
    let out = bash(
        dir.path(),
        "printf 'one\\ntwo\\n'; printf 'boom' 1>&2",
        Duration::from_secs(10),
        "call_1",
        &sink,
    )
    .await
    .unwrap();
    assert!(out.contains("one"), "got: {out}");
    assert!(out.contains("boom"), "got: {out}");
    // Every chunk was streamed through the sink, tagged with the tool id.
    let joined = collected.lock().unwrap_or_else(|e| e.into_inner()).concat();
    assert!(joined.contains("one"), "streamed: {joined}");
    assert!(joined.contains("two"), "streamed: {joined}");
    assert!(joined.contains("boom"), "streamed: {joined}");
}

#[tokio::test]
async fn times_out_and_kills() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let err = bash(
        dir.path(),
        "sleep 30",
        Duration::from_millis(300),
        "call_1",
        &sink,
    )
    .await;
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("timed out"));
}

#[tokio::test]
async fn timeout_kills_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let err = bash(
        dir.path(),
        "sleep 30 & echo $!; wait",
        Duration::from_millis(300),
        "call_1",
        &sink,
    )
    .await;
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("timed out"));

    // The backgrounded `sleep 30` shares the bash child's process group,
    // so the timeout must have killed it too — no orphan survives.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("pgrep -f '^sleep 30$' || true")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "orphaned sleep survived the timeout: {stdout}"
    );
}

/// Two concurrent bash commands genuinely overlap: two `sleep`s that
/// would take 0.9s back-to-back finish in ~0.6s when run together
/// (tokio::join! polls both futures concurrently), and each stream is
/// tagged with its own call id.
#[tokio::test]
async fn concurrent_commands_run_in_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let start = std::time::Instant::now();
    let (a, b) = tokio::join!(
        bash(
            dir.path(),
            "sleep 0.6; echo slow",
            Duration::from_secs(10),
            "call_a",
            &sink,
        ),
        bash(
            dir.path(),
            "sleep 0.3; echo fast",
            Duration::from_secs(10),
            "call_b",
            &sink,
        )
    );
    let elapsed = start.elapsed();
    assert!(a.as_ref().unwrap().contains("slow"), "got: {a:?}");
    assert!(b.as_ref().unwrap().contains("fast"), "got: {b:?}");
    // 0.6s + 0.3s = 0.9s if sequential; overlap keeps it near 0.6s.
    assert!(
        elapsed < Duration::from_millis(800),
        "commands did not overlap: {elapsed:?}"
    );
}

#[tokio::test]
async fn timeout_reports_partial_output() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let err = bash(
        dir.path(),
        "printf 'downloading deps...\\n'; sleep 30",
        Duration::from_millis(500),
        "call_1",
        &sink,
    )
    .await
    .unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
    // The output produced before the kill must reach the model.
    assert!(
        err.contains("downloading deps..."),
        "partial output missing from timeout error: {err}"
    );
}

#[tokio::test]
async fn timeout_with_no_output_hints_at_buffering() {
    let dir = tempfile::tempdir().unwrap();
    let sink = noop_sink();
    let err = bash(
        dir.path(),
        "sleep 30",
        Duration::from_millis(300),
        "call_1",
        &sink,
    )
    .await
    .unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
    assert!(
        err.contains("no output was received"),
        "missing buffering hint: {err}"
    );
}

#[tokio::test]
async fn full_output_streams_while_retained_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let total = OUTPUT_CAP + 256 * 1024; // ~1.25 MB
    let (collected, sink) = collecting_sink();
    let out = bash(
        dir.path(),
        &format!("head -c {total} /dev/zero | tr '\\0' 'x'"),
        Duration::from_secs(30),
        "call_1",
        &sink,
    )
    .await
    .unwrap();
    // Every byte streamed to the UI/journal...
    let streamed: usize = collected
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|s| s.len())
        .sum();
    assert!(streamed >= total, "streamed only {streamed} of {total}");
    // ...while the canonical result stays bounded at ~1 MB.
    assert!(
        out.len() < OUTPUT_CAP + 2048,
        "result too big: {}",
        out.len()
    );
    assert!(
        out.contains("output truncated"),
        "missing truncation note in result: {}",
        &out[out.len().saturating_sub(200)..]
    );
}

#[tokio::test]
async fn delta_stream_is_capped_with_marker() {
    let dir = tempfile::tempdir().unwrap();
    let total = DELTA_STREAM_CAP + 1024 * 1024;
    let (collected, sink) = collecting_sink();
    let out = bash(
        dir.path(),
        &format!("head -c {total} /dev/zero | tr '\\0' 'x'"),
        Duration::from_secs(30),
        "call_1",
        &sink,
    )
    .await
    .unwrap();
    // The marker replaced further deltas once the budget ran out.
    let streamed: usize = collected
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|s| s.len())
        .sum();
    assert!(
        streamed < DELTA_STREAM_CAP + 4096,
        "delta stream not capped: {streamed} bytes"
    );
    assert!(
        streamed >= DELTA_STREAM_CAP,
        "delta stream stopped early: {streamed} bytes"
    );
    // The retained result is still bounded at ~1 MB.
    assert!(out.len() < 2 * 1024 * 1024, "result too big: {}", out.len());
}
