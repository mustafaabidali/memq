use crate::error::{Error, Result};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct ProcessOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub timed_out: bool,
}

/// Bounded subprocess with concurrent pipe drains. Diagnostics never echo input
/// or stderr, which may contain configured provider credentials.
pub fn bounded(
    command: &mut Command,
    input: Vec<u8>,
    timeout: Duration,
    output_limit: usize,
) -> Result<ProcessOutput> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("pipe");
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stdout = child.stdout.take().expect("pipe");
    let stderr = child.stderr.take().expect("pipe");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take(output_limit as u64 + 1).read_to_end(&mut bytes);
        (bytes, result)
    });
    let errors = std::thread::spawn(move || {
        let mut drain = std::io::sink();
        let _ = std::io::copy(&mut stderr.take(1024 * 1024), &mut drain);
    });
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            timed_out = true;
            #[cfg(unix)]
            {
                // The child starts its own process group above. Kill only that
                // group so an SSH/credential subprocess cannot outlive the bound.
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    #[cfg(unix)]
    {
        // Reap pipes held by descendants even if the leader exited first.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = writer.join();
    let _ = errors.join();
    let (stdout, result) = reader
        .join()
        .map_err(|_| Error::new("process_error", "output reader failed"))?;
    result?;
    if stdout.len() > output_limit {
        return Err(Error::new(
            "process_error",
            "configured process exceeded its output bound",
        ));
    }
    Ok(ProcessOutput {
        success: status.success(),
        code: status.code(),
        stdout,
        timed_out,
    })
}
