//! Named contexts for the CLI: a saved (server, key file) pair under a name.
//!
//! WHY this exists: the operator should not type `--server` and
//! `--key-file` on every command, and must never have to put a credential
//! in shell history. A context names a gateway and the PATH of the
//! credential file that holds the key -- never the key itself. The
//! credential stays in its own 0600 file, which is the whole point: the
//! config file is a list of pointers, so it can be read, copied or
//! printed without leaking anything.
//!
//! WHERE it lives: `~/.config/dg/config.toml`, beside the
//! `~/.config/dg/key` the CLI already resolves, so everything about a
//! connection sits in one directory.
//!
//! FORMAT: the `toml` crate is not a dependency of this workspace, and
//! one more dependency for a file holding a URL and a path is not worth
//! the budget. The file is therefore written in a small TOML subset that
//! this module parses itself -- and the subset is valid TOML, so a real
//! parser can read it whenever one is worth having:
//!
//! ```toml
//! default = "micro2"
//!
//! [contexts.micro2]
//! server = "http://127.0.0.1:7432"
//! key_file = "/home/operator/.config/dg/key"
//! ```
//!
//! Only that subset is read: a top-level `default`, and one
//! `[contexts.<name>]` table per context holding `server` and
//! `key_file`. Anything else is an error naming the line -- a config the
//! CLI cannot read is never half-applied, and never a panic.

use dg::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The gateway the CLI talks to when nothing else says otherwise.
///
/// The same value the root `--server` flag carries today, so adding a
/// context changes nothing for a command that names no context.
const DEFAULT_SERVER: &str = "http://127.0.0.1:7432";

/// One named context: where the gateway is, and where its credential lives.
struct Context {
    /// The name the operator typed: `micro2`, `staging`, `local`.
    name: String,
    /// The gateway URL.
    server: String,
    /// The credential FILE. Never the credential.
    key_file: PathBuf,
}

/// The parsed config file.
struct Config {
    /// The context used when no `--context` is given.
    default: Option<String>,
    /// The contexts, in the order the file lists them.
    contexts: Vec<Context>,
}

impl Config {
    /// The context with this name.
    fn find(&self, name: &str) -> Option<&Context> {
        self.contexts.iter().find(|context| context.name == name)
    }

    /// The context used when no `--context` is given.
    ///
    /// A default naming a context the file does not define is an error,
    /// not a silent fallback: the operator asked for that gateway by
    /// marking it, and quietly using another one would talk to the wrong
    /// host.
    fn default_context(&self) -> Result<Option<&Context>, Error> {
        match &self.default {
            None => Ok(None),
            Some(name) => self.find(name).map(Some).ok_or_else(|| {
                Error::with_message(
                    400,
                    "no_such_context",
                    format!("the default context '{name}' is not defined"),
                )
            }),
        }
    }
}

/// The connection settings a command runs against.
///
/// Both halves are resolved the same way, and the resolution is documented
/// in `resolve`.
pub struct ResolvedContext {
    /// The gateway URL to talk to.
    pub server: String,
    /// The credential FILE to read. `None` when no credential file is
    /// known to exist, which is what the CLI already reports as a missing
    /// credential.
    pub key_file: Option<PathBuf>,
}

/// The default config file: `~/.config/dg/config.toml`.
///
/// Beside the default credential file the CLI already resolves, so one
/// directory holds everything about the connection.
pub fn config_path() -> Result<PathBuf, dg::ServerError> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        dg::ServerError::Application(Error::new(
            400,
            "no_home",
            "HOME is not set, so the config file has no location",
        ))
    })?;
    let mut path = PathBuf::from(home);
    path.push(".config/dg/config.toml");
    Ok(path)
}

