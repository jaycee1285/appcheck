use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

/// Drain subprocess pipes while polling cancellation/deadline, avoiding pipe
/// capacity deadlocks. gh is reaped before its worker returns.
pub fn output(
    command: &mut std::process::Command,
    capture_stdout: bool,
    cancel: Option<&AtomicBool>,
) -> anyhow::Result<std::process::Output> {
    output_with_timeout(
        command,
        capture_stdout,
        cancel,
        Some(std::time::Duration::from_secs(120)),
        None,
    )
}

/// Package compilation has no useful fixed wall-clock ceiling. It remains
/// cancellable and is always killed/reaped when cancellation is requested.
pub fn output_long_running(
    command: &mut std::process::Command,
    capture_stdout: bool,
    cancel: Option<&AtomicBool>,
) -> anyhow::Result<std::process::Output> {
    output_with_timeout(command, capture_stdout, cancel, None, None)
}

/// Report complete stderr lines while a long-running process is active.
/// The full output is still returned for exact failure reporting.
pub fn output_long_running_progress(
    command: &mut std::process::Command,
    capture_stdout: bool,
    cancel: Option<&AtomicBool>,
    on_stderr: &dyn Fn(&str),
) -> anyhow::Result<std::process::Output> {
    output_with_timeout(command, capture_stdout, cancel, None, Some(on_stderr))
}

