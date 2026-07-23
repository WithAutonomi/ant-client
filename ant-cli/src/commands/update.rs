use clap::Args;
use colored::Colorize;

use ant_core::node::binary::NoopProgress;
use ant_core::update;

use crate::progress::CliProgress;

#[derive(Args)]
pub struct UpdateArgs {
    /// Force re-download even if already on the latest version.
    #[arg(long)]
    pub force: bool,
}

impl UpdateArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let current_version = env!("CARGO_PKG_VERSION");

        if !json_output {
            eprintln!("{}", format!("Current version: {current_version}").dimmed());
            eprintln!("{}", "Checking for updates...".dimmed());
        }

        let mut check = update::check_for_update(current_version).await?;

        if !check.update_available && self.force {
            check.force()?;
        }

        if !check.update_available {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&check)?);
            } else {
                println!(
                    "{}",
                    format!("Already up to date (v{}).", check.current_version).green()
                );
            }
            return Ok(());
        }

        if !json_output {
            eprintln!(
                "{}",
                format!(
                    "Update available: v{} -> v{}",
                    check.current_version, check.latest_version
                )
                .cyan()
            );
        }

        let progress: Box<dyn ant_core::node::binary::ProgressReporter> = if json_output {
            Box::new(NoopProgress)
        } else {
            Box::new(CliProgress)
        };

        let result = update::perform_update(&check, progress.as_ref()).await?;

        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            println!(
                "{}",
                format!(
                    "Updated successfully: v{} -> v{}",
                    result.previous_version, result.new_version
                )
                .green()
            );
        }

        Ok(())
    }
}
