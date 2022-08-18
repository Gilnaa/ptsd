use clap::{AppSettings, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

#[derive(Parser, Debug)]
#[clap(author, version, about, allow_missing_positional = true, long_about = None, trailing_var_arg=true)]
struct PtsdArgs {
    #[clap(short, long, value_parser)]
    log_dir: Option<PathBuf>,

    #[clap(short, long)]
    shell: Option<String>,

    #[clap(multiple = true)]
    commands: Vec<String>,
}

#[tokio::main]
async fn main() {
    let args = PtsdArgs::parse();

    let m = MultiProgress::new();
    let sty = ProgressStyle::with_template(
        "[{elapsed_precise}] #{prefix} {spinner:8.cyan/blue} {msg:.cyan}",
    )
    .unwrap()
    .tick_strings(&[
        "🕛", "🕐", "🕑", "🕒", "🕓", "🕔", "🕕", "🕖", "🕗", "🕘", "🕙", "🕚",
    ]);

    let done_sty = ProgressStyle::with_template(
        "[{elapsed_precise}] #{prefix} {spinner:8.green} {msg:.green}",
    )
    .unwrap()
    .tick_strings(&["🎉"]);

    let fail_sty =
        ProgressStyle::with_template("[{elapsed_precise}] #{prefix} {spinner:8.red} {msg:.red}")
            .unwrap()
            .tick_strings(&["❌"]);

    let log_dir = args
        .log_dir
        .unwrap_or_else(|| tempfile::tempdir().unwrap().into_path());
    std::fs::create_dir_all(&log_dir).unwrap();
    eprintln!("Writing standard outputs to {log_dir:?}");

    let shell = args.shell.as_deref().unwrap_or("/bin/bash");

    let mut tasks = Vec::new();
    for (i, cmd) in args.commands.into_iter().enumerate() {
        let pb = m.add(ProgressBar::new_spinner());
        pb.set_style(sty.clone());
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_message(cmd.clone());
        pb.set_prefix(format!("{i}"));

        let done_sty = done_sty.clone();
        let fail_sty = fail_sty.clone();

        let mut stderr_file_path = log_dir.clone();
        stderr_file_path.push(format!("{i}.stderr"));
        let mut stdout_file_path = log_dir.clone();
        stdout_file_path.push(format!("{i}.stdout"));

        let stderr = std::fs::File::create(stderr_file_path).unwrap();
        let stdout = std::fs::File::create(stdout_file_path).unwrap();

        let proc = tokio::process::Command::new(shell)
            .arg("-c")
            .arg(cmd)
            .stderr(stderr)
            .stdout(stdout)
            .stdin(Stdio::null())
            .spawn();

        let mut proc = match proc {
            Ok(proc) => proc,
            Err(_) => {
                pb.set_style(fail_sty);
                pb.finish();
                return;
            }
        };

        let handle = tokio::spawn(async move {
            let res = proc.wait().await.unwrap();
            if res.success() {
                pb.set_style(done_sty);
            } else {
                pb.set_style(fail_sty);
            }
            pb.finish();
        });
        tasks.push(handle);
    }

    for t in tasks.into_iter() {
        t.await;
    }
}
