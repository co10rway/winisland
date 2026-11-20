//! 构建脚本：把应用图标（assets/icon.ico）编译成 COFF 资源对象并链接进 exe。
//!
//! 两条路径：
//!   * msvc（Windows 本机）：用 Windows SDK 的 rc.exe 编出 .res，再用 MSVC 的 cvtres
//!     转成 COFF 对象 → cargo:rustc-link-arg 交给 link.exe。
//!   * gnu（Linux 交叉编译）：用 zig 自带的资源编译器（resinator）替代 windres：
//!       zig rc /:output-format coff /:target x86_64 app.rc app_res.o
//! 资源链接后，资源管理器、任务栏、ALT-TAB、任务管理器显示的都是这个图标。
//!
//! 找不到工具链时只打印警告并跳过图标，不让构建失败（图标是装饰，不该挡住编译）。
//! 只在 windows 目标下执行；本地非 windows 的 cargo check 会跳过。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

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

    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if msvc {
        if !build_msvc(&rc_path, &obj, &out) {
            println!("cargo:warning=未找到 rc.exe/cvtres.exe，跳过程序图标嵌入");
            return;
        }
    } else {
        let zig = find_zig();
        let status = Command::new(&zig)
            .arg("rc")
            .args(["/:output-format", "coff", "/:target", "x86_64"])
            .arg(&rc_path)
            .arg(&obj)
            .status()
            .unwrap_or_else(|e| panic!("failed to run zig rc at {}: {e}", zig.display()));
        assert!(status.success(), "zig rc failed to compile app.rc");
    }

    assert!(obj.exists(), "resource object missing: {}", obj.display());
    println!("cargo:rustc-link-arg={}", obj.display());
}

/// msvc 路径：rc.exe 编 .res → cvtres 转 COFF 对象。成功返回 true。
fn build_msvc(rc_path: &Path, obj: &Path, out: &Path) -> bool {
    let (Some(rc), Some(cvtres)) = (find_rc(), find_cvtres()) else {
        return false;
    };
    let res = out.join("app.res");

    let status = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res.display()))
        .arg(rc_path)
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            println!("cargo:warning=rc.exe 编译 app.rc 失败");
            return false;
        }
    }

    let machine = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let status = Command::new(&cvtres)
        .arg(format!("/machine:{machine}"))
        .arg("/nologo")
        .arg(format!("/out:{}", obj.display()))
        .arg(&res)
        .status();
    match status {
        Ok(s) if s.success() => true,
        _ => {
            println!("cargo:warning=cvtres 转换 app.res 失败");
            false
        }
    }
}

fn exists_file(p: impl AsRef<Path>) -> Option<PathBuf> {
    let pb = p.as_ref().to_path_buf();
    if pb.is_file() {
        Some(pb)
    } else {
        None
    }
}

/// 定位 zig：优先 ZIG 环境变量，其次 ~/zig/zig（本机安装位置），最后 PATH。
fn find_zig() -> PathBuf {
    if let Ok(p) = env::var("ZIG") {
        if let Some(pb) = exists_file(&p) {
            return pb;
        }
    }
    if let Ok(home) = env::var("HOME") {
        if let Some(pb) = exists_file(PathBuf::from(&home).join("zig").join("zig")) {
            return pb;
        }
    }
    PathBuf::from("zig")
}

/// 定位 rc.exe：优先 RC 环境变量，其次 Windows Kits 各版本目录（新的优先），最后 PATH。
fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = env::var("RC") {
        if let Some(pb) = exists_file(&p) {
            return Some(pb);
        }
    }
    let mut kits: Vec<PathBuf> = Vec::new();
    for base in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        if let Ok(rd) = fs::read_dir(base) {
            for e in rd.flatten() {
                let p = e.path().join("x64").join("rc.exe");
                if p.is_file() {
                    kits.push(p);
                }
            }
        }
    }
    // 目录名是 10.0.26100.0 这种，按字符串倒序 ≈ 版本从新到旧
    kits.sort();
    kits.pop().or_else(|| exists_file("rc.exe"))
}

/// 定位 cvtres.exe：优先 CVTRES 环境变量，其次 VS 各实例 MSVC 目录，最后 PATH。
fn find_cvtres() -> Option<PathBuf> {
    if let Ok(p) = env::var("CVTRES") {
        if let Some(pb) = exists_file(&p) {
            return Some(pb);
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for base in [
        r"C:\Program Files\Microsoft Visual Studio\2022",
        r"C:\Program Files (x86)\Microsoft Visual Studio\2022",
        r"C:\Program Files\Microsoft Visual Studio\2019",
        r"C:\Program Files (x86)\Microsoft Visual Studio\2019",
    ] {
        let Ok(ed) = fs::read_dir(base) else { continue };
        for inst in ed.flatten() {
            let msvc = inst.path().join("VC").join("Tools").join("MSVC");
            let Ok(rd) = fs::read_dir(msvc) else { continue };
            for ver in rd.flatten() {
                let p = ver
                    .path()
                    .join("bin")
                    .join("Hostx64")
                    .join("x64")
                    .join("cvtres.exe");
                if p.is_file() {
                    found.push(p);
                }
            }
        }
    }
    found.sort();
    found.pop().or_else(|| exists_file("cvtres.exe"))
}
