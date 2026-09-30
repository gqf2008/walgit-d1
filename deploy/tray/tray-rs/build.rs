//! 把产品图标编进 Windows 二进制(deploy/windows/walgit.rc)。
//!
//! Windows 的 shell 从**可执行文件自己的 PE 资源**取图标:安装器建的快捷方式
//! `IconLocation` 是 `,0`(第一个图标资源),exe 里没有资源就只能画系统默认占位图
//! (线程 win-tray-no-embedded-icon)。macOS 的图标在 .app bundle 里
//! (deploy/tray/macos/build-dmg.sh),Linux 在 .desktop 里,所以这里只对 Windows
//! target 生效,其它平台直接返回。
//!
//! 用 winresource 只为「找到并调用资源编译器」这一件事(MSVC 用 Windows SDK 的
//! `rc.exe`,GNU 用 `windres`);`.rc` 是我们自己的(deploy/windows/walgit.rc),
//! 不用它生成的那份——生成的总会带一个由 `CARGO_PKG_VERSION` 拼出来的 VERSIONINFO,
//! 而托盘的产品版本是运行时事实(见 .rc 里的注释)。

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // 构建脚本跑在 host 上,目标平台只能从 cargo 给的环境变量读(不是 cfg!)。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let windows = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../windows");
    let rc = windows.join("walgit.rc");
    for artifact in [&rc, &windows.join("walgit.ico")] {
        println!("cargo:rerun-if-changed={}", artifact.display());
    }

    // 资源编译器缺失是**硬失败**:静默产出一个没有图标的 exe,正是这条线程要修的
    // 那个 bug,而不是它可以接受的降级。
    winresource::WindowsResource::new()
        .set_resource_file(&rc.to_string_lossy())
        .compile()
        .expect(
            "compile deploy/windows/walgit.rc: needs rc.exe (Windows SDK) for MSVC \
             targets, windres for GNU targets",
        );
}
