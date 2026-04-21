pub mod args;

use std::{ffi::OsString, path::PathBuf};

use args::{SystemArgs, SystemRebuildArgs, SystemReplArgs, SystemSubcommand};
use color_eyre::{
  Result,
  eyre::{Context, bail},
};
use nh_core::{
  command::{self, Command, ElevationStrategy},
  installable::{CommandContext, Installable},
  update::update,
  util::{get_hostname, print_dix_diff},
};
use tracing::{debug, warn};

const SYSTEM_PROFILE: &str =
  "/nix/var/nix/profiles/system-manager-profiles/system-manager";
const DEFAULT_PROFILE: &str = "default";
const SYSTEM_CONFIGS_ATTR: &str = "systemConfigs";

impl SystemArgs {
  /// Run the `system` subcommand.
  ///
  /// # Errors
  ///
  /// Returns an error if build, evaluation, or activation fails.
  pub fn run(self, elevation: ElevationStrategy) -> Result<()> {
    use SystemRebuildVariant::{Build, Switch};
    match self.subcommand {
      SystemSubcommand::Switch(args) => args.rebuild(&Switch, elevation),
      SystemSubcommand::Build(args) => {
        if args.common.ask || args.common.dry {
          warn!("`--ask` and `--dry` have no effect for `nh system build`");
        }
        args.rebuild(&Build, elevation)
      },
      SystemSubcommand::Repl(args) => args.run(),
    }
  }
}

enum SystemRebuildVariant {
  Switch,
  Build,
}

impl SystemRebuildArgs {
  fn rebuild(
    self,
    variant: &SystemRebuildVariant,
    elevation: ElevationStrategy,
  ) -> Result<()> {
    use SystemRebuildVariant::Build;

    let (out_path, _tempdir_guard): (PathBuf, Option<tempfile::TempDir>) =
      if let Some(ref p) = self.common.out_link {
        (p.clone(), None)
      } else {
        let dir = tempfile::Builder::new().prefix("nh-system").tempdir()?;
        (dir.as_ref().join("result"), Some(dir))
      };

    debug!("Output path: {out_path:?}");

    let installable = self
      .common
      .installable
      .clone()
      .resolve(CommandContext::System)?;

    let installable = match installable {
      Installable::Unspecified => Installable::try_find_default_for_system()?,
      other => other,
    };

    if self.update_args.update_all || self.update_args.update_input.is_some() {
      update(
        &installable,
        self.update_args.update_input,
        self.common.passthrough.commit_lock_file,
      )?;
    }

    let target =
      select_installable(installable, self.hostname, &self.extra_args)?;

    command::Build::new(target)
      .extra_arg("--out-link")
      .extra_arg(&out_path)
      .extra_args(&self.extra_args)
      .passthrough(&self.common.passthrough)
      .message("Building System Manager configuration")
      .nom(!self.common.no_nom)
      .run()
      .wrap_err("Failed to build System Manager configuration")?;

    if !matches!(self.common.diff, nh_core::args::DiffType::Never) {
      let current_profile = PathBuf::from(SYSTEM_PROFILE);
      if current_profile.exists() {
        let _ = print_dix_diff(&current_profile, &out_path);
      }
    }

    if self.common.dry || matches!(variant, Build) {
      if self.common.ask {
        warn!("--ask has no effect as dry run was requested");
      }
      return Ok(());
    }

    if self.common.ask {
      let confirmation = inquire::Confirm::new("Apply the config?")
        .with_default(false)
        .prompt()?;

      if !confirmation {
        bail!("User rejected the new config");
      }
    }

    let activate = out_path.join("bin/activate");
    let needs_elevation = !nix::unistd::Uid::effective().is_root();

    Command::new(&activate)
      .message("Activating configuration")
      .elevate(needs_elevation.then_some(elevation))
      .show_output(self.show_activation_logs)
      .with_required_env()
      .run()
      .wrap_err("System Manager activation failed")?;

    Ok(())
  }
}

impl SystemReplArgs {
  fn run(self) -> Result<()> {
    let installable = self.installable.resolve(CommandContext::System)?;

    let installable = match installable {
      Installable::Unspecified => Installable::try_find_default_for_system()?,
      other => other,
    };

    if matches!(installable, Installable::Store { .. }) {
      bail!("Nix doesn't support nix store installables.");
    }

    let extra_args = Vec::new();
    let target = select_installable(installable, self.hostname, &extra_args)?;

    Command::new("nix")
      .arg("repl")
      .args(target.to_args())
      .with_required_env()
      .show_output(true)
      .run()?;

    Ok(())
  }
}

