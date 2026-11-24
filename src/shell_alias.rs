//! Opt-in shell alias generation (`init`) and idempotent profile management (`alias`).

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use serde_json::json;

pub const DEFAULT_JIRA_ALIAS: &str = "j";
pub const DEFAULT_CONF_ALIAS: &str = "cf";

const BLOCK_START: &str = "# >>> bitefoundry atlassian aliases >>>";
const BLOCK_END: &str = "# <<< bitefoundry atlassian aliases <<<";

const RESERVED_NAMES: &[&str] = &[
    "atlassian",
    "confluence",
    "conf",
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "for",
    "in",
    "do",
    "done",
    "while",
    "until",
    "case",
    "esac",
    "function",
    "return",
    "exit",
    "end",
    "begin",
    "switch",
    "param",
    "break",
    "continue",
    "cd",
    "ls",
    "pwd",
    "echo",
    "export",
    "set",
    "unset",
    "alias",
    "unalias",
    "source",
    "eval",
    "exec",
    "test",
    "true",
    "false",
];

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    #[value(aliases = ["pwsh", "ps"])]
    Powershell,
    Zsh,
    Bash,
    Fish,
    Tcsh,
    Csh,
    #[value(aliases = ["dash", "ash", "ksh"])]
    Sh,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Powershell => "powershell",
            ShellKind::Zsh => "zsh",
            ShellKind::Bash => "bash",
            ShellKind::Fish => "fish",
            ShellKind::Tcsh => "tcsh",
            ShellKind::Csh => "csh",
            ShellKind::Sh => "sh",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum AliasCommand {
    /// Install opt-in short shell aliases (default: `j` for Jira, `cf` for Confluence) into your shell profile
    Install {
        /// Target shell (auto-detected if omitted)
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Alias name for `atlassian` (Jira TUI & CLI)
        #[arg(long, default_value = DEFAULT_JIRA_ALIAS)]
        jira: String,
        /// Alias name for `conf` (Confluence TUI & CLI)
        #[arg(long, default_value = DEFAULT_CONF_ALIAS)]
        conf: String,
        /// Define aliases even if another command with the same name exists on PATH
        #[arg(long)]
        force: bool,
        /// Preview the profile path and block without modifying any file
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Remove the managed BiteFoundry alias block from your shell profile
    Uninstall {
        /// Target shell (auto-detected if omitted)
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Preview removal without modifying any file
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// Show profile path, installation status, and PATH collision checks
    Status {
        /// Target shell (auto-detected if omitted)
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
        /// Explicit shell profile file path
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Alias name to check for `atlassian`
        #[arg(long, default_value = DEFAULT_JIRA_ALIAS)]
        jira: String,
        /// Alias name to check for `conf`
        #[arg(long, default_value = DEFAULT_CONF_ALIAS)]
        conf: String,
        #[arg(short = 'j', long)]
        json: bool,
    },
}

pub fn detect_shell() -> Result<ShellKind> {
    detect_shell_from(env::var("SHELL").ok().as_deref())
}

fn detect_shell_from(shell_env: Option<&str>) -> Result<ShellKind> {
    if let Some(raw) = shell_env.map(str::trim).filter(|s| !s.is_empty()) {
        let base = raw.split(['/', '\\']).next_back().unwrap_or(raw);
        let stripped = base.strip_prefix('-').unwrap_or(base).to_ascii_lowercase();
        let name = stripped.strip_suffix(".exe").unwrap_or(&stripped);
        return match name {
            "tcsh" => Ok(ShellKind::Tcsh),
            "csh" => Ok(ShellKind::Csh),
            "zsh" => Ok(ShellKind::Zsh),
            "fish" => Ok(ShellKind::Fish),
            "bash" => Ok(ShellKind::Bash),
            "sh" | "dash" | "ash" | "ksh" | "mksh" => Ok(ShellKind::Sh),
            "pwsh" | "powershell" => Ok(ShellKind::Powershell),
            _ => bail!(
                "Unsupported shell '{raw}' detected from $SHELL. Pass --shell <powershell|zsh|bash|fish|tcsh|csh|sh> and/or --profile <path>."
            ),
        };
    }
    if cfg!(windows) {
        return Ok(ShellKind::Powershell);
    }
    if cfg!(target_os = "macos") {
        return Ok(ShellKind::Zsh);
    }
    Ok(ShellKind::Bash)
}

pub fn validate_alias_name(name: &str, role: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("Alias name for {role} cannot be empty.");
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        bail!(
            "Invalid alias name '{trimmed}' for {role}: must start with an ASCII letter or '_'."
        );
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!(
            "Invalid alias name '{trimmed}' for {role}: only ASCII letters, digits, '_', and '-' are allowed."
        );
    }
    if RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(trimmed))
    {
        bail!("Alias name '{trimmed}' for {role} is reserved. Choose a different alias name.");
    }
    Ok(trimmed.to_string())
}

