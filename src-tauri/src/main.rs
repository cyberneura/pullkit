use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use pullkit_core::{load_config, sync_repos, RepoConfig, RepoStatus, SyncOutcome, SyncResult};
use std::collections::HashSet;
use tauri::Emitter;

#[derive(Parser)]
#[command(
    name = "pullkit",
    version,
    about = "Inspect and sync configured Git repositories"
)]
struct Args {
    /// Open the graphical interface.
    #[arg(long, global = true)]
    gui: bool,
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Sync all eligible repositories, or a named subset.
    Sync {
        /// Comma-separated repository names.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
    },
}

fn main() {
    let args = Args::parse();
    let result = if args.gui {
        run_gui()
    } else {
        run_cli(args.command)
    };
    if let Err(error) = result {
        eprintln!("pullkit: {error:#}");
        std::process::exit(1);
    }
}

fn run_cli(command: Option<CliCommand>) -> Result<()> {
    let config = load_config()?;
    match command {
        None => {
            println!(
                "{:<20} {:<8} {:<12} {:<8} PATH",
                "REPOSITORY", "TREE", "BRANCH", "ON MAIN"
            );
            for repo in &config.repos {
                let status = pullkit_core::inspect(repo);
                print_status(&status);
            }
            Ok(())
        }
        Some(CliCommand::Sync { only }) => {
            let repos = select_repos(&config.repos, &only)?;
            println!(
                "pullkit run: {} repositor{}",
                repos.len(),
                if repos.len() == 1 { "y" } else { "ies" }
            );
            println!();
            let results = sync_repos(&repos, |line| println!("{line}"));
            print_summary(&results);
            if results.iter().any(|r| {
                matches!(
                    r.outcome,
                    SyncOutcome::PullFailed | SyncOutcome::BuildFailed | SyncOutcome::StatusFailed
                )
            }) {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn select_repos(repos: &[RepoConfig], only: &[String]) -> Result<Vec<RepoConfig>> {
    if only.is_empty() {
        return Ok(repos.to_vec());
    }
    let wanted: HashSet<&str> = only.iter().map(String::as_str).collect();
    let known: HashSet<&str> = repos.iter().map(|repo| repo.name.as_str()).collect();
    let mut missing: Vec<_> = wanted.difference(&known).copied().collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        bail!("unknown repo name(s): {}", missing.join(", "));
    }
    Ok(repos
        .iter()
        .filter(|repo| wanted.contains(repo.name.as_str()))
        .cloned()
        .collect())
}

fn print_status(status: &RepoStatus) {
    if let Some(error) = &status.error {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {} ({error})",
            status.name,
            "error",
            "-",
            "-",
            status.path.display()
        );
    } else {
        println!(
            "{:<20} {:<8} {:<12} {:<8} {}",
            status.name,
            if status.clean { "clean" } else { "dirty" },
            status.branch.as_deref().unwrap_or("-"),
            if status.on_main { "yes" } else { "no" },
            status.path.display()
        );
    }
}

fn print_summary(results: &[SyncResult]) {
    println!("\nSummary");
    for result in results {
        println!(
            "  {:<20} {:<16} {}",
            result.name,
            format!("{:?}", result.outcome).to_lowercase(),
            result.message
        );
    }
}

#[tauri::command]
fn list_repos() -> Result<Vec<RepoStatus>, String> {
    let config = load_config().map_err(|error| format!("{error:#}"))?;
    Ok(config.repos.iter().map(pullkit_core::inspect).collect())
}

#[tauri::command]
async fn sync_selected(
    app: tauri::AppHandle,
    names: Vec<String>,
) -> Result<Vec<SyncResult>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let config = load_config().map_err(|error| format!("{error:#}"))?;
        let repos = select_repos(&config.repos, &names).map_err(|error| format!("{error:#}"))?;
        let results = sync_repos(&repos, |line| {
            let _ = app.emit("sync-log", line);
        });
        Ok(results)
    })
    .await
    .map_err(|error| error.to_string())?
}

fn run_gui() -> Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![list_repos, sync_selected])
        .run(tauri::generate_context!())?;
    Ok(())
}