fn select_installable(
  installable: Installable,
  hostname: Option<String>,
  extra_args: &[String],
) -> Result<Installable> {
  let extra_args: Vec<OsString> =
    extra_args.iter().map(OsString::from).collect();

  match installable {
    Installable::Flake {
      reference,
      attribute,
    } => select_flake_installable(reference, attribute, hostname, &extra_args),
    Installable::File {
      path,
      mut attribute,
    } => {
      normalize_nonflake_attribute(&mut attribute)?;
      Ok(Installable::File { path, attribute })
    },
    Installable::Expression {
      expression,
      mut attribute,
    } => {
      normalize_nonflake_attribute(&mut attribute)?;
      Ok(Installable::Expression {
        expression,
        attribute,
      })
    },
    other @ Installable::Store { .. } => Ok(other),
    Installable::Unspecified => {
      unreachable!(
        "Unspecified installable should have been resolved before calling \
         select_installable"
      )
    },
  }
}

fn select_flake_installable(
  reference: String,
  attribute: Vec<String>,
  hostname: Option<String>,
  extra_args: &[OsString],
) -> Result<Installable> {
  let current_system = get_current_system()?;
  let hostname = get_hostname(hostname)?;
  let candidates =
    flake_attr_candidates(attribute, &current_system, &hostname)?;

  resolve_flake_candidate(reference, candidates, |installable| {
    nix_eval_succeeds(installable, extra_args)
  })
}

fn resolve_flake_candidate<F>(
  reference: String,
  candidates: Vec<Vec<String>>,
  mut exists: F,
) -> Result<Installable>
where
  F: FnMut(&Installable) -> Result<bool>,
{
  let mut tried = Vec::new();

  for candidate in candidates {
    let installable = Installable::Flake {
      reference: reference.clone(),
      attribute: candidate,
    };
    tried.push(installable.to_args().join(" "));

    if exists(&installable)? {
      debug!(
        "Using System Manager installable: {:?}",
        installable.to_args()
      );
      return Ok(installable);
    }
  }

  bail!(
    "Couldn't find system-manager configuration automatically, tried: {}",
    tried.join(", ")
  );
}

fn normalize_nonflake_attribute(attribute: &mut Vec<String>) -> Result<()> {
  if attribute.is_empty() {
    bail!(
      "System Manager requires an explicit attribute path when using --file \
       or --expr"
    );
  }

  if attribute.first().map(String::as_str) != Some(SYSTEM_CONFIGS_ATTR) {
    attribute.insert(0, SYSTEM_CONFIGS_ATTR.to_string());
  }

  if attribute.len() > 3 {
    bail!(
      "Attribute path is too specific: {}. Please specify only the \
       configuration name (e.g., '.#{}') or the explicit systemConfigs path.",
      attribute.join("."),
      DEFAULT_PROFILE
    );
  }

  Ok(())
}

fn flake_attr_candidates(
  attribute: Vec<String>,
  current_system: &str,
  hostname: &str,
) -> Result<Vec<Vec<String>>> {
  let mut res = Vec::new();

  match attribute.as_slice() {
    [] => {
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        current_system.to_string(),
        hostname.to_string(),
      ]);
      res.push(vec![SYSTEM_CONFIGS_ATTR.to_string(), hostname.to_string()]);
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        current_system.to_string(),
        DEFAULT_PROFILE.to_string(),
      ]);
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        DEFAULT_PROFILE.to_string(),
      ]);
    },
    [single] if single == SYSTEM_CONFIGS_ATTR => {
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        current_system.to_string(),
        hostname.to_string(),
      ]);
      res.push(vec![SYSTEM_CONFIGS_ATTR.to_string(), hostname.to_string()]);
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        current_system.to_string(),
        DEFAULT_PROFILE.to_string(),
      ]);
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        DEFAULT_PROFILE.to_string(),
      ]);
    },
    [single] => {
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        current_system.to_string(),
        single.clone(),
      ]);
      res.push(vec![SYSTEM_CONFIGS_ATTR.to_string(), single.clone()]);
    },
    [root, name] if root == SYSTEM_CONFIGS_ATTR => {
      res.push(vec![root.clone(), name.clone()]);
    },
    [system, name] => {
      res.push(vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        system.clone(),
        name.clone(),
      ]);
    },
    [root, system, name] if root == SYSTEM_CONFIGS_ATTR => {
      res.push(vec![root.clone(), system.clone(), name.clone()]);
    },
    _ => {
      bail!(
        "Attribute path is too specific: {}. Please either:\n  1. Use the \
         flake reference without attributes (e.g., '.')\n  2. Specify only \
         the configuration name (e.g., '.#{}')\n  3. Use an explicit \
         systemConfigs path (e.g., '.#systemConfigs.{}.{}')",
        attribute.join("."),
        hostname,
        current_system,
        hostname
      );
    },
  }

  Ok(res)
}