fn output_with_timeout(
    command: &mut std::process::Command,
    capture_stdout: bool,
    cancel: Option<&AtomicBool>,
    timeout: Option<std::time::Duration>,
    on_stderr: Option<&dyn Fn(&str)>,
) -> anyhow::Result<std::process::Output> {
    use std::{
        io::Read,
        process::Stdio,
        time::Instant,
    };
    if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
        anyhow::bail!("Operation cancelled");
    }
    if capture_stdout {
        command.stdout(Stdio::piped());
    }
    command.stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).map(|_| bytes)
        })
    });
    let (progress_tx, progress_rx) = mpsc::channel();
    let track_progress = on_stderr.is_some();
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut pending = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = pipe.read(&mut chunk)?;
                if count == 0 { break; }
                bytes.extend_from_slice(&chunk[..count]);
                if track_progress {
                    for byte in &chunk[..count] {
                        if *byte == b'\n' || *byte == b'\r' {
                            if !pending.is_empty() {
                                let line = String::from_utf8_lossy(&pending).trim().to_string();
                                if !line.is_empty() { let _ = progress_tx.send(line); }
                                pending.clear();
                            }
                        } else if pending.len() < 4096 {
                            pending.push(*byte);
                        }
                    }
                }
            }
            if track_progress && !pending.is_empty() {
                let line = String::from_utf8_lossy(&pending).trim().to_string();
                if !line.is_empty() { let _ = progress_tx.send(line); }
            }
            Ok::<_, std::io::Error>(bytes)
        })
    });
    let start = Instant::now();
    let (status, stopped) = loop {
        if let Some(callback) = on_stderr {
            while let Ok(line) = progress_rx.try_recv() { callback(&line); }
        }
        if let Some(status) = child.try_wait()? {
            break (status, None);
        }
        let stopped = if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            Some("Operation cancelled")
        } else if timeout.is_some_and(|timeout| start.elapsed() > timeout) {
            Some("External operation timed out after 120 seconds")
        } else {
            None
        };
        if stopped.is_some() {
            let _ = child.kill();
            break (child.wait()?, stopped);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    };
    let stdout = match stdout {
        Some(t) => t
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))??,
        None => vec![],
    };
    let stderr = match stderr {
        Some(t) => t
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))??,
        None => vec![],
    };
    if let Some(callback) = on_stderr {
        while let Ok(line) = progress_rx.try_recv() { callback(&line); }
    }
    if let Some(reason) = stopped {
        anyhow::bail!("{reason}");
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// One thread per *user of a repository*, not per download. Each item occupies
/// its repository until it finishes, so two items naming the same upstream run
/// one after the other while different upstreams run at once. These are small
/// binaries on a capable machine: the ceiling that matters is the forge and the
/// link, so nothing is capped beyond that. Results retain input order.
pub fn map_by<T: Sync, R: Send>(
    items: &[T],
    cancel: &AtomicBool,
    key: impl Fn(&T) -> String,
    run: impl Fn(&T) -> R + Sync,
) -> Vec<Option<R>> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let k = key(item);
        match groups.iter_mut().find(|(existing, _)| *existing == k) {
            Some((_, indices)) => indices.push(i),
            None => groups.push((k, vec![i])),
        }
    }
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for (_, indices) in &groups {
            let tx = tx.clone();
            let run = &run;
            scope.spawn(move || {
                for &i in indices {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    if tx.send((i, run(&items[i]))).is_err() {
                        return;
                    }
                }
            });
        }
    });
    drop(tx);
    let mut results: Vec<_> = (0..items.len()).map(|_| None).collect();
    for (i, result) in rx {
        results[i] = Some(result);
    }
    results
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    #[test]
    fn long_running_stderr_is_visible_before_process_finishes() {
        let cancel = AtomicBool::new(false);
        let seen = std::sync::Mutex::new(Vec::new());
        let started = std::time::Instant::now();
        let result = output_long_running_progress(
            std::process::Command::new("sh").args(["-c", "printf 'Compiling fixture\\n' >&2; exec sleep 5"]),
            true,
            Some(&cancel),
            &|line| { seen.lock().unwrap().push(line.to_string()); cancel.store(true, Ordering::Release); },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(*seen.lock().unwrap(), ["Compiling fixture"]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
    #[test]
    fn subprocess_is_cancelled_and_reaped() {
        let cancel = AtomicBool::new(false);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(100));
                cancel.store(true, Ordering::Release);
            });
            let result = output(
                std::process::Command::new("sleep").arg("5"),
                true,
                Some(&cancel),
            );
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        });
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }
    /// Every item runs at once: the barrier only clears if all 120 are in
    /// flight simultaneously, so any worker cap would deadlock this test.
    #[test]
    fn every_item_runs_concurrently_and_keeps_order() {
        let items: Vec<_> = (0..120).collect();
        let barrier = std::sync::Barrier::new(items.len());
        let results = map_by(
            &items,
            &AtomicBool::new(false),
            |i| i.to_string(),
            |i| {
                barrier.wait();
                i * 2
            },
        );
        assert_eq!(
            results,
            items.iter().map(|i| Some(i * 2)).collect::<Vec<_>>()
        );
    }
    /// One user per repository: items sharing an upstream run one at a time,
    /// different upstreams run at once. The barrier only clears if every
    /// distinct repo is in flight together, and the counter would exceed one if
    /// two users of the same repo ever overlapped.
    #[test]
    fn one_user_per_repository_at_a_time() {
        // Three repos; the first has three users, so 5 items across 3 repos.
        let items = [
            ("a", 0),
            ("a", 1),
            ("b", 2),
            ("a", 3),
            ("c", 4),
        ];
        let barrier = std::sync::Barrier::new(3);
        let in_repo: std::collections::HashMap<&str, AtomicUsize> =
            [("a", AtomicUsize::new(0)), ("b", AtomicUsize::new(0)), ("c", AtomicUsize::new(0))]
                .into_iter()
                .collect();
        let overlaps = AtomicUsize::new(0);
        let results = map_by(
            &items,
            &AtomicBool::new(false),
            |(repo, _)| repo.to_string(),
            |(repo, n)| {
                if in_repo[repo].fetch_add(1, Ordering::SeqCst) > 0 {
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                // Every repo must be running concurrently at least once.
                if *n == 0 || *n == 2 || *n == 4 {
                    barrier.wait();
                }
                in_repo[repo].fetch_sub(1, Ordering::SeqCst);
                n * 10
            },
        );
        assert_eq!(overlaps.load(Ordering::SeqCst), 0, "same repo ran twice at once");
        assert_eq!(
            results,
            items.iter().map(|(_, n)| Some(n * 10)).collect::<Vec<_>>()
        );
    }

    /// With one thread per item there is no queue to drain, so cancellation
    /// before the run skips everything; cancelling work already in flight is
    /// the subprocess deadline's job, covered by the test above.
    #[test]
    fn cancellation_before_start_runs_nothing() {
        let cancel = AtomicBool::new(true);
        let results = map_by(
            &(0..120).collect::<Vec<_>>(),
            &cancel,
            |i| i.to_string(),
            |i| *i,
        );
        assert_eq!(results.iter().flatten().count(), 0);
    }
}
