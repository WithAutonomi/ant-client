use clap::Args;
use colored::Colorize;

use ant_core::install;
use ant_core::node::binary::NoopProgress;
use ant_core::node::types::UpgradeChannel;
use ant_core::update;

use crate::progress::CliProgress;

#[derive(Args)]
pub struct UpdateArgs {
    /// Force re-download even if already on the latest version.
    #[arg(long)]
    pub force: bool,

    /// Release channel to update along.
    ///
    /// Defaults to the channel the running binary belongs to: a `-beta.N` build stays on
    /// beta, anything else tracks stable.
    #[arg(long, value_enum)]
    pub channel: Option<UpdateChannelArg>,
}

/// CLI value for the update channel. Mirrors `ant node add --upgrade-channel`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum UpdateChannelArg {
    Stable,
    Beta,
}

impl From<UpdateChannelArg> for UpgradeChannel {
    fn from(arg: UpdateChannelArg) -> Self {
        match arg {
            UpdateChannelArg::Stable => Self::Stable,
            UpdateChannelArg::Beta => Self::Beta,
        }
    }
}

impl UpdateArgs {
    pub async fn execute(self, json_output: bool) -> anyhow::Result<()> {
        let current_version = env!("CARGO_PKG_VERSION");
        let channel = self
            .channel
            .map_or_else(|| update::channel_for_version(current_version), Into::into);

        if !json_output {
            eprintln!("{}", format!("Current version: {current_version}").dimmed());
            eprintln!(
                "{}",
                format!("Checking for updates on the {channel} channel...").dimmed()
            );
        }

        let mut check = update::check_for_update(current_version, channel).await?;

        // A package manager owns this binary. The version check above is still useful — the user
        // wants to know whether an update exists — but replacing the executable underneath the
        // manager would leave its metadata describing a file that is no longer there, and its
        // next update would silently roll the user back. Report and hand over.
        if let Some(deferred) = update::DeferredUpdate::new(&check, install::detect()) {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&deferred)?);
                return Ok(());
            }

            if deferred.update_available {
                println!(
                    "{}",
                    format!(
                        "Update available: v{} -> v{}",
                        deferred.current_version, deferred.latest_version
                    )
                    .cyan()
                );
            } else {
                println!(
                    "{}",
                    format!("Already up to date (v{}).", deferred.current_version).green()
                );
            }

            // Explain the refusal whenever the user could have expected something to happen:
            // an update is waiting, or they asked for a reinstall with --force.
            if deferred.update_available || self.force {
                let manager = deferred
                    .install_method
                    .package_manager()
                    .unwrap_or("a package manager");
                println!();
                println!("This ant was installed with {manager}, so it cannot replace itself.");
                println!("Update it with:");
                println!();
                println!("    {}", deferred.update_command.bold());
            }

            return Ok(());
        }

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
