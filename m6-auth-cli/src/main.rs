use anyhow::{bail, Context, Result};
use m6_auth::Db;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct Config {
    storage: StorageConfig,
}

#[derive(Deserialize, Default)]
struct StorageConfig {
    path: String,
}

fn load_config(config_path: &str) -> Result<PathBuf> {
    let p = Path::new(config_path);
    if !p.exists() {
        bail!("config file not found: {}", config_path);
    }
    let raw = std::fs::read_to_string(p)
        .with_context(|| format!("reading config file: {}", config_path))?;
    let cfg: Config = toml::from_str(&raw)
        .with_context(|| format!("parsing config file: {}", config_path))?;

    let db_path = if cfg.storage.path.is_empty() {
        bail!("config missing [storage] path");
    } else {
        let sp = Path::new(&cfg.storage.path);
        if sp.is_absolute() {
            sp.to_path_buf()
        } else {
            // relative to config file's directory
            let dir = p.parent().unwrap_or(Path::new("."));
            dir.join(sp)
        }
    };
    Ok(db_path)
}

// ── Password prompting ────────────────────────────────────────────────────────

fn get_password(pw_flag: Option<&str>, confirm: bool) -> Result<String> {
    if let Some(pw) = pw_flag {
        return Ok(pw.to_string());
    }
    let pw = rpassword::prompt_password("Password: ")
        .context("reading password")?;
    if confirm {
        let pw2 = rpassword::prompt_password("Confirm password: ")
            .context("reading password confirmation")?;
        if pw != pw2 {
            bail!("passwords do not match");
        }
    }
    Ok(pw)
}

// ── Output helpers ────────────────────────────────────────────────────────────

fn format_timestamp(ts: i64) -> String {
    // Simple formatting: convert unix timestamp to UTC date/time string
    // We do this without external crates by hand-rolling basic math
    let secs = ts as u64;
    // Days since epoch
    let days = secs / 86400;
    let rem_secs = secs % 86400;
    let hh = rem_secs / 3600;
    let mm = (rem_secs % 3600) / 60;
    let ss = rem_secs % 60;

    // Gregorian calendar computation
    let (year, month, day) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", year, month, day, hh, mm, ss)
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Usage ─────────────────────────────────────────────────────────────────────

fn print_usage() {
    eprintln!("Usage: m6-auth-cli <config> <entity> <command> [args] [flags]");
    eprintln!();
    eprintln!("User commands:");
    eprintln!("  m6-auth-cli <config> user ls [--json]");
    eprintln!("  m6-auth-cli <config> user add <username> [--role <role>]... [--password <pw>]");
    eprintln!("  m6-auth-cli <config> user del <username>");
    eprintln!("  m6-auth-cli <config> user passwd <username> [--password <pw>]");
    eprintln!("  m6-auth-cli <config> user roles <username> [--set <role>]... [--unset <role>]...");
    eprintln!();
    eprintln!("Group commands:");
    eprintln!("  m6-auth-cli <config> group ls [--json]");
    eprintln!("  m6-auth-cli <config> group add <name>");
    eprintln!("  m6-auth-cli <config> group del <name>");
    eprintln!("  m6-auth-cli <config> group member ls <group> [--json]");
    eprintln!("  m6-auth-cli <config> group member add <group> <username>");
    eprintln!("  m6-auth-cli <config> group member del <group> <username>");
}

// ── Argument parsing helpers ──────────────────────────────────────────────────

/// Collect all values for a repeated flag, e.g. `--role admin --role user`
fn collect_flag_values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
    let mut vals = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if i + 1 < args.len() {
                vals.push(args[i + 1].as_str());
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    vals
}

/// Get the single value for a flag, returning None if absent.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            if i + 1 < args.len() {
                return Some(args[i + 1].as_str());
            }
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_user_ls(db: &Db, args: &[String]) -> Result<()> {
    let json = has_flag(args, "--json");
    let users = db.user_list()?;
    if json {
        let out = serde_json::to_string(&users)?;
        println!("{}", out);
    } else {
        println!("{:<16} {:<24} {}", "USERNAME", "ROLES", "CREATED");
        for u in &users {
            let roles = u.roles.join(",");
            let created = format_timestamp(u.created_at);
            println!("{:<16} {:<24} {}", u.username, roles, created);
        }
    }
    Ok(())
}

