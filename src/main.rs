use clap::{Parser, Subcommand};
use std::process;

mod clickhouse;
mod cloudflare;
mod config;
mod config_check;
mod easyssh_mgr;
mod godaddy;
mod kerneltune;
mod logs_mgr;
mod mysql_mgr;
mod postgres_mgr;
mod secret;
mod ssh_exec;
mod ssh_tunnel;
mod sshuser;
mod sslcert;
mod tui;

#[derive(Parser)]
#[command(
    name = "atk",
    author = "or1k.net",
    about = "Admin Toolkit — SSH user provisioning, ClickHouse user management, GoDaddy DNS, all in one TUI"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "SSH user provisioning, scriptable (non-interactive)")]
    SshUser {
        #[command(subcommand)]
        action: SshUserCmd,
    },
}

#[derive(Subcommand)]
enum SshUserCmd {
    #[command(about = "User operations on remote host")]
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    #[command(about = "Manage SSH key profiles")]
    Profiles {
        #[command(subcommand)]
        action: ProfilesAction,
    },
    #[command(about = "Show or set default SSH settings")]
    Settings {
        #[command(subcommand)]
        action: SettingsAction,
    },
}

#[derive(Subcommand)]
enum UserAction {
    #[command(about = "Create user on remote and set authorized_keys")]
    Add {
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "")]
        port: String,
        #[arg(long)]
        profile: String,
    },
    #[command(about = "Remove user from remote")]
    Remove {
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "")]
        port: String,
        #[arg(long)]
        profile: String,
    },
}

#[derive(Subcommand)]
enum ProfilesAction {
    #[command(about = "List profiles")]
    List,
    #[command(about = "Add a profile")]
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        key: String,
    },
    #[command(about = "Delete a profile")]
    Delete {
        #[arg(long)]
        name: String,
    },
}

#[derive(Subcommand)]
enum SettingsAction {
    #[command(about = "Show settings")]
    Show,
    #[command(about = "Set settings")]
    Set {
        #[arg(long)]
        ssh_user: Option<String>,
        #[arg(long)]
        ssh_key_path: Option<String>,
        #[arg(long)]
        ssh_password: Option<String>,
        #[arg(long)]
        port: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("{e}");
        process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        None => tui::run()?,
        Some(Commands::SshUser { action }) => run_sshuser(action)?,
    }
    Ok(())
}

fn run_sshuser(action: SshUserCmd) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        SshUserCmd::User { action } => {
            let cfg = sshuser::config::load()?;
            match action {
                UserAction::Add { server, port, profile } => {
                    let port = if port.is_empty() { cfg.default_port.clone() } else { port };
                    let hosts = sshuser::commands::parse_server_list(&server);
                    if hosts.is_empty() {
                        return Err("no servers provided".into());
                    }
                    let prof = sshuser::find_profile(&cfg, &profile)
                        .ok_or_else(|| format!("profile {profile:?} not found"))?;
                    let commands = sshuser::commands::add_user_commands(&profile, &prof.key);
                    let creds = sshuser::make_creds(&cfg);
                    let mut failed = 0usize;
                    for host in &hosts {
                        match ssh_exec::run_commands(host, &port, &creds, &commands) {
                            Ok((_, stderr)) if stderr.trim().is_empty() => {
                                println!("[{host}] user {profile} added: ok")
                            }
                            Ok((_, stderr)) => {
                                println!("[{host}] user {profile} added: ok (warning: {})", stderr.trim())
                            }
                            Err(e) => {
                                failed += 1;
                                println!("[{host}] user {profile} not added: {e}");
                            }
                        }
                    }
                    if failed > 0 {
                        return Err(format!("completed with errors: {failed}/{}", hosts.len()).into());
                    }
                }
                UserAction::Remove { server, port, profile } => {
                    let port = if port.is_empty() { cfg.default_port.clone() } else { port };
                    let hosts = sshuser::commands::parse_server_list(&server);
                    if hosts.is_empty() {
                        return Err("no servers provided".into());
                    }
                    if sshuser::find_profile(&cfg, &profile).is_none() {
                        return Err(format!("profile {profile:?} not found").into());
                    }
                    let commands = sshuser::commands::remove_user_commands(&profile);
                    let creds = sshuser::make_creds(&cfg);
                    let mut failed = 0usize;
                    for host in &hosts {
                        match ssh_exec::run_commands(host, &port, &creds, &commands) {
                            Ok((_, stderr)) if stderr.trim().is_empty() => {
                                println!("[{host}] user {profile} removed: ok")
                            }
                            Ok((_, stderr)) => {
                                println!("[{host}] user {profile} removed: ok (warning: {})", stderr.trim())
                            }
                            Err(e) => {
                                failed += 1;
                                println!("[{host}] user {profile} not removed: {e}");
                            }
                        }
                    }
                    if failed > 0 {
                        return Err(format!("completed with errors: {failed}/{}", hosts.len()).into());
                    }
                }
            }
        }

        SshUserCmd::Profiles { action } => {
            let mut cfg = sshuser::config::load()?;
            match action {
                ProfilesAction::List => {
                    for p in &cfg.profiles {
                        println!("{}\t{}", p.name, p.key);
                    }
                }
                ProfilesAction::Add { name, key } => {
                    cfg.profiles.push(sshuser::config::Profile { name, key });
                    sshuser::config::save(&cfg)?;
                }
                ProfilesAction::Delete { name } => {
                    cfg.profiles.retain(|p| p.name != name);
                    sshuser::config::save(&cfg)?;
                }
            }
        }

        SshUserCmd::Settings { action } => {
            let mut cfg = sshuser::config::load()?;
            match action {
                SettingsAction::Show => {
                    println!("default_ssh_user: {}", cfg.default_ssh_user);
                    println!("default_ssh_key_path: {}", cfg.default_ssh_key_path);
                    println!("default_ssh_password: {}", cfg.default_ssh_password);
                    println!("default_port: {}", cfg.default_port);
                }
                SettingsAction::Set { ssh_user, ssh_key_path, ssh_password, port } => {
                    if let Some(v) = ssh_user {
                        cfg.default_ssh_user = v;
                    }
                    if let Some(v) = ssh_key_path {
                        cfg.default_ssh_key_path = v;
                    }
                    if let Some(v) = ssh_password {
                        cfg.default_ssh_password = v;
                    }
                    if let Some(v) = port {
                        cfg.default_port = v;
                    }
                    sshuser::config::save(&cfg)?;
                }
            }
        }
    }
    Ok(())
}