pub fn validate_alias_name_for_shell(
    name: &str,
    role: &str,
    shell: ShellKind,
) -> Result<String> {
    let validated = validate_alias_name(name, role)?;
    if shell == ShellKind::Sh && validated.contains('-') {
        bail!(
            "Invalid alias name '{validated}' for {role}: POSIX sh does not allow '-' in function names."
        );
    }
    Ok(validated)
}

pub fn validate_alias_pair(jira: &str, conf: &str) -> Result<(String, String)> {
    let j = validate_alias_name(jira, "Jira")?;
    let c = validate_alias_name(conf, "Confluence")?;
    if j.eq_ignore_ascii_case(&c) {
        bail!("Jira alias ('{j}') and Confluence alias ('{c}') must be different.");
    }
    Ok((j, c))
}

pub fn validate_alias_pair_for_shell(
    jira: &str,
    conf: &str,
    shell: ShellKind,
) -> Result<(String, String)> {
    let j = validate_alias_name_for_shell(jira, "Jira", shell)?;
    let c = validate_alias_name_for_shell(conf, "Confluence", shell)?;
    if j.eq_ignore_ascii_case(&c) {
        bail!("Jira alias ('{j}') and Confluence alias ('{c}') must be different.");
    }
    Ok((j, c))
}

pub fn render_init_script(shell: ShellKind, jira: &str, conf: &str, force: bool) -> String {
    match shell {
        ShellKind::Powershell => {
            if force {
                format!(
                    "Remove-Item -Path Alias:{jira} -ErrorAction SilentlyContinue\nfunction global:{jira} {{ atlassian @args }}\nRemove-Item -Path Alias:{conf} -ErrorAction SilentlyContinue\nfunction global:{conf} {{ conf @args }}\n"
                )
            } else {
                format!(
                    "if (-not (Get-Command '{jira}' -CommandType Alias,Function,Application,Cmdlet,ExternalScript -ErrorAction SilentlyContinue)) {{\n    function global:{jira} {{ atlassian @args }}\n}}\nif (-not (Get-Command '{conf}' -CommandType Alias,Function,Application,Cmdlet,ExternalScript -ErrorAction SilentlyContinue)) {{\n    function global:{conf} {{ conf @args }}\n}}\n"
                )
            }
        }
        ShellKind::Zsh | ShellKind::Bash | ShellKind::Sh => {
            if force {
                format!(
                    "unalias {jira} 2>/dev/null || true\n{jira}() {{ atlassian \"$@\"; }}\nunalias {conf} 2>/dev/null || true\n{conf}() {{ conf \"$@\"; }}\n"
                )
            } else {
                format!(
                    "if ! command -v {jira} >/dev/null 2>&1; then\n  {jira}() {{ atlassian \"$@\"; }}\nfi\nif ! command -v {conf} >/dev/null 2>&1; then\n  {conf}() {{ conf \"$@\"; }}\nfi\n"
                )
            }
        }
        ShellKind::Fish => {
            if force {
                format!(
                    "function {jira} --wraps atlassian --description 'BiteFoundry Atlassian Jira'\n    atlassian $argv\nend\nfunction {conf} --wraps conf --description 'BiteFoundry Atlassian Confluence'\n    conf $argv\nend\n"
                )
            } else {
                format!(
                    "if not type -q {jira}\n    function {jira} --wraps atlassian --description 'BiteFoundry Atlassian Jira'\n        atlassian $argv\n    end\nend\nif not type -q {conf}\n    function {conf} --wraps conf --description 'BiteFoundry Atlassian Confluence'\n        conf $argv\n    end\nend\n"
                )
            }
        }
        ShellKind::Tcsh | ShellKind::Csh => {
            if force {
                format!(
                    "unalias {jira} >& /dev/null\nalias {jira} 'atlassian \\!*'\nunalias {conf} >& /dev/null\nalias {conf} 'conf \\!*'\n"
                )
            } else {
                format!(
                    "which {jira} >& /dev/null\nif ( $status != 0 ) alias {jira} 'atlassian \\!*'\nwhich {conf} >& /dev/null\nif ( $status != 0 ) alias {conf} 'conf \\!*'\n"
                )
            }
        }
    }
}

pub fn render_managed_block(shell: ShellKind, jira: &str, conf: &str, force: bool) -> String {
    let body = render_init_script(shell, jira, conf, force);
    format!("{BLOCK_START}\n{body}{BLOCK_END}")
}

fn tcsh_profile_path(home: &Path) -> PathBuf {
    let tcshrc = home.join(".tcshrc");
    if tcshrc.exists() {
        return tcshrc;
    }
    let cshrc = home.join(".cshrc");
    if cshrc.exists() {
        return cshrc;
    }
    tcshrc
}

fn bash_profile_path(home: &Path) -> PathBuf {
    let bashrc = home.join(".bashrc");
    let bash_profile = home.join(".bash_profile");
    if !bashrc.exists() && bash_profile.exists() {
        return bash_profile;
    }
    bashrc
}

fn sh_profile_path(home: &Path, env_var: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(p) = env_var.filter(|v| !v.is_empty() && !v.to_string_lossy().trim().is_empty()) {
        return PathBuf::from(p);
    }
    home.join(".profile")
}