fn nix_eval_succeeds(
  installable: &Installable,
  extra_args: &[OsString],
) -> Result<bool> {
  let res = Command::new("nix")
    .with_required_env()
    .arg("eval")
    .args(extra_args)
    .args(installable.to_args())
    .run();

  match res {
    Ok(()) => Ok(true),
    Err(err) => {
      debug!("nix eval failed for {:?}: {err}", installable.to_args());
      Ok(false)
    },
  }
}

fn get_current_system() -> Result<String> {
  let output = Command::new("nix")
    .with_required_env()
    .args(["config", "show", "system"])
    .run_capture()
    .wrap_err("Failed to determine current Nix system")?;

  let system = output
    .as_deref()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .ok_or_else(|| {
      color_eyre::eyre::eyre!("Couldn't determine current Nix system")
    })?;

  Ok(system.to_string())
}

#[cfg(test)]
mod tests {
  use super::{
    DEFAULT_PROFILE, SYSTEM_CONFIGS_ATTR, flake_attr_candidates,
    resolve_flake_candidate,
  };
  use nh_core::installable::Installable;

  #[test]
  fn test_flake_attr_candidates_for_default_resolution() {
    let candidates =
      flake_attr_candidates(vec![], "x86_64-linux", "edge").unwrap();

    assert_eq!(
      candidates,
      vec![
        vec![
          SYSTEM_CONFIGS_ATTR.to_string(),
          "x86_64-linux".to_string(),
          "edge".to_string()
        ],
        vec![SYSTEM_CONFIGS_ATTR.to_string(), "edge".to_string()],
        vec![
          SYSTEM_CONFIGS_ATTR.to_string(),
          "x86_64-linux".to_string(),
          DEFAULT_PROFILE.to_string()
        ],
        vec![SYSTEM_CONFIGS_ATTR.to_string(), DEFAULT_PROFILE.to_string()],
      ]
    );
  }

  #[test]
  fn test_flake_attr_candidates_for_named_configuration() {
    let candidates =
      flake_attr_candidates(vec!["server".to_string()], "x86_64-linux", "edge")
        .unwrap();

    assert_eq!(
      candidates,
      vec![
        vec![
          SYSTEM_CONFIGS_ATTR.to_string(),
          "x86_64-linux".to_string(),
          "server".to_string()
        ],
        vec![SYSTEM_CONFIGS_ATTR.to_string(), "server".to_string()],
      ]
    );
  }

  #[test]
  fn test_flake_attr_candidates_for_explicit_system_path() {
    let candidates = flake_attr_candidates(
      vec!["aarch64-linux".to_string(), "server".to_string()],
      "x86_64-linux",
      "edge",
    )
    .unwrap();

    assert_eq!(
      candidates,
      vec![vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        "aarch64-linux".to_string(),
        "server".to_string(),
      ]]
    );
  }

  #[test]
  fn test_flake_attr_candidates_rejects_too_specific_paths() {
    let err = flake_attr_candidates(
      vec![
        SYSTEM_CONFIGS_ATTR.to_string(),
        "x86_64-linux".to_string(),
        "server".to_string(),
        "extra".to_string(),
      ],
      "x86_64-linux",
      "edge",
    )
    .unwrap_err();

    assert!(err.to_string().contains("Attribute path is too specific"));
  }

  #[test]
  fn test_resolve_flake_candidate_uses_default_fallback() {
    let reference = "/tmp/system-manager-fixture".to_string();
    let candidates =
      flake_attr_candidates(vec![], "x86_64-linux", "missing-host").unwrap();

    let resolved =
      resolve_flake_candidate(reference.clone(), candidates, |installable| {
        Ok(matches!(
          installable,
          Installable::Flake { attribute, .. }
            if attribute
              == &vec![SYSTEM_CONFIGS_ATTR.to_string(), DEFAULT_PROFILE.to_string()]
        ))
      })
      .expect("candidate resolution should fall back to systemConfigs.default");

    match resolved {
      Installable::Flake {
        reference: resolved_reference,
        attribute,
      } => {
        assert_eq!(resolved_reference, reference);
        assert_eq!(
          attribute,
          vec![SYSTEM_CONFIGS_ATTR.to_string(), DEFAULT_PROFILE.to_string()]
        );
      },
      other => panic!("Expected flake installable, got {other:?}"),
    }
  }
}