/// `dg config list`: every context, its server, its key file, and whether
/// that key file is actually there.
///
/// A context pointing at a missing key file is worth saying out loud: it
/// is the failure that otherwise shows up later as an unexplained
/// authentication error.
pub fn list(path: &Path, json: bool) -> Result<(), dg::ServerError> {
    let config = load(path).map_err(dg::ServerError::Application)?;
    if json || !std::io::stdout().is_terminal() {
        return print_line(&contexts_of(&config, path).to_string())
            .map_err(dg::ServerError::Application);
    }
    if config.contexts.is_empty() {
        return print_line("no contexts configured").map_err(dg::ServerError::Application);
    }
    let mut table = dg::render::Table::new();
    table.column("context", dg::render::Align::Left);
    table.column("server", dg::render::Align::Left);
    table.column("key file", dg::render::Align::Left);
    table.column("key", dg::render::Align::Left);
    for context in &config.contexts {
        let default = config.default.as_deref() == Some(context.name.as_str());
        // The default is marked in the name column: it is the one column
        // every operator reads first, and a separate column of asterisks
        // would be easy to miss.
        let name = if default {
            format!("* {}", context.name)
        } else {
            context.name.clone()
        };
        table.row(vec![
            dg::render::Cell::styled(name, dg::render::Style::Cyan),
            dg::render::Cell::text(context.server.clone()),
            dg::render::Cell::text(context.key_file.to_string_lossy().into_owned()),
            if context.key_file.exists() {
                dg::render::Cell::styled("present", dg::render::Style::Green)
            } else {
                dg::render::Cell::styled("missing", dg::render::Style::Red)
            },
        ]);
    }
    table.footer("* marks the default context");
    let theme = dg::render::Theme::for_stream(std::io::stdout().is_terminal());
    let mut out = std::io::stdout().lock();
    table
        .write(&mut out, &theme)
        .map_err(dg::ServerError::Application)
}

/// `dg config show <name>`: one context in detail.
pub fn show(path: &Path, name: &str, json: bool) -> Result<(), dg::ServerError> {
    let document = context(path, name)?;
    if json || !std::io::stdout().is_terminal() {
        return print_line(&document.to_string()).map_err(dg::ServerError::Application);
    }
    let facts = vec![
        (
            "server".to_string(),
            document["server"].as_str().unwrap_or_default().to_string(),
        ),
        (
            "key file".to_string(),
            document["key_file"].as_str().unwrap_or_default().to_string(),
        ),
        (
            "key".to_string(),
            match document["key_present"].as_bool() {
                Some(true) => "present".to_string(),
                _ => "missing".to_string(),
            },
        ),
        (
            "default".to_string(),
            match document["default"].as_bool() {
                Some(true) => "yes".to_string(),
                _ => "no".to_string(),
            },
        ),
    ];
    let outcome = dg::render::Outcome::with_facts(format!("context {}", name), facts);
    let theme = dg::render::Theme::for_stream(std::io::stdout().is_terminal());
    let mut out = std::io::stdout().lock();
    outcome
        .write(&mut out, &theme)
        .map_err(dg::ServerError::Application)
}

/// `dg config set <name> --server <url> --key-file <path>`: create or
/// update a context.
///
/// The key file PATH is what is stored. The credential never enters the
/// config, so nothing here can leak a secret.
pub fn set(
    path: &Path,
    name: &str,
    server: &str,
    key_file: &Path,
    default: bool,
    json: bool,
) -> Result<(), dg::ServerError> {
    validate_name(name).map_err(dg::ServerError::Application)?;
    validate_server(server).map_err(dg::ServerError::Application)?;
    let mut config = load(path).map_err(dg::ServerError::Application)?;
    let key_file = expand_home(key_file);
    let updated = match config
        .contexts
        .iter_mut()
        .find(|context| context.name == name)
    {
        Some(context) => {
            context.server = server.to_string();
            context.key_file = key_file.clone();
            true
        }
        None => {
            config.contexts.push(Context {
                name: name.to_string(),
                server: server.to_string(),
                key_file: key_file.clone(),
            });
            false
        }
    };
    if default {
        // One default at a time: marking a context moves the mark.
        config.default = Some(name.to_string());
    }
    store(path, &config).map_err(dg::ServerError::Application)?;
    let document = serde_json::json!({
        "name": name,
        "server": server,
        "key_file": key_file.to_string_lossy(),
        "default": config.default.as_deref() == Some(name),
        "created": !updated,
    });
    if json || !std::io::stdout().is_terminal() {
        return print_line(&document.to_string()).map_err(dg::ServerError::Application);
    }
    let outcome = dg::render::Outcome::with_facts(
        if updated {
            format!("updated context {name}")
        } else {
            format!("saved context {name}")
        },
        vec![
            ("server".to_string(), server.to_string()),
            (
                "key file".to_string(),
                key_file.to_string_lossy().into_owned(),
            ),
            (
                "default".to_string(),
                if document["default"].as_bool() == Some(true) {
                    "yes".to_string()
                } else {
                    "no".to_string()
                },
            ),
        ],
    );
    let theme = dg::render::Theme::for_stream(std::io::stdout().is_terminal());
    let mut out = std::io::stdout().lock();
    outcome
        .write(&mut out, &theme)
        .map_err(dg::ServerError::Application)
}

