use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use clap_complete::{Shell, generate};
use git2::build::RepoBuilder;
use git2::{FetchOptions, RemoteCallbacks};
use indicatif::{ProgressBar, ProgressStyle};
use inquire::Confirm;
use qwx::QwxConfig;
use qwx::editor::{Mode, Qwx};
use std::env::{current_dir, set_current_dir};
use std::fs::{File, create_dir_all, read_to_string};
use std::io::Write;
use std::path::PathBuf;
use std::{env, io};

const HELP_CONTENT: &str = include_str!("../help.txt");

fn cli() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .version(env!("CARGO_PKG_VERSION"))
        .long_about(HELP_CONTENT)
        .subcommand(
            Command::new("open").about("Open a directory or file").arg(
                Arg::new("path")
                    .help("Path to open")
                    .required(true)
                    .action(ArgAction::Set)
                    .value_parser(value_parser!(PathBuf)),
            ),
        )
        .subcommand(
            Command::new("gen")
                .about("Gen auto completion for shell")
                .arg(
                    Arg::new("shell")
                        .help("Generate the auto completion script")
                        .value_parser(["bash", "zsh", "fish", "powershell", "elvish"]),
                ),
        )
        .subcommand(
            Command::new("config")
                .about("Manage the qwx.toml file")
                .subcommands([Command::new("edit").about("Open the config file")]),
        )
        .subcommand(
            Command::new("clone")
                .about("Get a git repository in a directory and open it in qwx")
                .arg(
                    Arg::new("url")
                        .required(true)
                        .action(ArgAction::Set)
                        .value_parser(value_parser!(String)),
                )
                .arg(
                    Arg::new("destination")
                        .required(true)
                        .action(ArgAction::Set)
                        .value_parser(value_parser!(PathBuf)),
                )
                .arg(
                    Arg::new("depth")
                        .long("depth")
                        .value_parser(value_parser!(i32))
                        .required(false)
                        .help("Number of commits to fetch"),
                ),
        )
}

fn clone_and_open(sub: &ArgMatches, c: &QwxConfig) -> io::Result<()> {
    let url = sub.get_one::<String>("url").expect("url is required");
    let destination = sub
        .get_one::<PathBuf>("destination")
        .expect("destination is required");
    let d = sub.get_one::<i32>("depth").unwrap_or(&0);
    let w = &mut io::stdout();
    let dest = destination.as_path();
    let x = current_dir()?;
    let p = x.join(destination);

    if p.is_dir() {
        set_current_dir(p.as_path())?;
        return Qwx::new(p.as_path(), Mode::Normal, c).as_mut().run(w);
    }

    let pb = ProgressBar::new(0);
    pb.set_style(
        ProgressStyle::default_bar()
            // {pos}/{len} will count objects or deltas depending on the phase
            .template(
                "{spinner:.white} [{elapsed_precise}] [{bar:40.white}] {pos:>7}/{len:7} {msg}",
            )
            .unwrap()
            .progress_chars("██  "),
    );

    let mut callbacks = RemoteCallbacks::new();
    let pb_clone = pb.clone();

    callbacks.transfer_progress(move |stats| {
        if stats.received_objects() < stats.total_objects() {
            // PHASE 1: Receiving from server
            pb_clone.set_length(stats.total_objects() as u64);
            pb_clone.set_position(stats.received_objects() as u64);

            // Display downloaded MiB in message text
            let mb_received = stats.received_bytes() as f64 / 1_048_576.0;
            pb_clone.set_message(format!("Receiving objects... ({:.2} MiB)", mb_received));
        } else {
            // PHASE 2: Resolving deltas locally
            pb_clone.set_length(stats.total_deltas() as u64);
            pb_clone.set_position(stats.indexed_deltas() as u64);
            pb_clone.set_message("Resolving deltas...");
        }
        true
    });
    let w = &mut io::stdout();
    let mut fetch_options = FetchOptions::new();
    fetch_options.depth(*d);
    fetch_options.remote_callbacks(callbacks);
    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch_options);

    pb.set_message(format!("Connecting to {}...", url));

    if let Err(e) = builder.clone(url, dest) {
        pb.abandon_with_message(format!("Clone failed: {e}"));
        return Err(io::Error::other(e.to_string()));
    }
    pb.finish_with_message("Clone and checkout completed.");
    set_current_dir(p.as_path())?;
    Qwx::new(p.as_path(), Mode::Normal, c).as_mut().run(w)
}

fn main() -> io::Result<()> {
    let path = dirs::config_dir().expect("no config dir");
    let conf_dir = path.join("qwx");
    let config_file = conf_dir.join("config.toml");
    let mut app = cli();
    let w = &mut io::stdout();
    let matches = app.clone().get_matches();
    if config_file.exists() {
        let config_content =
            std::fs::read_to_string(&config_file).expect("failed to read config file");
        let c: QwxConfig = toml::from_str(&config_content).unwrap();

        match matches.subcommand() {
            Some(("open", sub)) => {
                let p = sub.get_one::<PathBuf>("path").expect("path is required");
                Qwx::new(p.canonicalize()?.as_path(), Mode::Normal, &c)
                    .as_mut()
                    .run(w)
            }
            Some(("gen", sub)) => {
                let shell = sub.get_one::<String>("shell").expect("shell is required");
                let shell = match shell.as_str() {
                    "bash" => Shell::Bash,
                    "zsh" => Shell::Zsh,
                    "fish" => Shell::Fish,
                    "powershell" => Shell::PowerShell,
                    "elvish" => Shell::Elvish,
                    _ => unreachable!(),
                };
                generate(shell, &mut app, env!("CARGO_BIN_NAME"), w);
                Ok(())
            }
            Some(("clone", sub)) => clone_and_open(sub, &c),
            None => {
                if let Some(p) = matches.get_one::<PathBuf>("path") {
                    Qwx::new(p.canonicalize()?.as_path(), Mode::Normal, &c)
                        .as_mut()
                        .run(w)
                } else {
                    Qwx::new(current_dir()?.as_path(), Mode::Normal, &c)
                        .as_mut()
                        .run(w)
                }
            }
            Some(("config", sub)) => match sub.subcommand() {
                Some(("edit", _)) => {
                    if Confirm::new("Changes qwx keybindings ?")
                        .with_default(false)
                        .prompt()
                        .unwrap_or_default()
                    {
                        let config =
                            read_to_string(config_file.as_path()).expect("failed to read config");
                        let new_content = inquire::Editor::new("Configuration of qwx")
                            .with_predefined_text(&config)
                            .prompt();
                        if let Ok(x) = new_content {
                            let mut f = File::create(config_file.as_path())
                                .expect("failed to empty config file");
                            f.write_all(x.as_bytes())
                                .expect("faield to write new config");
                            f.sync_all().expect("failed to sync data");
                        }
                        Ok(())
                    } else {
                        Ok(())
                    }
                }
                _ => Ok(()),
            },
            _ => {
                app.clone().print_help()?;
                Ok(())
            }
        }
    } else {
        println!("Generating config at : {}", config_file.display());
        create_dir_all(conf_dir.as_path()).expect("failed to create qwx dir");
        println!(
            "Config directory created successfully at : {}",
            conf_dir.display()
        );
        let config = QwxConfig::default();
        let content = toml::to_string(&config).expect("msg");
        let mut c = File::create_new(config_file.as_path()).expect("msg");
        c.write_all(content.as_bytes()).expect("faield to write");
        c.sync_all().expect("faield to sync_all");
        println!("Config file generated successfully");
        Ok(())
    }
}
