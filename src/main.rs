use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

const KEY_LEN: usize = 32;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [flag] if flag == "--version" => {
            println!("wrapfile 0.1.0");
            ExitCode::SUCCESS
        }
        [cmd, path] if cmd == "keygen" && !path.starts_with('-') => {
            keygen(Path::new(path))
        }
        _ => {
            eprintln!("Usage: wrapfile --version");
            eprintln!("       wrapfile keygen <key-file>");
            ExitCode::from(2)
        }
    }
}

fn keygen(path: &Path) -> ExitCode {
    // 由操作系统密码学安全随机源填充密钥，不输出到任何日志或终端。
    let mut key = [0u8; KEY_LEN];
    if let Err(e) = getrandom::fill(&mut key) {
        eprintln!("wrapfile: keygen: 无法获取密码学安全随机数: {e}");
        return ExitCode::FAILURE;
    }

    // create_new 对应 O_CREAT|O_EXCL：目标（含悬空符号链接）已存在即失败，
    // 不会沿链接创建，也不会触碰已有对象。
    let mut open_options = OpenOptions::new();
    open_options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // 创建时即限定为仅当前用户可读写，不依赖 umask。
        open_options.mode(0o600);
    }

    let mut file = match open_options.open(path) {
        Ok(file) => file,
        Err(e) => {
            if e.kind() == io::ErrorKind::AlreadyExists {
                eprintln!(
                    "wrapfile: keygen: 目标 {} 已存在，拒绝覆盖",
                    path.display()
                );
            } else {
                eprintln!(
                    "wrapfile: keygen: 无法创建密钥文件 {}: {e}",
                    path.display()
                );
            }
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = file.write_all(&key).and_then(|()| file.sync_all()) {
        eprintln!(
            "wrapfile: keygen: 写入密钥文件 {} 失败: {e}",
            path.display()
        );
        // 文件由本次操作以独占方式创建，写入不完整时将其清除，
        // 避免留下残缺的密钥文件。
        drop(file);
        let _ = fs::remove_file(path);
        return ExitCode::FAILURE;
    }

    println!("密钥已生成并保存到 {}", path.display());
    ExitCode::SUCCESS
}
