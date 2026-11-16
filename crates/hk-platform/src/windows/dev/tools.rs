//! Running the developer tools themselves, like `docker` or `node --version`, with no window and
//! a time limit.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use hk_core::dev::ToolVersion;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// For asking something, like a version.
pub(super) const QUICK: Duration = Duration::from_secs(15);
/// For doing something that can take a while, like cleaning up.
pub(super) const SLOW: Duration = Duration::from_secs(10 * 60);

/// Runs a tool from the PATH. Tools that are batch files, like `npm`, are found too. Gives what it
/// wrote if it worked, or why it didn't.
pub(super) fn run(program: &str, arguments: &[&str], within: Duration) -> Result<String, String> {
    match run_exactly(program, arguments, within) {
        Err(Missing) => match run_exactly(&format!("{program}.cmd"), arguments, within) {
            Err(Missing) => Err(format!("{program} isn't installed")),
            Ok(result) => result,
        },
        Ok(result) => result,
    }
}

struct Missing;

fn run_exactly(
    program: &str,
    arguments: &[&str],
    within: Duration,
) -> Result<Result<String, String>, Missing> {
    let mut child = match Command::new(program)
        .args(arguments)
        .creation_flags(CREATE_NO_WINDOW.0)
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("NO_COLOR", "1")
        .env("DOCKER_CLI_HINTS", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(Missing),
        Err(error) => return Ok(Err(error.to_string())),
    };
    // Read as it comes, or a tool writing a lot would wait for room forever.
    let read = |stream: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut stream) = stream {
                let _ = stream.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let out = read(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = read(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= within => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break None,
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    Ok(match status {
        Some(status) if status.success() => Ok(out.trim().to_string()),
        Some(_) => Err(first_line(if err.trim().is_empty() { &out } else { &err })),
        None => Err(format!("{program} took too long")),
    })
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("it didn't work")
        .chars()
        .take(300)
        .collect()
}

/// The developer tools installed here, asked all at once.
pub(super) fn versions() -> Vec<ToolVersion> {
    type Read = fn(&str) -> Option<String>;
    let tools: [(&str, &str, &[&str], Read); 7] = [
        ("Node.js", "node", &["--version"], |text| {
            Some(text.trim_start_matches('v').to_string())
        }),
        ("npm", "npm", &["--version"], |text| Some(text.to_string())),
        ("pnpm", "pnpm", &["--version"], |text| {
            Some(text.to_string())
        }),
        (".NET SDK", "dotnet", &["--list-sdks"], |text| {
            let sdks: Vec<&str> = text
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .collect();
            (!sdks.is_empty()).then(|| sdks.join(", "))
        }),
        ("Rust", "rustc", &["--version"], |text| {
            text.split_whitespace().nth(1).map(str::to_string)
        }),
        ("Docker", "docker", &["--version"], |text| {
            text.split_whitespace()
                .nth(2)
                .map(|version| version.trim_end_matches(',').to_string())
        }),
        ("Git", "git", &["--version"], |text| {
            text.split_whitespace().nth(2).map(str::to_string)
        }),
    ];
    std::thread::scope(|scope| {
        let asked: Vec<_> = tools
            .iter()
            .map(|(tool, program, arguments, read)| {
                scope.spawn(move || ToolVersion {
                    tool: (*tool).to_string(),
                    version: run(program, arguments, QUICK)
                        .ok()
                        .and_then(|text| read(&text))
                        .filter(|version| !version.is_empty()),
                })
            })
            .collect();
        asked
            .into_iter()
            .filter_map(|asked| asked.join().ok())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_that_isnt_there_says_so() {
        assert_eq!(
            run("hackerman-no-such-tool", &["--version"], QUICK),
            Err("hackerman-no-such-tool isn't installed".into())
        );
    }

    /// Windows' own `where.exe` is always there.
    #[test]
    fn what_a_tool_writes_comes_back() {
        let found = run("where", &["cmd"], QUICK).unwrap();
        assert!(found.to_ascii_lowercase().contains("cmd.exe"), "{found}");
        assert!(run("where", &["hackerman-no-such-file"], QUICK).is_err());
    }

    #[test]
    fn a_tool_that_takes_too_long_is_stopped() {
        let started = Instant::now();
        let result = run(
            "ping",
            &["-n", "30", "127.0.0.1"],
            Duration::from_millis(500),
        );
        assert_eq!(result, Err("ping took too long".into()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Only looks. Run with `--nocapture` to see what's installed.
    #[test]
    fn the_versions_here_make_sense() {
        let tools = versions();
        println!("{tools:#?}");
        assert_eq!(tools.len(), 7);
        assert!(tools.iter().all(|tool| tool.version.as_deref() != Some("")));
    }
}
