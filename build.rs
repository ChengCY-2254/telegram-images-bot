use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn main() {
    let version = get_git_version();
    let mut f =
        File::create(Path::new(&std::env::var("OUT_DIR").unwrap()).join("VERSION")).unwrap();
    f.write_all(version.trim().as_bytes()).unwrap()
}

// 定义一个函数，用于获取git版本号
fn get_git_version() -> String {
    // 获取Cargo包的版本号
    let version = env!("CARGO_PKG_VERSION").to_string();

    // 执行git命令，获取git描述信息
    let child = Command::new("git").args(["describe", "--always"]).output();

    // 匹配git命令的执行结果
    match child {
        // 如果执行成功
        Ok(child) => {
            // 将git描述信息转换为字符串
            let buf = String::from_utf8(child.stdout).expect("failed to read stdout");
            // 将Cargo包的版本号和git描述信息拼接起来
            version + "-" + &buf
        }
        // 如果执行失败
        Err(why) => {
            // 打印错误信息
            eprintln!("`git describe` err: {}", why);
            // 返回Cargo包的版本号
            version
        }
    }
}