use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const HELP: &str = "Deployraft - run a deployment over SSH

Usage: deployraft [--config PATH] [--verbose] [--] APP

Options:
  --config PATH  Configuration file (default: ~/.config/deployraft/config.toml)
  --verbose      Print progress, output and the full command (may expose secrets)
  --             End options
  -h, --help     Show this help

An absolute XDG_CONFIG_HOME overrides ~/.config.
";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    servers: BTreeMap<String, Server>,
    apps: BTreeMap<String, App>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Server {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    user: String,
    auth: Auth,
    identity_file: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Auth {
    Password,
    Key,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct App {
    server: String,
    directory: String,
    command: String,
    at: Option<String>,
}

fn default_port() -> u16 {
    22
}

struct LogOutput {
    file: fs::File,
    verbose: bool,
}

impl Write for LogOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write_all(bytes)?;

        if self.verbose {
            io::stdout().write_all(bytes)?;
        }

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;

        if self.verbose {
            io::stdout().flush()?;
        }

        Ok(())
    }
}

fn create_log(app_name: &str, verbose: bool) -> Result<LogOutput, Box<dyn Error>> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .ok_or("cannot locate the log directory. Set an absolute HOME or XDG_STATE_HOME")?;
    let directory = base.join("deployraft/logs");
    fs::create_dir_all(&directory)?;
    let name: String = app_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let path = directory.join(format!(
        "{name}-{}-{:09}-{}.log",
        timestamp.as_secs(),
        timestamp.subsec_nanos(),
        std::process::id()
    ));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("cannot create log {}: {error}", path.display()))?;
    let mut output = LogOutput { file, verbose };
    writeln!(output, "Log: {}", path.display())?;
    writeln!(output, "Started: {} (Unix seconds)", timestamp.as_secs())?;
    Ok(output)
}

fn default_config_path() -> Result<PathBuf, String> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or("cannot locate the configuration directory. Set an absolute HOME or XDG_CONFIG_HOME, or use --config PATH")?;

    Ok(base.join("deployraft/config.toml"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "FAILURE: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let mut path = None;
    let mut app_name = None;
    let mut verbose = false;
    let mut parse_options = true;

    while let Some(arg) = args.next() {
        if parse_options && (arg == "-h" || arg == "--help") {
            write!(io::stdout(), "{HELP}")?;
            io::stdout().flush()?;
            return Ok(());
        }

        if parse_options && arg == "--" {
            parse_options = false;
        } else if parse_options && arg == "--config" {
            path = Some(PathBuf::from(
                args.next().ok_or("--config requires a path")?,
            ));
        } else if parse_options && arg == "--verbose" {
            verbose = true;
        } else if parse_options && arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()).into());
        } else if app_name.is_some() {
            return Err("expected exactly one application name".into());
        } else {
            app_name = Some(arg.into_string().map_err(|_| "invalid application name")?);
        }
    }

    let app_name = app_name.ok_or_else(|| format!("application name is required\n\n{HELP}"))?;
    let path = match path {
        Some(path) => path,
        None => default_config_path()?,
    };
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let config: Config = toml::from_str(&contents).map_err(|error: toml::de::Error| {
        let position = error
            .span()
            .map(|span| format!(" at byte {}", span.start + 1))
            .unwrap_or_default();

        format!(
            "invalid configuration in {}{position}. Check TOML syntax, required fields and value types",
            path.display()
        )
    })?;
    let app = config.apps.get(&app_name).ok_or_else(|| {
        let names = config.apps.keys().cloned().collect::<Vec<_>>().join(", ");
        format!("unknown application {app_name:?}. Available applications: {names}")
    })?;
    let server = config
        .servers
        .get(&app.server)
        .ok_or_else(|| format!("unknown server: {:?}", app.server))?;

    validate(server, app)?;

    let mut output = create_log(&app_name, verbose)?;
    let result = deploy(&app_name, server, app, &mut output);

    if let Err(error) = &result {
        writeln!(output, "FAILURE: {error}")?;
    }

    writeln!(
        output,
        "Finished: {} (Unix seconds)",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs()
    )?;
    output.flush()?;
    result
}

fn deploy(
    app_name: &str,
    server: &Server,
    app: &App,
    output: &mut LogOutput,
) -> Result<(), Box<dyn Error>> {
    writeln!(output, "Application: {app_name}")?;
    writeln!(
        output,
        "Server: {} ({}@{}:{})",
        app.server, server.user, server.host, server.port
    )?;
    writeln!(output, "Directory: {}", app.directory)?;

    if output.verbose {
        writeln!(
            output,
            "WARNING: the full command below may contain secrets."
        )?;
        writeln!(output, "Command: {}", app.command)?;
    }

    if server.identity_file.is_some() {
        writeln!(
            output,
            "WARNING: SSH may also try identity files from your SSH configuration."
        )?;
    }

    if let Some(at) = &app.at {
        writeln!(
            output,
            "Schedule: {at} (server timezone, next day if already passed)"
        )?;
        writeln!(
            output,
            "WARNING: scheduled commands need server-side credentials. Forwarded SSH agents are unavailable after disconnecting."
        )?;
        writeln!(output, "Connecting and submitting the scheduled job...")?;
    } else {
        writeln!(output, "Connecting and executing the command...")?;
    }

    output.flush()?;
    execute(server, &remote_script(app), output)?;

    if app.at.is_some() {
        writeln!(
            output,
            "SCHEDULED: job accepted. Future command output and failures are not monitored."
        )?;
    } else {
        writeln!(output, "SUCCESS: command completed.")?;
    }

    output.flush()?;

    Ok(())
}

