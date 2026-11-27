//! Argv pre-processing, smart view shortcut expansion, and hyphen-value guardrails.

use std::ffi::{OsStr, OsString};

use clap::Parser;

pub(crate) fn argv_requests_json(args: &[OsString]) -> bool {
    for arg in args.iter().skip(1) {
        let Some(text) = arg.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "--json" || text == "-j" {
            return true;
        }
        if let Some(short_cluster) = text.strip_prefix('-') {
            if !short_cluster.starts_with('-')
                && short_cluster.len() >= 2
                && short_cluster.bytes().all(|b| b.is_ascii_alphabetic())
                && short_cluster.contains('j')
            {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KnownFlagToken {
    Help,
    Other,
}

fn find_matching_option<'a>(
    active: &'a clap::Command,
    root: &'a clap::Command,
    flag: &str,
) -> Option<&'a clap::Arg> {
    if let Some(long_name) = flag.strip_prefix("--") {
        if long_name.is_empty() {
            return None;
        }
        let matches_long = |arg: &&clap::Arg| {
            !arg.is_positional()
                && (arg.get_long() == Some(long_name)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&long_name)))
        };
        active
            .get_arguments()
            .find(matches_long)
            .or_else(|| {
                root.get_arguments()
                    .filter(|arg| arg.is_global_set())
                    .find(matches_long)
            })
    } else if flag.starts_with('-') && flag.len() == 2 {
        let ch = flag.as_bytes()[1] as char;
        let matches_short = |arg: &&clap::Arg| {
            !arg.is_positional()
                && (arg.get_short() == Some(ch)
                    || arg
                        .get_all_short_aliases()
                        .is_some_and(|aliases| aliases.contains(&ch)))
        };
        active
            .get_arguments()
            .find(matches_short)
            .or_else(|| {
                root.get_arguments()
                    .filter(|arg| arg.is_global_set())
                    .find(matches_short)
            })
    } else {
        None
    }
}

fn classify_known_flag_or_terminator(
    active: &clap::Command,
    root: &clap::Command,
    token: &OsStr,
) -> Option<KnownFlagToken> {
    let text = token.to_str()?;
    if text == "--" {
        return Some(KnownFlagToken::Other);
    }
    if (text == "-h" || text == "--help") && !active.is_disable_help_flag_set() {
        return Some(KnownFlagToken::Help);
    }
    if (text == "-V" || text == "--version")
        && active.get_version().is_some()
        && !active.is_disable_version_flag_set()
    {
        return Some(KnownFlagToken::Other);
    }
    if text.starts_with("--") {
        let flag = text.split_once('=').map_or(text, |(left, _)| left);
        return find_matching_option(active, root, flag).map(|_| KnownFlagToken::Other);
    }
    if text.starts_with('-') && text.len() == 2 {
        return find_matching_option(active, root, text).map(|_| KnownFlagToken::Other);
    }
    if let Some(cluster) = text.strip_prefix('-') {
        if cluster.len() >= 2
            && !cluster.starts_with('-')
            && cluster.bytes().all(|b| b.is_ascii_alphabetic())
        {
            let mut single = [b'-', 0];
            let all_known = cluster.bytes().all(|b| {
                single[1] = b;
                let flag = std::str::from_utf8(&single).unwrap_or("");
                find_matching_option(active, root, flag).is_some()
            });
            if all_known {
                return Some(KnownFlagToken::Other);
            }
        }
    }
    None
}

fn is_jira_issue_key(token: &str) -> bool {
    let Some((project, number)) = token.split_once('-') else {
        return false;
    };
    if project.is_empty() || number.is_empty() || number.contains('-') {
        return false;
    }
    let mut chars = project.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && number.bytes().all(|b| b.is_ascii_digit())
}

fn is_confluence_page_id(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit())
}

fn has_subcommand_named(cmd: &clap::Command, token: &str) -> bool {
    cmd.get_subcommands()
        .any(|sub| sub.get_name() == token || sub.get_all_aliases().any(|a| a == token))
}