fn cmd_user_add(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: user add <username> [--role <role>]... [--password <pw>]");
    }
    let username = &args[0];
    let roles = collect_flag_values(args, "--role");
    let pw_flag = flag_value(args, "--password");
    let password = get_password(pw_flag, true)?;

    match db.user_create(username, &password, &roles) {
        Ok(_) => {
            eprintln!("user '{}' created", username);
            Ok(())
        }
        Err(m6_auth::AuthError::UserExists(_)) => {
            bail!("username '{}' already exists", username);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_user_del(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: user del <username>");
    }
    let username = &args[0];
    match db.user_delete(username) {
        Ok(()) => {
            eprintln!("user '{}' deleted", username);
            Ok(())
        }
        Err(m6_auth::AuthError::UserNotFound(_)) => {
            bail!("user '{}' not found", username);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_user_passwd(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: user passwd <username> [--password <pw>]");
    }
    let username = &args[0];
    let pw_flag = flag_value(args, "--password");
    let password = get_password(pw_flag, true)?;

    match db.user_set_password(username, &password) {
        Ok(()) => {
            eprintln!("password updated for '{}'", username);
            Ok(())
        }
        Err(m6_auth::AuthError::UserNotFound(_)) => {
            bail!("user '{}' not found", username);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_user_roles(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: user roles <username> [--set <role>]... [--unset <role>]...");
    }
    let username = &args[0];
    let set_roles = collect_flag_values(args, "--set");
    let unset_roles = collect_flag_values(args, "--unset");

    if set_roles.is_empty() && unset_roles.is_empty() {
        // Just display current roles
        match db.user_get(username)? {
            None => bail!("user '{}' not found", username),
            Some(u) => {
                println!("{}", u.roles.join(","));
            }
        }
        return Ok(());
    }

    // Get current user
    let user = match db.user_get(username)? {
        None => bail!("user '{}' not found", username),
        Some(u) => u,
    };

    // Compute new roles: start with current, apply set/unset
    let mut roles: Vec<String> = user.roles.clone();

    for r in &set_roles {
        let rs = r.to_string();
        if !roles.contains(&rs) {
            roles.push(rs);
        }
    }
    for r in &unset_roles {
        roles.retain(|x| x != r);
    }

    let roles_ref: Vec<&str> = roles.iter().map(|s| s.as_str()).collect();
    match db.user_set_roles(username, &roles_ref) {
        Ok(()) => {
            eprintln!("roles updated for '{}'", username);
            Ok(())
        }
        Err(m6_auth::AuthError::UserNotFound(_)) => {
            bail!("user '{}' not found", username);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_group_ls(db: &Db, args: &[String]) -> Result<()> {
    let json = has_flag(args, "--json");
    let groups = db.group_list()?;
    if json {
        let out = serde_json::to_string(&groups)?;
        println!("{}", out);
    } else {
        println!("{:<16} {}", "GROUP", "MEMBERS");
        for g in &groups {
            println!("{:<16} {}", g.name, g.members.len());
        }
    }
    Ok(())
}

fn cmd_group_add(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: group add <name>");
    }
    let name = &args[0];
    match db.group_create(name) {
        Ok(_) => {
            eprintln!("group '{}' created", name);
            Ok(())
        }
        Err(m6_auth::AuthError::GroupExists(_)) => {
            bail!("group '{}' already exists", name);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_group_del(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: group del <name>");
    }
    let name = &args[0];
    match db.group_delete(name) {
        Ok(()) => {
            eprintln!("group '{}' deleted", name);
            Ok(())
        }
        Err(m6_auth::AuthError::GroupNotFound(_)) => {
            bail!("group '{}' not found", name);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_group_member_ls(db: &Db, args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: group member ls <group> [--json]");
    }
    let group = &args[0];
    let json = has_flag(args, "--json");

    let members = match db.group_members(group) {
        Ok(m) => m,
        Err(m6_auth::AuthError::GroupNotFound(_)) => {
            bail!("group '{}' not found", group);
        }
        Err(e) => return Err(e.into()),
    };

    if json {
        let out = serde_json::to_string(&members)?;
        println!("{}", out);
    } else {
        println!("{:<16} {}", "USERNAME", "ROLES");
        for u in &members {
            let roles = u.roles.join(",");
            println!("{:<16} {}", u.username, roles);
        }
    }
    Ok(())
}

fn cmd_group_member_add(db: &Db, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: group member add <group> <username>");
    }
    let group = &args[0];
    let username = &args[1];
    match db.group_member_add(group, username) {
        Ok(()) => {
            eprintln!("user '{}' added to group '{}'", username, group);
            Ok(())
        }
        Err(m6_auth::AuthError::GroupNotFound(_)) => {
            bail!("group '{}' not found", group);
        }
        Err(m6_auth::AuthError::UserNotFound(_)) => {
            bail!("user '{}' not found", username);
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_group_member_del(db: &Db, args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: group member del <group> <username>");
    }
    let group = &args[0];
    let username = &args[1];
    match db.group_member_remove(group, username) {
        Ok(()) => {
            eprintln!("user '{}' removed from group '{}'", username, group);
            Ok(())
        }
        Err(m6_auth::AuthError::GroupNotFound(_)) => {
            bail!("group '{}' not found", group);
        }
        Err(m6_auth::AuthError::UserNotFound(_)) => {
            bail!("user '{}' not found", username);
        }
        Err(e) => Err(e.into()),
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn run() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();

    if raw_args.len() < 4 {
        print_usage();
        process::exit(2);
    }

    let config_path = &raw_args[1];
    let entity = &raw_args[2];
    let command = &raw_args[3];
    let rest: Vec<String> = raw_args[4..].to_vec();

    // Load config and open db
    let db_path = load_config(config_path).map_err(|e| {
        // Re-tag config errors so they get exit code 2
        anyhow::anyhow!("__config_error__: {}", e)
    })?;

    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db directory: {}", parent.display()))?;
        }
    }

    let db = Db::open(&db_path).context("opening database")?;

    match entity.as_str() {
        "user" => match command.as_str() {
            "ls"     => cmd_user_ls(&db, &rest)?,
            "add"    => cmd_user_add(&db, &rest)?,
            "del"    => cmd_user_del(&db, &rest)?,
            "passwd" => cmd_user_passwd(&db, &rest)?,
            "roles"  => cmd_user_roles(&db, &rest)?,
            other    => {
                eprintln!("error: unknown user command: {}", other);
                print_usage();
                process::exit(2);
            }
        },
        "group" => match command.as_str() {
            "ls"  => cmd_group_ls(&db, &rest)?,
            "add" => cmd_group_add(&db, &rest)?,
            "del" => cmd_group_del(&db, &rest)?,
            "member" => {
                // group member <subcommand> [args]
                if rest.is_empty() {
                    eprintln!("error: missing group member subcommand");
                    print_usage();
                    process::exit(2);
                }
                let subcmd = &rest[0];
                let sub_rest: Vec<String> = rest[1..].to_vec();
                match subcmd.as_str() {
                    "ls"  => cmd_group_member_ls(&db, &sub_rest)?,
                    "add" => cmd_group_member_add(&db, &sub_rest)?,
                    "del" => cmd_group_member_del(&db, &sub_rest)?,
                    other => {
                        eprintln!("error: unknown group member subcommand: {}", other);
                        print_usage();
                        process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("error: unknown group command: {}", other);
                print_usage();
                process::exit(2);
            }
        },
        other => {
            eprintln!("error: unknown entity: {}", other);
            print_usage();
            process::exit(2);
        }
    }

    Ok(())
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            let msg = e.to_string();
            // config errors → exit 2
            if msg.starts_with("__config_error__:") {
                let clean = msg.trim_start_matches("__config_error__: ");
                eprintln!("error: {}", clean);
                process::exit(2);
            }
            // everything else → exit 1
            eprintln!("error: {}", e);
            process::exit(1);
        }
    }
}
