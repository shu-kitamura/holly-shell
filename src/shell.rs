// use crate::helper::DynError;
use nix::{
    libc,
    sys::{
        signal::{killpg, signal, SigHandler, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{self, dup2, execvp, fork, pipe, setpgid, tcgetpgrp, tcsetpgrp, ForkResult, Pid},
};
use rustyline::{error::ReadlineError, Editor};
use signal_hook::{consts::*, iterator::Signals};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::CString,
    mem::replace,
    path::PathBuf,
    process::exit,
    sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender},
    thread,
};


/// システムコール呼び出しの wrapper 関数。
fn syscall<F, T>(f: F) -> Result<T, nix::Error>
where
    F: Fn() -> Result<T, nix::Error>,
{
    loop {
        match f() {
            Err(nix::Error::EINTR) => (), // EINTER の場合、リトライする
            result => return result,
        }
    }
}

/// worker スレッドが受信するメッセージ。
enum WorkerMsg {
    Signal(i32), // シグナルを受信。
    Cmd(String), // コマンド入力。
}

/// main スレッドが受信するメッセージ。
enum MainMsg {
    Continue(i32), // シェルの読み込みを再開する。(引数は最後の終了コード) 
    Quit(i32),     // シェルを終了する。(引数はシェルの終了コード)
}

/// HollyShell 型
#[derive(Debug)]
pub struct HollyShell {
    history_file: String,
}

impl HollyShell {
    pub fn new(history_file: &str) -> Self {
        HollyShell{ history_file: history_file.to_string() }
    }

    /// main スレッド
    pub fn run(&self) -> Result<(), DynError> {
        // 標準出力への書き込み時に SIGSTP が配送され、シェルが停止してしまうため、
        // SIGTTOU を無視に設定し、シェルが停止しないようにする。
        unsafe { signal(Signal::SIGTTOU, SigHandler::SigIgn).unwrap() };

        // rustyline の Editor を使用する。
        // 標準入力からの読み込みが容易、矢印キーを使った操作をサポートできるなどのメリットがある。
        let mut rl = Editor::<()>::new()?;

        // ヒストリファイルを読み込む
        if let Err(e) = rl.load_histlry(&self.history_file) {
            eprintln!("ERROR(HollyShell): Failed to load history file.")
        }

        // channel を生成し、signal_handler, worker スレッドを生成。
        let (worker_tx, worker_rx) = channel();
        let (shell_tx, shell_rx) = sync_channel(0);
        spawn_sig_handler(worker_tx.clone())?;
        Worker::new().spawn(worker_rx, shell_tx);

        let exit_value;   // 終了コード
        let mut prev = 0; // 直前の終了コード

        loop {
            let face = if prev == 0 {'\u{1F642}'} else { '\u{1F480}' };
            // 入力から1行読み込む。
            match rl.readline(&format!("HollyShell {face} %> ")) {
                Ok(line) => {
                    let line_trimed = line.trim(); // 行頭・行末の空白を削除する。
                    if line_trimed.is_empty() {
                        continue; // 空のコマンドの場合、下の処理を飛ばして、再読み込みする。
                    } else {
                        rl.add_history_entry(line_trimed) // ヒストリファイルに追加する。
                    }

                    // worker スレッドに送信
                    worker_tx.send(WorkerMsg::Cmd(line)).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Continue(n) => prev = n, // 読み込みを再開する。
                        ShellMsg::Quit(n) => {             // シェルを終了する。
                            exit_value = n;
                            break;
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => eprintln!("HollyShell : To exit shell, enter Ctrl+d"),
                Err(ReadlineError::Eof) => {
                    worker_tx.send(WorkerMsg::Cmd("exit".to_string())).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Quit(n) => {
                            exit_value = n;
                            break;
                        }
                        _ => panic!("Exitに失敗")
                    }
                }
                Err(e) => {
                    eprintln!("ERROR(HollyShell): The following error occurred on loading.\n{e}");
                    exit_value = 1;
                    break;
                }
            }
        }
        if let Err(e) = rl.save_history(&self.history_file) {
            eprintln!("ERROR(HollyShell): Failed to write history file.")
        }
        exit(exit_value);
    }

    fn spawn_sig_handler(tx: Sender<WorkerMsg>) -> Result<(), DynError> -> {
        let mut signals = Signals::new(&[SIGINT, SIGTSTP, SIGCHLD])?;
        thread::spawn(move || {
            for sig in signals.forever() {
                // シグナルを受信して、worker スレッドに転送する。
                tx.send(WorkerMsg::Signal(sig)).unwrap();
            }
        });
        Ok(())
    }
}

/// プロセスの実行状態を表す型。
#[derive(Debug, PartialEq, Clone)]
enum ProcState {
    Run,  // 実行中
    Stop, // 停止中
}

/// プロセスの情報管理用の型。
#[derive(Debug, Clone)]
struct ProcInfo {
    state: ProcState, // 実行状態
    pgid: Pid,        // プロセスグループID
}


/// worker スレッド用の型
struct Worker {
    exit_value: i32, // 終了コード
    fg: Option<Pid>, // フォアグラウンドのプロセスグループID
    jobs: BTreeMap<usize, (Pid, String)>, // ジョブIDから (プロセスグループID, 実行コマンド) へのマッピング
    pgid_to_pids: HashMap<Pid, (usize, HashSet<Pid>)>, // プロセスグループIDから (ジョブID, プロセスID) へのマッピング
    pid_to_info: HashMap<Pid, ProcInfo>, // プロセスグループIDからプロセスグループIDへのマッピング
    shell_pgid: Pid, // シェルのプロセスグループID
}

impl Worker {
    fn new() -> Self {
        Worker {
            exit_value: 0,
            fn: None,
            jobs: BTreeMap::new(),
            pgid_to_pids: HashMap::new(),
            pid_to_info: HashMap::new(),
            shell_pgid: tcgetpgrp(libc::STDIN_FILENO).unwrap(),
        }
    }

