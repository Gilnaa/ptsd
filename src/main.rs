use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

#[derive(Parser, Debug)]
#[clap(
    author,
    version,
    about = "Parallel Thing/Stuff Doer",
    allow_missing_positional = true,
    long_about = "Parallel Thing/Stuff Doer\n\nRun commands in parallel and report failures"
)]
struct PtsdArgs {
    /// Directory path to place command outputs in. If left unspecified, a temporary directory will be generated
    #[clap(long, value_parser)]
    log_dir: Option<PathBuf>,

    /// Use a specific shell to execute the commands
    #[clap(short, long, default_value = "/bin/bash")]
    shell: String,

    /// A set of commands to run
    #[clap(multiple = true)]
    commands: Vec<String>,

    /// Read commands from a file, line by line
    #[clap(long)]
    command_file: Option<PathBuf>,

    /// Disable progress bars, only print failure report
    #[clap(long, takes_value = false)]
    disable_progress: bool,

    /// Limit the number of jobs that will run in parallel.
    /// If unspecified, a sensible value will be chosen based on available
    /// parallelism capabilities.
    #[clap(short, long)]
    jobs: Option<NonZeroUsize>,
}

const PROGRESS_TICK_FRAMES: &[&str] = &[
    "( ●    )",
    "(  ●   )",
    "(   ●  )",
    "(    ● )",
    "(     ●)",
    "(    ● )",
    "(   ●  )",
    "(  ●   )",
    "( ●    )",
    "(●     )",
];

#[derive(Clone)]
struct ProgressStylesByState {
    progress: ProgressStyle,
    done: ProgressStyle,
    fail: ProgressStyle,
}

fn init_progress_styles() -> ProgressStylesByState {
    let progress =
        ProgressStyle::with_template("[{elapsed_precise}] #{prefix} {spinner:8.cyan} {msg:.cyan}")
            .unwrap()
            .tick_strings(PROGRESS_TICK_FRAMES);

    let done = ProgressStyle::with_template(
        "[{elapsed_precise}] #{prefix} {spinner:8.green} {msg:.green}",
    )
    .unwrap()
    .tick_strings(&["🎉"]);

    let fail =
        ProgressStyle::with_template("[{elapsed_precise}] #{prefix} {spinner:8.red} {msg:.red}")
            .unwrap()
            .tick_strings(&["❌"]);

    ProgressStylesByState {
        progress,
        done,
        fail,
    }
}

fn spawn_task_process(
    log_dir: &PathBuf,
    task_name: &str,
    shell: &str,
    cmd: &str,
) -> std::io::Result<tokio::process::Child> {
    let mut stdout_file_path = log_dir.clone();
    stdout_file_path.push(format!("{task_name}.stdout"));
    let stdout = std::fs::File::create(stdout_file_path).unwrap();

    let mut stderr_file_path = log_dir.clone();
    stderr_file_path.push(format!("{task_name}.stderr"));
    let stderr = std::fs::File::create(stderr_file_path).unwrap();

    tokio::process::Command::new(shell)
        .arg("-c")
        .arg(cmd)
        .stderr(stderr)
        .stdout(stdout)
        .stdin(Stdio::null())
        .spawn()
}

#[tokio::main]
async fn main() {
    let mut args = PtsdArgs::parse();

    let multi_progress_bar = if args.disable_progress {
        None
    } else {
        Some(MultiProgress::new())
    };

    let styles = init_progress_styles();

    if let Some(file_path) = args.command_file {
        let extra_commands = match std::fs::read_to_string(&file_path) {
            Err(e) => {
                eprintln!("Failed reading extra commands from {file_path:?}: {e}");
                std::process::exit(1);
            }
            Ok(cmds) => cmds,
        };
        args.commands
            .extend(extra_commands.lines().map(ToString::to_string));
    }

    // Don't do anything if command list is empty
    if args.commands.len() == 0 {
        return;
    }

    let log_dir = args
        .log_dir
        .unwrap_or_else(|| tempfile::tempdir().unwrap().into_path());
    std::fs::create_dir_all(&log_dir).unwrap();

    // Calculate the character-width of the largest command index.
    // This is used  to align the log file names so they would be sortable by
    // command order.
    let width = (args.commands.len() as f32).log10() as usize + 1;

    eprintln!("Writing standard outputs to {log_dir:?}");

    let mut failed_tasks = Vec::new();

    let jobs = args
        .jobs
        .or_else(|| std::thread::available_parallelism().ok())
        .map(NonZeroUsize::get)
        .unwrap_or(12);
    let concurrent_jobs = Arc::new(Semaphore::new(jobs));

    // Convert the collected commands into async join-handles
    let mut tasks = Vec::new();
    for (i, cmd) in args.commands.into_iter().enumerate() {
        // Wait for a permit to be acquired before starting.
        let permit = concurrent_jobs.clone().acquire_owned().await.unwrap();

        // Cloning the styles since they're consumed by-move by every bar.
        let styles = styles.clone();

        let pb = multi_progress_bar.as_ref().map(|multi_progress_bar| {
            let pb = multi_progress_bar.add(ProgressBar::new_spinner());
            pb.set_style(styles.progress);
            pb.enable_steady_tick(Duration::from_millis(80));
            pb.set_message(cmd.clone());
            pb.set_prefix(i.to_string());
            pb
        });

        let mut proc =
            match spawn_task_process(&log_dir, &format!("{i:0width$}"), &args.shell, &cmd) {
                Ok(proc) => proc,
                Err(_) => {
                    pb.map(|pb| {
                        pb.set_style(styles.fail);
                        pb.finish();
                    });
                    failed_tasks.push(i);
                    continue;
                }
            };

        let handle = tokio::spawn(async move {
            let res = proc.wait().await.unwrap();
            pb.map(|pb| {
                if res.success() {
                    pb.set_style(styles.done);
                } else {
                    pb.set_style(styles.fail);
                }
                pb.finish();
            });
            drop(permit);
            // Report the process exit code as task output
            res.success()
        });
        tasks.push((i, handle));
    }

    // Await the tasks and record failures
    for (task_index, handle) in tasks {
        let result = handle.await;
        match result {
            Err(join_err) => {
                eprintln!("Failed joining task {task_index}: {join_err:?}");
            }
            Ok(false) => {
                failed_tasks.push(task_index);
            }
            _ => {}
        }
    }

    let exit_code = if failed_tasks.len() > 0 {
        eprintln!("The following tasks failed: {:?}", failed_tasks);
        eprintln!("You can view their output in {log_dir:?}");
        1
    } else {
        0
    };
    std::process::exit(exit_code);
}
