//! WinIsland — Windows 灵动岛
//! 单一原生 exe，零运行时依赖，全部数据仅存本机。

#![cfg(windows)]
#![cfg_attr(windows, windows_subsystem = "windows")] // 纯 GUI，双击不弹黑色控制台窗口

mod anim;
mod app;
mod config;
mod crypto;
mod modal;
mod modules;
mod render;
mod settings;
mod store;
mod theme;
mod ui;

fn main() {
    app::run();
}