fn validate(server: &Server, app: &App) -> Result<(), String> {
    for (name, value) in [
        ("host", server.host.as_str()),
        ("user", server.user.as_str()),
        ("server", app.server.as_str()),
        ("directory", app.directory.as_str()),
        ("command", app.command.as_str()),
    ] {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(format!("{name} must be nonempty and contain no NUL bytes"));
        }
    }

    for (name, value) in [("host", &server.host), ("user", &server.user)] {
        if value.starts_with('-') || value.contains('@') || value.chars().any(char::is_whitespace) {
            return Err(format!(
                "{name} must not start with '-' or contain '@' or whitespace"
            ));
        }
    }

    if server.port == 0 {
        return Err("port must be between 1 and 65535".into());
    }

    if !app.directory.starts_with('/') {
        return Err("directory must be an absolute path on the server".into());
    }

    if let Some(identity) = &server.identity_file {
        if matches!(server.auth, Auth::Password) {
            return Err("identity_file is only supported with auth = \"key\"".into());
        }

        let identity_path = PathBuf::from(identity);

        if !identity_path.is_absolute() {
            return Err(
                "identity_file must be an absolute local path. Expand '~' to your home directory"
                    .into(),
            );
        }

        if identity.contains('%') || identity.contains("${") {
            return Err("identity_file must not contain SSH substitutions ('%' or '${'). Use a symlink with a plain path if needed".into());
        }

        if identity.contains('\0') || !identity_path.is_file() {
            return Err("identity_file must point to an existing local file".into());
        }
    }

    if let Some(at) = &app.at {
        let valid = at.len() == 5
            && at.bytes().enumerate().all(|(i, byte)| match i {
                2 => byte == b':',
                _ => byte.is_ascii_digit(),
            })
            && at[..2].parse::<u8>().is_ok_and(|hour| hour < 24)
            && at[3..].parse::<u8>().is_ok_and(|minute| minute < 60);

        if !valid {
            return Err(
                "at must use HH:MM (00:00-23:59) in the server timezone. Dates are not supported"
                    .into(),
            );
        }
    }

    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn remote_script(app: &App) -> String {
    let directory = shell_quote(&app.directory);
    let deployment = format!(
        "cd -- {directory} && exec /bin/sh -c {}",
        shell_quote(&app.command)
    );

    let Some(at) = &app.at else {
        return deployment;
    };

    format!(
        r#"set -eu
printf '%s\n' 'Checking scheduler and deployment directory...'
command -v at >/dev/null || {{ printf '%s\n' 'at is not installed on the server'; exit 1; }}
(cd -- {directory}) || exit 1
printf 'Submitting job for %s (server timezone, next day if already passed)\n' {requested}
printf '%s\n' {deployment} | at -v {requested}
"#,
        requested = shell_quote(at),
        deployment = shell_quote(&deployment),
    )
}

fn execute(server: &Server, script: &str, output: &mut LogOutput) -> Result<(), Box<dyn Error>> {
    let mut ssh = Command::new("ssh");
    ssh.args(["-T", "-p", &server.port.to_string(), "-l", &server.user]);
    ssh.args([
        "-S",
        "none",
        "-o",
        "ForkAfterAuthentication=no",
        "-o",
        "SessionType=default",
        "-o",
        "RemoteCommand=none",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "LogLevel=INFO",
        "-o",
        "LogVerbose=none",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "BatchMode=no",
    ]);

    match server.auth {
        Auth::Password => {
            ssh.args([
                "-o",
                "PubkeyAuthentication=no",
                "-o",
                "PreferredAuthentications=keyboard-interactive,password",
                "-o",
                "PasswordAuthentication=yes",
                "-o",
                "KbdInteractiveAuthentication=yes",
            ]);
        }
        Auth::Key => {
            ssh.args([
                "-o",
                "PreferredAuthentications=publickey",
                "-o",
                "PubkeyAuthentication=yes",
            ]);

            if let Some(identity) = &server.identity_file {
                ssh.args(["-o", "IdentitiesOnly=yes", "-i", identity]);
            }
        }
    }

    let reader = if output.verbose {
        let (reader, writer) = UnixStream::pair()?;
        ssh.stdout(Stdio::from(OwnedFd::from(writer.try_clone()?)))
            .stderr(Stdio::from(OwnedFd::from(writer)));
        Some(reader)
    } else {
        ssh.stdout(Stdio::from(output.file.try_clone()?))
            .stderr(Stdio::from(output.file.try_clone()?));
        None
    };
    let mut child = ssh
        .arg("--")
        .arg(&server.host)
        .arg(format!("/bin/sh -c {}", shell_quote(script)))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start ssh: {error}"))?;
    drop(ssh);
    let copied = match reader {
        Some(mut reader) => io::copy(&mut reader, output).map(|_| ()),
        None => Ok(()),
    };
    let status = child.wait()?;
    copied?;

    if status.code().is_none() || status.code() == Some(255) {
        return Err(format!(
            "SSH ended without a confirmed result ({status}). The remote operation may already have started or completed. Check the server state and, for scheduled jobs, atq before retrying"
        ).into());
    }

    if !status.success() {
        return Err(format!("SSH or remote operation failed ({status})").into());
    }

    Ok(())
}