pub fn default_profile_paths(shell: ShellKind) -> Result<Vec<PathBuf>> {
    match shell {
        ShellKind::Powershell => {
            if let Some(p) = env::var_os("PROFILE").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(p)]);
            }
            let docs = match dirs::document_dir() {
                Some(d) => d,
                None => dirs::home_dir()
                    .context("Could not determine home directory")?
                    .join("Documents"),
            };
            let ps7 = docs
                .join("PowerShell")
                .join("Microsoft.PowerShell_profile.ps1");
            if cfg!(windows) {
                let ps5 = docs
                    .join("WindowsPowerShell")
                    .join("Microsoft.PowerShell_profile.ps1");
                let mut paths = Vec::new();
                if ps7.exists() || ps7.parent().is_some_and(|p| p.exists()) {
                    paths.push(ps7.clone());
                }
                if ps5.exists() || ps5.parent().is_some_and(|p| p.exists()) {
                    paths.push(ps5);
                }
                if paths.is_empty() {
                    paths.push(ps7);
                }
                Ok(paths)
            } else {
                Ok(vec![ps7])
            }
        }
        ShellKind::Zsh => {
            if let Some(zdot) = env::var_os("ZDOTDIR").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(zdot).join(".zshrc")]);
            }
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![home.join(".zshrc")])
        }
        ShellKind::Bash => {
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![bash_profile_path(&home)])
        }
        ShellKind::Fish => {
            if let Some(xdg) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
                return Ok(vec![PathBuf::from(xdg).join("fish").join("config.fish")]);
            }
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![home.join(".config").join("fish").join("config.fish")])
        }
        ShellKind::Tcsh => {
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![tcsh_profile_path(&home)])
        }
        ShellKind::Csh => {
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![home.join(".cshrc")])
        }
        ShellKind::Sh => {
            let env_val = env::var_os("ENV");
            if let Some(p) = env_val
                .as_deref()
                .filter(|v| !v.is_empty() && !v.to_string_lossy().trim().is_empty())
            {
                return Ok(vec![PathBuf::from(p)]);
            }
            let home = dirs::home_dir().context("Could not determine home directory")?;
            Ok(vec![sh_profile_path(&home, None)])
        }
    }
}

pub fn default_profile_path(shell: ShellKind) -> Result<PathBuf> {
    default_profile_paths(shell)?
        .into_iter()
        .next()
        .context("Could not determine default shell profile path")
}