/// `dg config remove <name>`: delete one context.
///
/// Removing the default context clears the default: a default that names
/// nothing would be a broken file.
pub fn remove(path: &Path, name: &str, json: bool) -> Result<(), dg::ServerError> {
    let mut config = load(path).map_err(dg::ServerError::Application)?;
    let before = config.contexts.len();
    config.contexts.retain(|context| context.name != name);
    if config.contexts.len() == before {
        return Err(dg::ServerError::Application(Error::with_message(
            400,
            "no_such_context",
            format!("no context named '{name}'"),
        )));
    }
    if config.default.as_deref() == Some(name) {
        config.default = None;
    }
    store(path, &config).map_err(dg::ServerError::Application)?;
    let document = serde_json::json!({"removed": name});
    if json || !std::io::stdout().is_terminal() {
        return print_line(&document.to_string()).map_err(dg::ServerError::Application);
    }
    let outcome = dg::render::Outcome::done(format!("removed context {name}"));
    let theme = dg::render::Theme::for_stream(std::io::stdout().is_terminal());
    let mut out = std::io::stdout().lock();
    outcome
        .write(&mut out, &theme)
        .map_err(dg::ServerError::Application)
}

/// `dg config path`: where the config file is.
///
/// The bare path, always: the question a script asks is "where is the
/// file", and a script wants the path, not a document about the path.
pub fn path() -> Result<(), dg::ServerError> {
    let path = config_path()?;
    print_line(&path.to_string_lossy()).map_err(dg::ServerError::Application)
}

/// The contexts as a document: what `dg config list` prints.
pub fn contexts(path: &Path) -> Result<serde_json::Value, dg::ServerError> {
    let config = load(path).map_err(dg::ServerError::Application)?;
    Ok(contexts_of(&config, path))
}

/// One context as a document: what `dg config show` prints.
pub fn context(path: &Path, name: &str) -> Result<serde_json::Value, dg::ServerError> {
    let config = load(path).map_err(dg::ServerError::Application)?;
    let context = config.find(name).ok_or_else(|| {
        dg::ServerError::Application(Error::with_message(
            400,
            "no_such_context",
            format!("no context named '{name}'"),
        ))
    })?;
    Ok(serde_json::json!({
        "name": context.name,
        "server": context.server,
        "key_file": context.key_file.to_string_lossy(),
        "key_present": context.key_file.exists(),
        "default": config.default.as_deref() == Some(context.name.as_str()),
        "config_path": path.to_string_lossy(),
    }))
}

/// Resolve the server and the credential file for a command.
///
/// The precedence, highest first, and each half is resolved on its own:
///
/// 1. the explicit `--server` / `--key-file` flags,
/// 2. the named `--context`,
/// 3. the `DG_SERVER` / `DG_KEY_FILE` environment variables,
/// 4. the config file's default context,
/// 5. the built-in defaults: `http://127.0.0.1:7432` and
///    `~/.config/dg/key`.
///
/// The flags come first because they are the operator's word at the
/// command line, and the environment comes after a context because a
/// context is the more specific statement: the environment says "where
/// do I usually talk", a context says "talk to THIS one now".
///
/// The environment is read here rather than by the argument parser,
/// because a parser that merges a flag and an environment variable into
/// one value cannot tell them apart -- and the difference is exactly what
/// this order is about.
pub fn resolve(
    context: Option<&str>,
    server_flag: Option<&str>,
    key_file_flag: Option<&Path>,
) -> Result<ResolvedContext, dg::ServerError> {
    let path = config_path()?;
    let server_env = std::env::var("DG_SERVER").ok();
    let key_file_env = std::env::var("DG_KEY_FILE").ok();
    resolve_with(
        &path,
        context,
        server_flag,
        key_file_flag,
        server_env.as_deref(),
        key_file_env.as_deref().map(Path::new),
    )
}