/// Expands shorthand target lookups into explicit `view` invocations:
/// - `atlassian ENG-42 [-j]` -> `atlassian view ENG-42 [-j]`
/// - `conf 40211 [-j]` -> `conf view 40211 [-j]`
/// - `atlassian wiki 40211 [-j]` -> `atlassian wiki view 40211 [-j]`
/// - `conf jira ENG-42 [-j]` -> `conf jira view ENG-42 [-j]`
fn expand_smart_view_shortcut(cmd: &clap::Command, name: &str, argv: &mut Vec<OsString>) {
    let is_confluence_root = matches!(name, "confluence" | "conf");
    let mut i = 1usize;
    while i < argv.len() {
        let Some(text) = argv[i].to_str() else {
            return;
        };
        if text == "--" || text == "-h" || text == "--help" || text == "-V" || text == "--version" {
            return;
        }
        if text == "--site" {
            i += 2;
            continue;
        }
        if text.starts_with("--site=") {
            i += 1;
            continue;
        }
        if text.starts_with('-') {
            return;
        }

        // Handle cross-mounted namespace shortcuts: `atlassian wiki 40211` / `conf jira ENG-42`
        if !is_confluence_root && matches!(text, "wiki" | "conf") {
            if let Some(next_tok) = argv.get(i + 1).and_then(|s| s.to_str()) {
                if let Some(wiki_sub) = cmd.find_subcommand("wiki") {
                    if !next_tok.starts_with('-')
                        && !has_subcommand_named(wiki_sub, next_tok)
                        && is_confluence_page_id(next_tok)
                    {
                        argv.insert(i + 1, OsString::from("view"));
                    }
                }
            }
            return;
        }
        if is_confluence_root && text == "jira" {
            if let Some(next_tok) = argv.get(i + 1).and_then(|s| s.to_str()) {
                if let Some(jira_sub) = cmd.find_subcommand("jira") {
                    if !next_tok.starts_with('-')
                        && !has_subcommand_named(jira_sub, next_tok)
                        && is_jira_issue_key(next_tok)
                    {
                        argv.insert(i + 1, OsString::from("view"));
                    }
                }
            }
            return;
        }

        if has_subcommand_named(cmd, text) {
            return;
        }
        let matches_target = if is_confluence_root {
            is_confluence_page_id(text)
        } else {
            is_jira_issue_key(text)
        };
        if matches_target {
            argv.insert(i, OsString::from("view"));
        }
        return;
    }
}

fn disable_hyphen_values_on_arg(
    cmd: clap::Command,
    subcmd_path: &[String],
    arg_id: &str,
) -> clap::Command {
    if let Some((first, rest)) = subcmd_path.split_first() {
        cmd.mut_subcommand(first, |sub| {
            disable_hyphen_values_on_arg(sub, rest, arg_id)
        })
    } else {
        cmd.mut_arg(arg_id, |arg| arg.allow_hyphen_values(false))
    }
}

enum OptionFlagViolation {
    Help(Vec<OsString>),
    FlagInValue {
        subcmd_path: Vec<String>,
        arg_id: String,
        err_argv: Vec<OsString>,
    },
}

fn has_potential_flag_in_option_value(argv: &[OsString]) -> bool {
    let mut dash_tokens = 0usize;
    for token in argv.iter().skip(1) {
        let Some(text) = token.to_str() else {
            continue;
        };
        if text.starts_with('-') && text != "-" {
            dash_tokens += 1;
            if dash_tokens >= 2 {
                return true;
            }
            if text == "--" {
                break;
            }
        }
    }
    false
}