    fn spawn(mut self, worker_rx: Receiver<WorkerMsg>, shell_tx: SyncSender<ShellMsg>) {
        thread::spawn(move || {
            for msg in worker_rx.iter() { // worker_rx からメッセージを受信する。
                match msg {
                    WorkerMsg::Cmd(line) => {
                        match parse_cmd(&line) { // コマンドラインの入力をパースする。
                            Ok(cmd) => {
                                if self.built_in_cmd(&cmd, &shell_tx) { // 組み込みコマンドの場合、built_in_cmd を実行し、コマンドを実行。
                                    continue;
                                }

                                if !self.spawn_child(&line, &cmd) { // 外部コマンドの場合、子プロセスを生成し、コマンドを実行。
                                    shell_tx.send(ShellMsg::Continue(self.exit_value)).unwrap();
                                }
                            }
                            Err(e) => {
                                eprintln!("ERROR(HollyShell): {e}");
                                shell_tx.send(ShellMsg::Continue(self.exit_value)).unwrap(); // コマンドのパースに失敗した場合、入力待ちを再開する。
                            }
                        }
                    }
                    WorkerMsg::Signal(SIGCHLD) => {
                        self.wait_child(&shell_tx); // 子プロセスの状態変化を管理する。
                    }
                    _ => (),
                }
            }
        });
    }

    fn built_in_cmd(&mut self, cmd: &[(&str, Vec<&str>)], shell_tx: &SyncSender<ShellMsg>) -> bool {
        if cmd.len() > 1 {
            return false; // 組み込みコマンドはパイプ非対応のため、false を返す。
        }

        match cmd[0].0 {
            "exit" => self.run_exit(&cmd[0].1, shell_tx),
            "jobs" => self.run_jobs(shell_tx),
            "fg" => self.run_fg(&cmd[0].1, shell_tx),
            "cd" => self.run_fg(&cmd[0].1, shell_tx),
            _ => false,
        }
    }

    fn run_exit(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        // 実行中のジョブがある場合は終了しない。
        if !self.job.is_empty() {
            eprintln!("HollyShell can't be ended because the job is currently running");
            self.exit_value = 1;
            shell_tx.seld(ShellMsg::Continue(self.exit_value)).unwrap();
            return true;
        }

        let exit_value = if let Some(s) = args.get(1) {
            if let Ok(n) = (*s).parse::<i32>() {
                n
            } else {
                eprintln!("{s} is invalied argment");
                self.exit_value = 1;
                shell_tx.send(ShellMsg::Continue(self.exit_value)).unwrap();
                return true;
            }
        } else {
            self.exit_value
        };

        shell_tx.send(ShellMsg::Quit(exit_value)).unwrap(); // 終了
        true
    }

    fn run_fg(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        self.exit_value = 1;

        if args.len() < 2 {
            eprintln!("Usage: fg <数字>");
            shell_tx.send(ShellMsg::Continue(self.exit_value)).unwrap();
            return true;
        }

        if let Ok(n) = args[1].parse::<usize>() {
            if let Some((pgid, cmd)) = self.jobs.get(&n) {
                eprintln!("[{n}] 再開 \t {cmd}");
                self.fg = Some(*pgid);
                tcsetpgrp(libc::STDIN_FILENO, *pgid).unwrap();

                killpg(*pgid, Signal::SIGCONT).unwrap();
                return true;
            }
        }

        eprintln!("ERROR(HollyShell): The job '{}' is not found.", args[1]);
        shell_tx.send(ShellMsg::Continue(self.exit_value)).unwrap();
        true
    }
}

/// ドロップ時にクロージャ f を呼び出す型。
struct CleanUp<F>
where
    F: Fn(),
{
    f: F,
}

impl<F> Drop for CleanUp<F>
where
    F: Fn(),
{
    fn drop(&mut self) {
        (self.f)()
    }
}
