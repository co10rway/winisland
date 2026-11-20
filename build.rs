//! 构建脚本：把应用图标（assets/icon.ico）编译成 COFF 资源对象并链接进 exe。
//!
//! 交叉编译到 x86_64-pc-windows-gnu 时用 zig 自带的资源编译器（resinator）替代 windres：
//!   zig rc /:output-format coff /:target x86_64 app.rc app_res.o
//! 再通过 cargo:rustc-link-arg 把这个 .rsrc 对象交给 lld，exe 便带上程序图标
//! （资源管理器、任务栏、ALT-TAB、任务管理器显示的都是它）。
//!
//! 只在 windows 目标下执行；本地非 windows 的 cargo check 会跳过。

use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico = manifest.join("assets").join("icon.ico");
    assert!(ico.exists(), "missing assets/icon.ico");

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    // ICON 资源 ID = 1：Windows 取最小 ID 的图标组作为程序图标。
    // 用绝对路径引用 ico，避免 rc 编译器 CWD 差异导致找不到文件。
    let ico_path = ico.to_string_lossy().replace('\\', "/");
    let rc_src = format!("1 ICON \"{}\"\n", ico_path);
    let rc_path = out.join("app.rc");
    fs::write(&rc_path, &rc_src).expect("write app.rc");
    println!("cargo:rerun-if-changed={}", rc_path.display());

    let obj = out.join("app_res.o");
    let zig = find_zig();
    let status = Command::new(&zig)
        .arg("rc")
        .args(["/:output-format", "coff", "/:target", "x86_64"])
        .arg(&rc_path)
        .arg(&obj)
        .status()
        .unwrap_or_else(|e| panic!("failed to run zig rc at {}: {e}", zig.display()));
    assert!(status.success(), "zig rc failed to compile app.rc");
    assert!(obj.exists(), "zig rc did not produce {}", obj.display());

    println!("cargo:rustc-link-arg={}", obj.display());
}

/// 定位 zig：优先 ZIG 环境变量，其次 ~/zig/zig（本机安装位置），最后 PATH。
fn find_zig() -> PathBuf {
    if let Ok(p) = env::var("ZIG") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    if let Ok(home) = env::var("HOME") {
        let pb = PathBuf::from(home).join("zig").join("zig");
        if pb.exists() {
            return pb;
        }
    }
    PathBuf::from("zig")
}