fn find_option_flag_violation(
    cmd: &clap::Command,
    argv: &[OsString],
) -> Option<OptionFlagViolation> {
    let mut active = cmd;
    let mut subcmd_path: Vec<String> = Vec::new();
    let mut i = 1;
    while i < argv.len() {
        let token = &argv[i];
        if token == "--" {
            break;
        }
        if let Some(text) = token.to_str() {
            if (text == "-h" || text == "--help") && !active.is_disable_help_flag_set() {
                return None;
            }
            if let Some((flag, _)) = text.split_once('=') {
                if flag.starts_with("--") && find_matching_option(active, cmd, flag).is_some() {
                    i += 1;
                    continue;
                }
            }
            if let Some(opt) = find_matching_option(active, cmd, text) {
                let takes = opt
                    .get_num_args()
                    .map(|range| range.min_values())
                    .unwrap_or_else(|| usize::from(opt.get_action().takes_values()));
                if takes == 0 {
                    i += 1;
                    continue;
                }
                if opt.is_allow_hyphen_values_set() {
                    for k in 0..takes {
                        let Some(next_tok) = argv.get(i + 1 + k) else {
                            break;
                        };
                        if let Some(kind) =
                            classify_known_flag_or_terminator(active, cmd, next_tok)
                        {
                            if kind == KnownFlagToken::Help {
                                let mut help_argv = vec![argv[0].clone()];
                                help_argv.extend(subcmd_path.iter().map(OsString::from));
                                help_argv.push(next_tok.clone());
                                return Some(OptionFlagViolation::Help(help_argv));
                            }
                            let mut err_argv = argv.to_vec();
                            for prev in 0..k {
                                err_argv[i + 1 + prev] = OsString::from("placeholder");
                            }
                            return Some(OptionFlagViolation::FlagInValue {
                                subcmd_path,
                                arg_id: opt.get_id().as_str().to_string(),
                                err_argv,
                            });
                        }
                    }
                    i += 1 + takes;
                    continue;
                }
                let mut consumed = 0;
                while consumed < takes {
                    let Some(next_tok) = argv.get(i + 1 + consumed) else {
                        break;
                    };
                    if next_tok
                        .to_str()
                        .is_some_and(|s| s.starts_with('-') && s != "-")
                    {
                        break;
                    }
                    consumed += 1;
                }
                i += 1 + consumed;
                continue;
            }
            if active.has_subcommands() {
                if let Some(sub) = active.get_subcommands().find(|sub| {
                    sub.get_name() == text
                        || sub.get_all_aliases().any(|alias| alias == text)
                }) {
                    subcmd_path.push(sub.get_name().to_string());
                    active = sub;
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    None
}

pub(crate) fn try_parse_cli_from<T: Parser>(
    name: &'static str,
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<T, clap::Error> {
    let mut argv: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let cmd = T::command().name(name).bin_name(name);
    expand_smart_view_shortcut(&cmd, name, &mut argv);

    if has_potential_flag_in_option_value(&argv) {
        if let Some(violation) = find_option_flag_violation(&cmd, &argv) {
            let parsed = match violation {
                OptionFlagViolation::Help(help_argv) => cmd.try_get_matches_from(help_argv),
                OptionFlagViolation::FlagInValue {
                    subcmd_path,
                    arg_id,
                    err_argv,
                } => {
                    let err_cmd = disable_hyphen_values_on_arg(cmd, &subcmd_path, &arg_id);
                    err_cmd.try_get_matches_from(err_argv)
                }
            };
            return Err(parsed.err().unwrap_or_else(|| {
                clap::Error::raw(
                    clap::error::ErrorKind::InvalidValue,
                    "expected a value before the next option",
                )
            }));
        }
    }

    let mut matches = cmd.try_get_matches_from(argv)?;
    T::from_arg_matches_mut(&mut matches)
}

pub(super) fn parse_cli_with_stack<T: Parser + Send + 'static>(name: &'static str) -> T {
    let parse = move || {
        try_parse_cli_from::<T>(name, std::env::args_os()).unwrap_or_else(|error| error.exit())
    };
    std::thread::Builder::new()
        .name("atlassian-cli-parse".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(parse)
        .ok()
        .and_then(|handle| handle.join().ok())
        .unwrap_or_else(parse)
}