pub fn find_path_collision(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    let pathext: Vec<String> = if cfg!(windows) {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.PS1".to_string())
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if cfg!(windows) {
            let direct = dir.join(name);
            if direct.is_file() {
                return Some(direct);
            }
            for ext in &pathext {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        } else {
            let candidate = dir.join(name);
            if candidate.is_file() && is_executable_unix(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_unix(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_unix(_path: &Path) -> bool {
    true
}

pub fn extract_managed_block(content: &str) -> Option<String> {
    let normalized = content.replace("\r\n", "\n");
    let start = normalized.find(BLOCK_START)?;
    let rest = &normalized[start..];
    let end_rel = rest.find(BLOCK_END)?;
    let end = start + end_rel + BLOCK_END.len();
    Some(normalized[start..end].to_string())
}

pub fn upsert_managed_block(existing: &str, block: &str) -> (String, bool) {
    let use_crlf = existing.contains("\r\n");
    let normalized = existing.replace("\r\n", "\n");
    let block_norm = block.replace("\r\n", "\n");

    let updated_norm = if let Some(start) = normalized.find(BLOCK_START) {
        let rest = &normalized[start..];
        if let Some(end_rel) = rest.find(BLOCK_END) {
            let end = start + end_rel + BLOCK_END.len();
            let mut out = String::with_capacity(normalized.len() + block_norm.len());
            out.push_str(&normalized[..start]);
            out.push_str(&block_norm);
            out.push_str(&normalized[end..]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out
        } else {
            let trimmed = normalized.trim_end_matches('\n');
            format!("{trimmed}\n\n{block_norm}\n")
        }
    } else if normalized.trim().is_empty() {
        format!("{block_norm}\n")
    } else {
        let trimmed = normalized.trim_end_matches('\n');
        format!("{trimmed}\n\n{block_norm}\n")
    };

    let changed = updated_norm != normalized;
    let final_text = if use_crlf {
        updated_norm.replace('\n', "\r\n")
    } else {
        updated_norm
    };
    (final_text, changed)
}

pub fn remove_managed_block(existing: &str) -> (String, bool) {
    let use_crlf = existing.contains("\r\n");
    let normalized = existing.replace("\r\n", "\n");

    let Some(start) = normalized.find(BLOCK_START) else {
        return (existing.to_string(), false);
    };
    let rest = &normalized[start..];
    let Some(end_rel) = rest.find(BLOCK_END) else {
        return (existing.to_string(), false);
    };
    let mut end = start + end_rel + BLOCK_END.len();
    if normalized[end..].starts_with('\n') {
        end += 1;
    }

    let before = normalized[..start].trim_end_matches('\n');
    let after = normalized[end..].trim_start_matches('\n');

    let updated_norm = match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => {
            if after.ends_with('\n') {
                after.to_string()
            } else {
                format!("{after}\n")
            }
        }
        (false, true) => format!("{before}\n"),
        (false, false) => {
            if after.ends_with('\n') {
                format!("{before}\n\n{after}")
            } else {
                format!("{before}\n\n{after}\n")
            }
        }
    };

    let final_text = if use_crlf {
        updated_norm.replace('\n', "\r\n")
    } else {
        updated_norm
    };
    (final_text, true)
}

pub fn cmd_init(shell: ShellKind, jira: &str, conf: &str, force: bool) -> Result<()> {
    let (j, c) = validate_alias_pair_for_shell(jira, conf, shell)?;
    print!("{}", render_init_script(shell, &j, &c, force));
    Ok(())
}

/// Resolves the write destination for a shell profile file so that valid symlinks are
/// preserved in place and dangling symlinks have their target parent directory created
/// cleanly (with clear context on failure or symlink loops) instead of an opaque ENOENT.
fn prepare_profile_write_target(profile_path: &Path) -> Result<PathBuf> {
    if let Ok(meta) = fs::symlink_metadata(profile_path) {
        if meta.file_type().is_symlink() {
            if profile_path.exists() {
                return Ok(profile_path.to_path_buf());
            }
            let mut current = profile_path.to_path_buf();
            for _ in 0..16 {
                let raw_target = fs::read_link(&current).with_context(|| {
                    format!(
                        "Failed to read symlink target for shell profile {}",
                        current.display()
                    )
                })?;
                let resolved = if raw_target.is_absolute() {
                    raw_target
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(raw_target)
                };
                match fs::symlink_metadata(&resolved) {
                    Ok(next_meta) if next_meta.file_type().is_symlink() => {
                        current = resolved;
                    }
                    _ => {
                        if let Some(target_parent) = resolved.parent() {
                            if !target_parent.as_os_str().is_empty() && !target_parent.exists() {
                                fs::create_dir_all(target_parent).with_context(|| {
                                    format!(
                                        "Shell profile {} is a dangling symlink to {}; failed to create target directory {}",
                                        profile_path.display(),
                                        resolved.display(),
                                        target_parent.display()
                                    )
                                })?;
                            }
                        }
                        return Ok(resolved);
                    }
                }
            }
            bail!(
                "Shell profile {} is a dangling symlink loop",
                profile_path.display()
            );
        }
    }
    if let Some(parent) = profile_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create profile directory {}", parent.display())
            })?;
        }
    }
    Ok(profile_path.to_path_buf())
}

pub fn cmd_alias(action: AliasCommand) -> Result<()> {
    match action {
        AliasCommand::Install {
            shell,
            profile,
            jira,
            conf,
            force,
            dry_run,
            json: json_out,
        } => {
            let shell = match shell {
                Some(s) => s,
                None => detect_shell()?,
            };
            let (j, c) = validate_alias_pair_for_shell(&jira, &conf, shell)?;
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let jira_collision = find_path_collision(&j);
            let conf_collision = find_path_collision(&c);
            let block = render_managed_block(shell, &j, &c, force);

            let mut any_changed = false;
            let mut results = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let existing = if profile_path.exists() {
                    fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?
                } else {
                    String::new()
                };

                let (updated, changed) = upsert_managed_block(&existing, &block);
                if changed {
                    any_changed = true;
                }

                if !dry_run && changed {
                    let write_target = prepare_profile_write_target(profile_path)?;
                    fs::write(&write_target, updated).with_context(|| {
                        format!("Failed to write shell profile {}", profile_path.display())
                    })?;
                }
                results.push((profile_path.clone(), changed));
            }

            let primary_path = &profile_paths[0];
            let reload_hint = reload_instruction(shell, primary_path);
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "action": "install",
                        "shell": shell.as_str(),
                        "profile": primary_path.display().to_string(),
                        "profiles": results.iter().map(|(p, ch)| json!({
                            "path": p.display().to_string(),
                            "changed": ch,
                        })).collect::<Vec<_>>(),
                        "aliases": { "jira": j, "confluence": c },
                        "force": force,
                        "dry_run": dry_run,
                        "changed": any_changed,
                        "collisions": {
                            "jira": jira_collision.as_ref().map(|p| p.display().to_string()),
                            "confluence": conf_collision.as_ref().map(|p| p.display().to_string()),
                        },
                        "block": block,
                        "reload_hint": reload_hint,
                    }))?
                );
            } else {
                for (profile_path, changed) in &results {
                    if dry_run {
                        println!(
                            "[dry-run] Would {} aliases ({j} -> atlassian, {c} -> conf) in {} ({}).",
                            if *changed { "update" } else { "keep existing" },
                            profile_path.display(),
                            shell.as_str()
                        );
                    } else if *changed {
                        println!(
                            "Installed aliases ({j} -> atlassian, {c} -> conf) in {}.",
                            profile_path.display()
                        );
                    } else {
                        println!(
                            "Aliases ({j} -> atlassian, {c} -> conf) are already up to date in {}.",
                            profile_path.display()
                        );
                    }
                }
                if dry_run {
                    println!("\n{block}");
                } else if any_changed {
                    println!("Reload your shell or run: {reload_hint}");
                }

                if let Some(ref path) = jira_collision {
                    eprintln!(
                        "Warning: '{j}' already exists on PATH ({}). {}",
                        path.display(),
                        if force {
                            "Overriding because --force was passed."
                        } else {
                            "The guarded alias will skip it; pick another name with --jira <NAME> or pass --force."
                        }
                    );
                }
                if let Some(ref path) = conf_collision {
                    eprintln!(
                        "Warning: '{c}' already exists on PATH ({}). {}",
                        path.display(),
                        if force {
                            "Overriding because --force was passed."
                        } else {
                            "The guarded alias will skip it; pick another name with --conf <NAME> or pass --force."
                        }
                    );
                }
            }
            Ok(())
        }
        AliasCommand::Uninstall {
            shell,
            profile,
            dry_run,
            json: json_out,
        } => {
            let shell = match shell {
                Some(s) => s,
                None => detect_shell()?,
            };
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let mut any_removed = false;
            let mut any_existed = false;
            let mut results = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let (removed, existed) = if profile_path.exists() {
                    let existing = fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?;
                    let (updated, removed) = remove_managed_block(&existing);
                    if removed && !dry_run {
                        fs::write(profile_path, updated).with_context(|| {
                            format!("Failed to write shell profile {}", profile_path.display())
                        })?;
                    }
                    (removed, true)
                } else {
                    (false, false)
                };
                any_removed |= removed;
                any_existed |= existed;
                results.push((profile_path.clone(), existed, removed));
            }

            let primary_path = &profile_paths[0];
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "action": "uninstall",
                        "shell": shell.as_str(),
                        "profile": primary_path.display().to_string(),
                        "profiles": results.iter().map(|(p, ex, rem)| json!({
                            "path": p.display().to_string(),
                            "profile_exists": ex,
                            "removed": rem,
                        })).collect::<Vec<_>>(),
                        "profile_exists": any_existed,
                        "dry_run": dry_run,
                        "removed": any_removed,
                    }))?
                );
            } else {
                for (profile_path, _, removed) in &results {
                    if *removed {
                        if dry_run {
                            println!(
                                "[dry-run] Would remove BiteFoundry alias block from {}.",
                                profile_path.display()
                            );
                        } else {
                            println!(
                                "Removed BiteFoundry alias block from {}.",
                                profile_path.display()
                            );
                        }
                    } else {
                        println!(
                            "No BiteFoundry alias block found in {}.",
                            profile_path.display()
                        );
                    }
                }
            }
            Ok(())
        }
        AliasCommand::Status {
            shell,
            profile,
            jira,
            conf,
            json: json_out,
        } => {
            let shell = match shell {
                Some(s) => s,
                None => detect_shell()?,
            };
            let (j, c) = validate_alias_pair_for_shell(&jira, &conf, shell)?;
            let profile_paths = match profile {
                Some(p) => vec![p],
                None => default_profile_paths(shell)?,
            };

            let mut entries = Vec::with_capacity(profile_paths.len());
            for profile_path in &profile_paths {
                let exists = profile_path.exists();
                let block = if exists {
                    let existing = fs::read_to_string(profile_path).with_context(|| {
                        format!("Failed to read shell profile {}", profile_path.display())
                    })?;
                    extract_managed_block(&existing)
                } else {
                    None
                };
                entries.push((profile_path.clone(), exists, block));
            }

            let primary = &entries[0];
            let installed = entries.iter().any(|(_, _, b)| b.is_some());
            let jira_collision = find_path_collision(&j);
            let conf_collision = find_path_collision(&c);

            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "shell": shell.as_str(),
                        "profile": primary.0.display().to_string(),
                        "profile_exists": primary.1,
                        "installed": installed,
                        "block": primary.2,
                        "profiles": entries.iter().map(|(p, ex, b)| json!({
                            "path": p.display().to_string(),
                            "profile_exists": ex,
                            "installed": b.is_some(),
                        })).collect::<Vec<_>>(),
                        "checked_aliases": { "jira": j, "confluence": c },
                        "collisions": {
                            "jira": jira_collision.as_ref().map(|p| p.display().to_string()),
                            "confluence": conf_collision.as_ref().map(|p| p.display().to_string()),
                        },
                    }))?
                );
            } else {
                println!("Shell:   {}", shell.as_str());
                for (profile_path, _, block) in &entries {
                    println!(
                        "Profile: {} ({})",
                        profile_path.display(),
                        if block.is_some() {
                            "installed"
                        } else {
                            "not installed"
                        }
                    );
                }
                if let Some(ref path) = jira_collision {
                    println!("PATH collision ({j}): {}", path.display());
                }
                if let Some(ref path) = conf_collision {
                    println!("PATH collision ({c}): {}", path.display());
                }
                if let Some(b) = entries.iter().find_map(|(_, _, b)| b.as_ref()) {
                    println!("\n{b}");
                }
            }
            Ok(())
        }
    }
}

