use std::env;

use clap::{Args, Subcommand};
use nh_core::{
  args::CommonRebuildArgs,
  checks::{
    FeatureRequirements, FlakeFeatures, LegacyFeatures, SystemReplFeatures,
  },
  installable::Installable,
  update::UpdateArgs,
};

/// System Manager functionality
#[derive(Debug, Args)]
pub struct SystemArgs {
  #[command(subcommand)]
  pub subcommand: SystemSubcommand,
}

impl SystemArgs {
  #[must_use]
  pub fn get_feature_requirements(&self) -> Box<dyn FeatureRequirements> {
    match &self.subcommand {
      SystemSubcommand::Repl(args) => {
        let is_flake = args.uses_flakes();
        Box::new(SystemReplFeatures { is_flake })
      },
      SystemSubcommand::Switch(args) | SystemSubcommand::Build(args) => {
        if args.uses_flakes() {
          Box::new(FlakeFeatures)
        } else {
          Box::new(LegacyFeatures)
        }
      },
    }
  }
}

#[derive(Debug, Subcommand)]
pub enum SystemSubcommand {
  /// Build and activate a system-manager configuration
  Switch(SystemRebuildArgs),
  /// Build a system-manager configuration
  Build(SystemRebuildArgs),
  /// Load a system-manager configuration in a Nix REPL
  Repl(SystemReplArgs),
}

#[derive(Debug, Args)]
pub struct SystemRebuildArgs {
  #[command(flatten)]
  pub common: CommonRebuildArgs,

  #[command(flatten)]
  pub update_args: UpdateArgs,

  /// When using a flake installable, select this hostname from systemConfigs
  #[arg(long, short = 'H', global = true)]
  pub hostname: Option<String>,

  /// Extra arguments passed to nix build
  #[arg(last = true)]
  pub extra_args: Vec<String>,

  /// Show activation logs
  #[arg(long, env = "NH_SHOW_ACTIVATION_LOGS", value_parser = clap::builder::BoolishValueParser::new())]
  pub show_activation_logs: bool,
}

impl SystemRebuildArgs {
  #[must_use]
  pub fn uses_flakes(&self) -> bool {
    if env::var("NH_SYSTEM_FLAKE").is_ok_and(|v| !v.is_empty()) {
      return true;
    }

    matches!(self.common.installable, Installable::Flake { .. })
  }
}

#[derive(Debug, Args)]
pub struct SystemReplArgs {
  #[command(flatten)]
  pub installable: Installable,

  /// When using a flake installable, select this hostname from systemConfigs
  #[arg(long, short = 'H', global = true)]
  pub hostname: Option<String>,
}

impl SystemReplArgs {
  #[must_use]
  pub fn uses_flakes(&self) -> bool {
    if env::var("NH_SYSTEM_FLAKE").is_ok_and(|v| !v.is_empty()) {
      return true;
    }

    matches!(self.installable, Installable::Flake { .. })
  }
}