/// The resolution with every input given explicitly.
///
/// `resolve` is this, plus the process environment and the default config
/// location. The inputs are parameters so the order can be tested
/// without touching the machine's environment.
pub fn resolve_with(
    path: &Path,
    context: Option<&str>,
    server_flag: Option<&str>,
    key_file_flag: Option<&Path>,
    server_env: Option<&str>,
    key_file_env: Option<&Path>,
) -> Result<ResolvedContext, dg::ServerError> {
    let config = load(path).map_err(dg::ServerError::Application)?;
    // An explicit --context must name a context that exists: falling back
    // to the default would silently talk to a different gateway.
    let selected = match context {
        Some(name) => Some(config.find(name).ok_or_else(|| {
            dg::ServerError::Application(Error::with_message(
                400,
                "no_such_context",
                format!("no context named '{name}'"),
            ))
        })?),
        None => None,
    };

    let server = if let Some(server) = server_flag {
        server.to_string()
    } else if let Some(context) = selected {
        context.server.clone()
    } else if let Some(server) = server_env {
        server.to_string()
    } else if let Some(context) = config
        .default_context()
        .map_err(dg::ServerError::Application)?
    {
        context.server.clone()
    } else {
        DEFAULT_SERVER.to_string()
    };

    let key_file = if let Some(key_file) = key_file_flag {
        Some(key_file.to_path_buf())
    } else if let Some(context) = selected {
        Some(context.key_file.clone())
    } else if let Some(key_file) = key_file_env {
        Some(key_file.to_path_buf())
    } else if let Some(context) = config
        .default_context()
        .map_err(dg::ServerError::Application)?
    {
        Some(context.key_file.clone())
    } else {
        // The built-in default is the credential file the CLI already
        // resolves: present only when it is there, so a missing
        // credential is still reported as a missing credential.
        dg::client::default_key_file()
    };

    Ok(ResolvedContext { server, key_file })
}

/// The list document.
fn contexts_of(config: &Config, path: &Path) -> serde_json::Value {
    let contexts: Vec<serde_json::Value> = config
        .contexts
        .iter()
        .map(|context| {
            serde_json::json!({
                "name": context.name,
                "server": context.server,
                "key_file": context.key_file.to_string_lossy(),
                "key_present": context.key_file.exists(),
            })
        })
        .collect();
    serde_json::json!({
        "config_path": path.to_string_lossy(),
        "default": config.default,
        "contexts": contexts,
    })
}

/// Read the config file. A file that is not there is an empty config.
fn load(path: &Path) -> Result<Config, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config {
            default: None,
            contexts: Vec::new(),
        }),
        Err(error) => Err(Error::with_message(
            500,
            "config_unreadable",
            format!("cannot read {}: {error}", path.display()),
        )),
    }
}

/// Write the config file, privately.
///
/// The file holds paths today and may hold more later, so it is never
/// left readable by anyone else: it is staged at 0600 and published by a
/// rename, so a half-written config is never visible and the mode is
/// never widened.
fn store(path: &Path, config: &Config) -> Result<(), Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            Error::new(
                400,
                "config_path_invalid",
                "the config path has no directory to write into",
            )
        })?;
    // The directory is not created: a config tree that appears by
    // accident is a surprise nobody asked for.
    if !parent.is_dir() {
        return Err(Error::with_message(
            400,
            "config_dir_missing",
            format!("{} does not exist; create it first", parent.display()),
        ));
    }
    let text = render(config);
    let mut staged = tempfile::Builder::new()
        .prefix("config.toml.")
        .tempfile_in(parent)
        .map_err(|error| {
            Error::with_message(
                500,
                "config_write_failed",
                format!(
                    "cannot stage a config file in {}: {error}",
                    parent.display()
                ),
            )
        })?;
    staged.write_all(text.as_bytes()).map_err(|error| {
        Error::with_message(
            500,
            "config_write_failed",
            format!("cannot write the config file: {error}"),
        )
    })?;
    staged.flush().map_err(|error| {
        Error::with_message(
            500,
            "config_write_failed",
            format!("cannot write the config file: {error}"),
        )
    })?;
    staged.as_file().sync_all().map_err(|error| {
        Error::with_message(
            500,
            "config_write_failed",
            format!("cannot write the config file: {error}"),
        )
    })?;
    private(staged.path())?;
    staged.persist(path).map_err(|error| {
        Error::with_message(
            500,
            "config_write_failed",
            format!("cannot write {}: {}", path.display(), error.error),
        )
    })?;
    // The published file IS the staged file, so the mode above came with
    // it; setting it again says so out loud and survives a change in how
    // the staging file was made.
    private(path)
}