fn reload_instruction(shell: ShellKind, profile_path: &Path) -> String {
    match shell {
        ShellKind::Powershell | ShellKind::Sh => format!(". \"{}\"", profile_path.display()),
        ShellKind::Zsh | ShellKind::Bash | ShellKind::Fish | ShellKind::Tcsh | ShellKind::Csh => {
            format!("source \"{}\"", profile_path.display())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_alias_names_and_rejects_reserved_or_duplicate() {
        assert_eq!(validate_alias_pair("j", "cf").unwrap(), ("j".into(), "cf".into()));
        assert_eq!(
            validate_alias_pair("my_jira", "my-conf").unwrap(),
            ("my_jira".into(), "my-conf".into())
        );
        assert!(validate_alias_pair("j", "j").is_err());
        assert!(validate_alias_pair("1bad", "cf").is_err());
        assert!(validate_alias_pair("j;rm", "cf").is_err());
        assert!(validate_alias_pair("atlassian", "cf").is_err());
        assert!(validate_alias_pair("j", "conf").is_err());
        assert!(validate_alias_pair("if", "cf").is_err());
    }

    #[test]
    fn renders_scripts_for_all_shells() {
        let ps = render_init_script(ShellKind::Powershell, "j", "cf", false);
        assert!(ps.contains("function global:j { atlassian @args }"));
        assert!(ps.contains("function global:cf { conf @args }"));
        assert!(ps.contains("Get-Command 'j'"));

        let ps_force = render_init_script(ShellKind::Powershell, "j", "cf", true);
        assert!(!ps_force.contains("Get-Command"));
        assert!(ps_force.contains("function global:j { atlassian @args }"));

        let zsh = render_init_script(ShellKind::Zsh, "j", "cf", false);
        assert!(zsh.contains("j() { atlassian \"$@\"; }"));
        assert!(zsh.contains("cf() { conf \"$@\"; }"));
        assert!(zsh.contains("command -v j"));

        let fish = render_init_script(ShellKind::Fish, "j", "cf", false);
        assert!(fish.contains("function j --wraps atlassian"));
        assert!(fish.contains("function cf --wraps conf"));
        assert!(fish.contains("not type -q cf"));
    }

    #[test]
    fn powershell_force_script_removes_existing_alias() {
        // An alias wins over a function of the same name, so --force has to drop it first.
        let script = render_init_script(ShellKind::Powershell, "j", "cf", true);
        for name in ["j", "cf"] {
            let removal = format!("Remove-Item -Path Alias:{name} -ErrorAction SilentlyContinue");
            let removal_at = script
                .find(&removal)
                .unwrap_or_else(|| panic!("missing `{removal}` in:\n{script}"));
            let function_at = script
                .find(&format!("function global:{name} {{"))
                .unwrap();
            assert!(removal_at < function_at, "alias removal must precede the function:\n{script}");
        }
    }

    #[test]
    fn powershell_guard_does_not_clobber_existing_function() {
        let script = render_init_script(ShellKind::Powershell, "j", "cf", false);
        for name in ["j", "cf"] {
            let guard = script
                .lines()
                .find(|line| line.contains(&format!("Get-Command '{name}'")))
                .unwrap();
            let kinds = guard
                .split("-CommandType ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap();
            assert!(
                kinds.split(',').any(|kind| kind == "Function"),
                "guard for '{name}' must see existing functions: {guard}"
            );
        }
    }

    #[test]
    fn upsert_and_remove_managed_block_are_idempotent() {
        let block = render_managed_block(ShellKind::Zsh, "j", "cf", false);
        let initial = "export EDITOR=nvim\n";

        let (once, changed1) = upsert_managed_block(initial, &block);
        assert!(changed1);
        assert!(once.starts_with("export EDITOR=nvim\n\n# >>> bitefoundry atlassian aliases >>>"));
        assert!(extract_managed_block(&once).is_some());

        let (twice, changed2) = upsert_managed_block(&once, &block);
        assert!(!changed2);
        assert_eq!(once, twice);

        let updated_block = render_managed_block(ShellKind::Zsh, "jr", "cw", true);
        let (replaced, changed3) = upsert_managed_block(&once, &updated_block);
        assert!(changed3);
        assert!(replaced.contains("jr() { atlassian \"$@\"; }"));
        assert!(!replaced.contains("j() { atlassian \"$@\"; }"));

        let (removed, was_removed) = remove_managed_block(&replaced);
        assert!(was_removed);
        assert_eq!(removed, "export EDITOR=nvim\n");

        let (removed_again, was_removed_again) = remove_managed_block(&removed);
        assert!(!was_removed_again);
        assert_eq!(removed_again, "export EDITOR=nvim\n");
    }

    #[test]
    fn install_status_uninstall_roundtrip_on_temp_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("profile.ps1");
        fs::write(&profile, "# existing profile\n").unwrap();

        cmd_alias(AliasCommand::Install {
            shell: Some(ShellKind::Powershell),
            profile: Some(profile.clone()),
            jira: "j".into(),
            conf: "cf".into(),
            force: false,
            dry_run: true,
            json: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), "# existing profile\n");

        cmd_alias(AliasCommand::Install {
            shell: Some(ShellKind::Powershell),
            profile: Some(profile.clone()),
            jira: "j".into(),
            conf: "cf".into(),
            force: false,
            dry_run: false,
            json: false,
        })
        .unwrap();
        let installed = fs::read_to_string(&profile).unwrap();
        assert!(installed.contains("function global:j { atlassian @args }"));
        assert!(installed.contains("function global:cf { conf @args }"));

        cmd_alias(AliasCommand::Uninstall {
            shell: Some(ShellKind::Powershell),
            profile: Some(profile.clone()),
            dry_run: false,
            json: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&profile).unwrap(), "# existing profile\n");
    }

    #[test]
    fn detect_shell_from_handles_basenames_login_shells_and_rejects_unsupported() {
        assert_eq!(detect_shell_from(Some("/bin/tcsh")).unwrap(), ShellKind::Tcsh);
        assert_eq!(detect_shell_from(Some("/bin/csh")).unwrap(), ShellKind::Csh);
        assert_eq!(detect_shell_from(Some("/bin/sh")).unwrap(), ShellKind::Sh);
        assert_eq!(detect_shell_from(Some("/usr/bin/dash")).unwrap(), ShellKind::Sh);
        assert_eq!(detect_shell_from(Some("/bin/ash")).unwrap(), ShellKind::Sh);
        assert_eq!(detect_shell_from(Some("/bin/ksh")).unwrap(), ShellKind::Sh);
        assert_eq!(detect_shell_from(Some("/bin/mksh")).unwrap(), ShellKind::Sh);
        assert_eq!(detect_shell_from(Some("/bin/bash")).unwrap(), ShellKind::Bash);
        assert_eq!(detect_shell_from(Some("/bin/zsh")).unwrap(), ShellKind::Zsh);
        assert_eq!(detect_shell_from(Some("/usr/bin/fish")).unwrap(), ShellKind::Fish);

        // Login shells prefixed with '-'
        assert_eq!(detect_shell_from(Some("-tcsh")).unwrap(), ShellKind::Tcsh);
        assert_eq!(detect_shell_from(Some("-zsh")).unwrap(), ShellKind::Zsh);

        // Basename extraction avoids matching "bash" inside "/home/bashir/bin/tcsh"
        assert_eq!(
            detect_shell_from(Some("/home/bashir/bin/tcsh")).unwrap(),
            ShellKind::Tcsh
        );

        // Windows path & .exe suffix stripping
        assert_eq!(
            detect_shell_from(Some(r"C:\Program Files\PowerShell\7\pwsh.exe")).unwrap(),
            ShellKind::Powershell
        );

        // Unsupported shells error explicitly instead of silently falling back to bash
        for unsupported in ["/usr/local/bin/nu", "xonsh", "/usr/bin/elvish"] {
            let err = detect_shell_from(Some(unsupported)).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("Unsupported shell") && msg.contains(unsupported),
                "unexpected error for {unsupported}: {msg}"
            );
        }

        // Unset or empty falls back to platform default
        let expected_default = if cfg!(windows) {
            ShellKind::Powershell
        } else if cfg!(target_os = "macos") {
            ShellKind::Zsh
        } else {
            ShellKind::Bash
        };
        assert_eq!(detect_shell_from(None).unwrap(), expected_default);
        assert_eq!(detect_shell_from(Some("   ")).unwrap(), expected_default);
    }

    #[test]
    fn renders_scripts_and_validation_for_tcsh_csh_and_sh() {
        for shell in [ShellKind::Tcsh, ShellKind::Csh] {
            let guarded = render_init_script(shell, "j", "cf", false);
            assert_eq!(
                guarded,
                "which j >& /dev/null\nif ( $status != 0 ) alias j 'atlassian \\!*'\nwhich cf >& /dev/null\nif ( $status != 0 ) alias cf 'conf \\!*'\n"
            );
            let forced = render_init_script(shell, "j", "cf", true);
            assert_eq!(
                forced,
                "unalias j >& /dev/null\nalias j 'atlassian \\!*'\nunalias cf >& /dev/null\nalias cf 'conf \\!*'\n"
            );
            assert!(reload_instruction(shell, Path::new("/tmp/.tcshrc")).starts_with("source "));
        }

        let sh_guarded = render_init_script(ShellKind::Sh, "j", "cf", false);
        assert_eq!(sh_guarded, render_init_script(ShellKind::Bash, "j", "cf", false));
        let sh_forced = render_init_script(ShellKind::Sh, "j", "cf", true);
        assert_eq!(sh_forced, render_init_script(ShellKind::Bash, "j", "cf", true));
        assert!(reload_instruction(ShellKind::Sh, Path::new("/tmp/.profile")).starts_with(". "));
        assert!(reload_instruction(ShellKind::Powershell, Path::new("p.ps1")).starts_with(". "));

        // POSIX sh rejects hyphens in function identifiers
        assert!(validate_alias_pair_for_shell("my-jira", "cf", ShellKind::Sh).is_err());
        assert!(validate_alias_pair_for_shell("j", "my-conf", ShellKind::Sh).is_err());
        assert_eq!(
            validate_alias_pair_for_shell("my_jira", "my_conf", ShellKind::Sh).unwrap(),
            ("my_jira".into(), "my_conf".into())
        );
        assert!(validate_alias_pair_for_shell("my-jira", "my-conf", ShellKind::Bash).is_ok());
        assert!(validate_alias_pair_for_shell("my-jira", "my-conf", ShellKind::Tcsh).is_ok());
    }

    #[test]
    fn tcsh_preserves_existing_cshrc_when_tcshrc_absent() {
        let home = tempfile::tempdir().unwrap();
        let tcshrc = home.path().join(".tcshrc");
        let cshrc = home.path().join(".cshrc");

        // Neither exists -> defaults to .tcshrc
        assert_eq!(tcsh_profile_path(home.path()), tcshrc);

        // Only .cshrc exists -> MUST pick .cshrc so tcsh doesn't ignore it
        fs::write(&cshrc, "set prompt = '% '\n").unwrap();
        assert_eq!(tcsh_profile_path(home.path()), cshrc);

        // Both exist -> picks .tcshrc
        fs::write(&tcshrc, "set prompt = '> '\n").unwrap();
        assert_eq!(tcsh_profile_path(home.path()), tcshrc);

        // Bash chooses .bash_profile when .bashrc is absent and .bash_profile exists
        let bash_home = tempfile::tempdir().unwrap();
        let bashrc = bash_home.path().join(".bashrc");
        let bash_profile = bash_home.path().join(".bash_profile");
        assert_eq!(bash_profile_path(bash_home.path()), bashrc);
        fs::write(&bash_profile, "# bp\n").unwrap();
        assert_eq!(bash_profile_path(bash_home.path()), bash_profile);
        fs::write(&bashrc, "# rc\n").unwrap();
        assert_eq!(bash_profile_path(bash_home.path()), bashrc);

        // Sh honors ENV override if non-empty, else ~/.profile
        assert_eq!(
            sh_profile_path(bash_home.path(), Some(std::ffi::OsStr::new("/tmp/custom.sh"))),
            PathBuf::from("/tmp/custom.sh")
        );
        assert_eq!(
            sh_profile_path(bash_home.path(), None),
            bash_home.path().join(".profile")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_and_dangling_rc_files_preserve_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dotfiles = tmp.path().join("dotfiles");
        let home = tmp.path().join("home");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&home).unwrap();

        // 1. Valid symlink is preserved on install and uninstall
        let real_rc = dotfiles.join("tcshrc");
        fs::write(&real_rc, "set autolist\n").unwrap();
        let link_rc = home.join(".tcshrc");
        std::os::unix::fs::symlink(&real_rc, &link_rc).unwrap();

        cmd_alias(AliasCommand::Install {
            shell: Some(ShellKind::Tcsh),
            profile: Some(link_rc.clone()),
            jira: "j".into(),
            conf: "cf".into(),
            force: false,
            dry_run: false,
            json: false,
        })
        .unwrap();
        assert!(fs::symlink_metadata(&link_rc)
            .unwrap()
            .file_type()
            .is_symlink());
        let installed = fs::read_to_string(&real_rc).unwrap();
        assert!(installed.contains("set autolist"));
        assert!(installed.contains("alias j 'atlassian \\!*'"));

        cmd_alias(AliasCommand::Uninstall {
            shell: Some(ShellKind::Tcsh),
            profile: Some(link_rc.clone()),
            dry_run: false,
            json: false,
        })
        .unwrap();
        assert!(fs::symlink_metadata(&link_rc)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real_rc).unwrap(), "set autolist\n");

        // 2. Dangling relative symlink with missing target parent directory creates target cleanly
        let dangling_link = home.join(".profile");
        std::os::unix::fs::symlink(Path::new("../dotfiles/nested/sh_profile"), &dangling_link)
            .unwrap();
        assert!(!dangling_link.exists());

        cmd_alias(AliasCommand::Install {
            shell: Some(ShellKind::Sh),
            profile: Some(dangling_link.clone()),
            jira: "j".into(),
            conf: "cf".into(),
            force: false,
            dry_run: false,
            json: false,
        })
        .unwrap();
        assert!(fs::symlink_metadata(&dangling_link)
            .unwrap()
            .file_type()
            .is_symlink());
        let expected_target = dotfiles.join("nested").join("sh_profile");
        assert!(expected_target.exists());
        assert!(fs::read_to_string(&expected_target)
            .unwrap()
            .contains("j() { atlassian \"$@\"; }"));
    }
}
