//! asb: Apple Container のサンドボックスを、アイドル時間に応じて
//! hot → stopped → archived に降格し、exec を合図に復元する。

mod admission;
mod client;
mod config;
mod daemon;
mod lifecycle;
mod model;
mod proc;
mod protocol;
mod reconcile;
mod runtime;
mod runtime_cli;
#[cfg(test)]
mod runtime_fake;
mod store;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "asb", version, about = "Idle-tiering sandboxes on Apple Container (hot / stopped / archived)")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// デーモンを起動する(フォアグラウンド)。
    Daemon,
    /// サンドボックスを作って起動する。
    Create {
        name: String,
        #[arg(long)]
        image: String,
        #[arg(long)]
        cpus: Option<u32>,
        /// メモリ量(MB)。既定 1024。
        #[arg(long)]
        memory: Option<u64>,
        /// インターネットへの外向き通信を許可する(既定は不可)。
        #[arg(long)]
        internet: bool,
    },
    /// サンドボックスでコマンドを実行する。必要なら復元してから実行する。
    Exec {
        name: String,
        #[arg(short = 'i', long)]
        interactive: bool,
        #[arg(short = 't', long)]
        tty: bool,
        #[arg(short = 'w', long = "workdir")]
        workdir: Option<String>,
        #[arg(short = 'e', long = "env")]
        env: Vec<String>,
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// ホストとサンドボックスの間でファイルをコピーする(`<sandbox>:<path>`)。
    Cp { src: String, dst: String },
    /// サンドボックスの一覧。
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// サンドボックスと退避イメージを削除する。
    Rm { name: String },
}

fn main() {
    let cli = Cli::parse();
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("asb: config error: {e:#}");
            std::process::exit(client::EXIT_FAILURE);
        }
    };
    let result: Result<i32, client::CliError> = match cli.command {
        Cmd::Daemon => daemon::run(cfg).map(|_| 0).map_err(|e| client::CliError {
            exit: client::EXIT_FAILURE,
            message: format!("asb daemon: {e:#}"),
        }),
        Cmd::Create { name, image, cpus, memory, internet } => {
            client::create(&cfg, &name, &image, cpus, memory, internet).map(|_| 0)
        }
        Cmd::Exec { name, interactive, tty, workdir, env, cmd } => {
            client::exec(&cfg, &name, &client::ExecOptions { interactive, tty, workdir, env }, &cmd)
        }
        Cmd::Cp { src, dst } => client::cp(&cfg, &src, &dst),
        Cmd::Ls { json } => client::list(&cfg).map(|v| {
            client::print_list(&v, json);
            0
        }),
        Cmd::Rm { name } => client::remove(&cfg, &name).map(|_| 0),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("{}", e.message);
            std::process::exit(e.exit);
        }
    }
}
