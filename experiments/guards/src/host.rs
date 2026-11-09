//! Command-line composition of the stock host with optional experiment plugins.

use crate::rules::plugins::MessageLimitPlugin;
use crate::rules::{Arm, RulesConfig, RulesPlugin, RunRules};
use clap::{Arg, ArgAction, value_parser};
use polaris_code::startup::Host;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Build the stock host with all experiment options parsed alongside its flags.
/// No experiment plugins are installed when neither an arm nor a budget is set.
pub fn host() -> Host {
    let prepared = Arc::new(OnceLock::<RunRules>::new());
    let for_plugins = prepared.clone();
    Host::new()
        .arg(
            Arg::new("rules_arm")
                .long("rules-arm")
                .env("POLARIS_CODE_RULES_ARM")
                .value_parser(value_parser!(Arm))
                .ignore_case(true)
                .requires("print")
                .value_name("ARM")
                .help("Enforce task rules in none, prompt, per-call, steps, moved, or prompt-gate"),
        )
        .arg(
            Arg::new("protect")
                .long("protect")
                .env("POLARIS_CODE_PROTECT")
                .value_parser(value_parser!(PathBuf))
                .action(ArgAction::Append)
                .value_delimiter(',')
                .requires("rules_arm")
                .value_name("PATH")
                .help("Protected test file or directory; may be repeated"),
        )
        .arg(
            Arg::new("test_command")
                .long("test-command")
                .env("POLARIS_CODE_TEST_COMMAND")
                .requires("rules_arm")
                .value_name("COMMAND")
                .help("Sealed test command, run through sh -c"),
        )
        .arg(
            Arg::new("run_record")
                .long("run-record")
                .env("POLARIS_CODE_RUN_RECORD")
                .value_parser(value_parser!(PathBuf))
                .requires("rules_arm")
                .value_name("FILE")
                .help("Write the JSON run record here; no record file is written by default"),
        )
        .arg(
            Arg::new("check_timeout_secs")
                .long("check-timeout-secs")
                .env("POLARIS_CODE_CHECK_TIMEOUT_SECS")
                .value_parser(value_parser!(u64))
                .default_value("120")
                .requires("rules_arm")
                .value_name("SECONDS")
                .help("Time limit for each sealed check"),
        )
        .arg(
            Arg::new("max_holds")
                .long("max-holds")
                .env("POLARIS_CODE_MAX_HOLDS")
                .value_parser(value_parser!(u32))
                .default_value("3")
                .requires("rules_arm")
                .value_name("N")
                .help("Failed gate send-backs allowed before ending not_done"),
        )
        .arg(
            Arg::new("no_give_up")
                .long("no-give-up")
                .action(ArgAction::SetTrue)
                .requires("rules_arm")
                .help("Disable the CANNOT_COMPLETE final-message route"),
        )
        .arg(
            Arg::new("rules_file")
                .long("rules-file")
                .value_parser(value_parser!(PathBuf))
                .requires("rules_arm")
                .value_name("FILE")
                .help("Read replacement rule words verbatim as UTF-8"),
        )
        .arg(
            Arg::new("max_messages")
                .long("max-messages")
                .value_parser(value_parser!(usize))
                .value_name("N")
                .help("Bound canonical conversation messages independently of the arm"),
        )
        .arg(
            Arg::new("legacy_timeout")
                .long("legacy-model-timeout-secs")
                .env("POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS")
                .hide(true),
        )
        .configure(move |launch, options| {
            let args = launch.matches();
            if args.get_one::<polaris_code::providers::Provider>("provider")
                == Some(&polaris_code::providers::Provider::Bedrock)
                && args
                    .get_one::<polaris_code::providers::ModelRequestTimeout>(
                        "model_request_timeout_secs",
                    )
                    .is_none()
                && let Some(raw) = args
                    .get_one::<String>("legacy_timeout")
                    .filter(|text| !text.trim().is_empty())
            {
                options.model_request_timeout =
                    Some(raw.parse().map_err(|error| {
                        format!("POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS: {error}")
                    })?);
            }

            if args.get_one::<usize>("max_messages").is_some() {
                options.auto_route = false;
            }
            let rules = if let Some(arm) = args.get_one::<Arm>("rules_arm") {
                options.auto_route = false;
                let mut config = RulesConfig::new(*arm)
                    .with_protected(
                        args.get_many::<PathBuf>("protect")
                            .map(|values| values.cloned().collect())
                            .unwrap_or_default(),
                    )
                    .with_check_timeout(Duration::from_secs(
                        *args.get_one::<u64>("check_timeout_secs").unwrap(),
                    ))
                    .with_max_holds(*args.get_one::<u32>("max_holds").unwrap());
                if args.get_flag("no_give_up") {
                    config = config.without_give_up();
                }
                if let Some(path) = args.get_one::<PathBuf>("rules_file") {
                    let words = std::fs::read_to_string(path).map_err(|error| {
                        format!("reading rules file {}: {error}", path.display())
                    })?;
                    config = config.with_rules_text(words);
                }
                if let Some(maximum) = args.get_one::<usize>("max_messages") {
                    config = config.with_max_messages(*maximum);
                }
                if let Some(command) = args.get_one::<String>("test_command") {
                    config = config.with_test_command(command);
                }
                if let Some(path) = args.get_one::<PathBuf>("run_record") {
                    config = config.with_record_path(path);
                }
                RunRules::new(config, launch.working_dir())?
            } else {
                RunRules::inactive()
            };
            prepared.set(rules).expect("one host launch");
            Ok(())
        })
        .plugins(move |launch, server| {
            let rules = for_plugins
                .get()
                .expect("host configured before plugins")
                .clone();
            if rules.is_active() {
                server.add_plugins(RulesPlugin::new(rules));
            } else if let Some(maximum) = launch.matches().get_one::<usize>("max_messages") {
                server.add_plugins(MessageLimitPlugin::new(*maximum));
            }
            Ok(())
        })
}