/// Make a file 0600.
#[cfg(unix)]
fn private(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        Error::with_message(
            500,
            "config_write_failed",
            format!("cannot set the mode of {}: {error}", path.display()),
        )
    })
}

/// Windows has no POSIX mode to narrow, so there is nothing to do.
#[cfg(not(unix))]
fn private(_path: &Path) -> Result<(), Error> {
    Ok(())
}

/// A context name has to be printable and non-empty.
///
/// Spaces are allowed: an operator names a context after the thing it is
/// for, and "my ctx" is a name someone will type.
fn validate_name(name: &str) -> Result<(), Error> {
    if name.trim().is_empty() {
        return Err(Error::new(
            400,
            "invalid_context_name",
            "a context name cannot be empty",
        ));
    }
    if name.chars().any(|character| character.is_control()) {
        return Err(Error::new(
            400,
            "invalid_context_name",
            "a context name cannot hold control characters",
        ));
    }
    Ok(())
}

/// A server URL has to be one value on one line.
fn validate_server(server: &str) -> Result<(), Error> {
    if server.trim().is_empty() {
        return Err(Error::new(
            400,
            "invalid_server",
            "a server URL cannot be empty",
        ));
    }
    if server
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(Error::new(
            400,
            "invalid_server",
            "a server URL cannot hold whitespace",
        ));
    }
    Ok(())
}

/// Expand a leading `~` to the home directory.
///
/// A path stored as `~/keys/micro2` would never resolve from another
/// directory, so it is expanded once, here, where the operator can see
/// what was stored.
fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let rest = if text == "~" {
        Some("")
    } else {
        text.strip_prefix("~/")
    };
    match (rest, std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => {
            let mut expanded = PathBuf::from(home);
            expanded.push(rest);
            expanded
        }
        _ => path.to_path_buf(),
    }
}

/// Print one line, without panicking on a closed pipe.
fn print_line(text: &str) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}").map_err(|error| {
        Error::with_message(
            500,
            "output_failed",
            format!("cannot write to stdout: {error}"),
        )
    })
}

/// Parse the config file.
///
/// Only the subset this module writes is accepted, and anything else is
/// an error naming the line: a config the CLI cannot read is never half
/// applied, and never a panic.
fn parse(text: &str) -> Result<Config, Error> {
    let mut default: Option<String> = None;
    let mut contexts: Vec<Context> = Vec::new();
    let mut open: Option<Context> = None;
    // The line a context's table started on, so an error about it points
    // at the context and not at the line that happened to follow it.
    let mut open_line = 0usize;

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let line_number = index + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if let Some(context) = open.take() {
                contexts.push(finish(context, open_line)?);
            }
            open = Some(Context {
                name: table_name(line, line_number)?,
                server: String::new(),
                key_file: PathBuf::new(),
            });
            open_line = line_number;
            continue;
        }
        let (key, value) = assignment(line, line_number)?;
        match open.as_mut() {
            None => {
                if key != "default" {
                    return Err(invalid(line_number, format!("unknown key '{key}'")));
                }
                if default.is_some() {
                    return Err(invalid(line_number, "duplicate 'default'"));
                }
                default = Some(value);
            }
            Some(context) => match key.as_str() {
                "server" => context.server = value,
                "key_file" => context.key_file = PathBuf::from(value),
                other => return Err(invalid(line_number, format!("unknown key '{other}'"))),
            },
        }
    }
    if let Some(context) = open.take() {
        contexts.push(finish(context, open_line)?);
    }
    // Two contexts with one name would make `--context` ambiguous, and
    // the second would silently win.
    for (index, context) in contexts.iter().enumerate() {
        if contexts[..index]
            .iter()
            .any(|other| other.name == context.name)
        {
            return Err(Error::with_message(
                400,
                "config_invalid",
                format!("duplicate context '{}'", context.name),
            ));
        }
    }
    Ok(Config { default, contexts })
}

/// A context table needs both halves before it is usable.
fn finish(context: Context, line_number: usize) -> Result<Context, Error> {
    if context.server.is_empty() {
        return Err(invalid(
            line_number,
            format!("context '{}' has no server", context.name),
        ));
    }
    if context.key_file.as_os_str().is_empty() {
        return Err(invalid(
            line_number,
            format!("context '{}' has no key_file", context.name),
        ));
    }
    Ok(context)
}

/// The context name in a `[contexts.<name>]` header.
fn table_name(line: &str, line_number: usize) -> Result<String, Error> {
    let body = line
        .strip_prefix('[')
        .and_then(|body| body.strip_suffix(']'))
        .ok_or_else(|| invalid(line_number, "malformed table header"))?
        .trim();
    let name = body
        .strip_prefix("contexts.")
        .ok_or_else(|| invalid(line_number, format!("unknown table '{body}'")))?;
    key(name, line_number)
}

/// A `key = "value"` line.
fn assignment(line: &str, line_number: usize) -> Result<(String, String), Error> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| invalid(line_number, "expected 'key = value'"))?;
    let key = key.trim();
    if !bare_key(key) {
        return Err(invalid(line_number, format!("unsupported key '{key}'")));
    }
    let value = basic_string(value.trim(), line_number)?;
    Ok((key.to_string(), value))
}

/// A key is bare (letters, digits, `_`, `-`) or a quoted string.
fn key(text: &str, line_number: usize) -> Result<String, Error> {
    let text = text.trim();
    if text.starts_with('"') {
        return basic_string(text, line_number);
    }
    if bare_key(text) {
        return Ok(text.to_string());
    }
    Err(invalid(line_number, format!("unsupported key '{text}'")))
}

/// Whether a key can be written without quotes.
fn bare_key(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

/// Read a TOML basic string: `"..."` with the escapes TOML defines.
fn basic_string(text: &str, line_number: usize) -> Result<String, Error> {
    let body = text
        .strip_prefix('"')
        .ok_or_else(|| invalid(line_number, "expected a quoted string"))?;
    let body = body
        .strip_suffix('"')
        .ok_or_else(|| invalid(line_number, "unterminated string"))?;
    unescape(body, line_number)
}

/// Read the escapes of a TOML basic string.
fn unescape(text: &str, line_number: usize) -> Result<String, Error> {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        let escape = characters
            .next()
            .ok_or_else(|| invalid(line_number, "dangling escape"))?;
        match escape {
            'b' => out.push('\u{8}'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'f' => out.push('\u{c}'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' | 'U' => {
                let width = if escape == 'u' { 4 } else { 8 };
                let mut digits = String::new();
                for _ in 0..width {
                    digits.push(
                        characters
                            .next()
                            .ok_or_else(|| invalid(line_number, "truncated escape"))?,
                    );
                }
                let code = u32::from_str_radix(&digits, 16)
                    .map_err(|_| invalid(line_number, "malformed escape"))?;
                let character =
                    char::from_u32(code).ok_or_else(|| invalid(line_number, "malformed escape"))?;
                out.push(character);
            }
            other => {
                return Err(invalid(line_number, format!("unknown escape '\\{other}'")));
            }
        }
    }
    Ok(out)
}

/// Write a TOML basic string.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 || control as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", control as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// The config file, as it is written.
///
/// The order is the file's own: the default first, because a TOML key
/// has to come before any table, then one table per context.
fn render(config: &Config) -> String {
    let mut out = String::new();
    out.push_str("# dg named contexts.\n");
    out.push_str("# A context names a gateway and the PATH of its credential file.\n");
    out.push_str("# The credential itself never lives here.\n");
    if let Some(default) = &config.default {
        out.push_str(&format!("default = \"{}\"\n", escape(default)));
    }
    for context in &config.contexts {
        out.push('\n');
        out.push_str(&format!("[contexts.{}]\n", render_key(&context.name)));
        out.push_str(&format!("server = \"{}\"\n", escape(&context.server)));
        out.push_str(&format!(
            "key_file = \"{}\"\n",
            escape(&context.key_file.to_string_lossy())
        ));
    }
    out
}

/// A key as it is written: bare when it can be, quoted when it cannot.
fn render_key(name: &str) -> String {
    if bare_key(name) {
        name.to_string()
    } else {
        format!("\"{}\"", escape(name))
    }
}

/// An error in the config file, naming the line it is on.
fn invalid(line_number: usize, message: impl Into<String>) -> Error {
    Error::with_message(
        400,
        "config_invalid",
        format!("config line {line_number}: {}", message.into()),
    )
}
