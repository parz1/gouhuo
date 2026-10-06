// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    println!("cargo:rerun-if-env-changed=GOUHUO_UPDATE_BASE_URL");
    // fluent-dark 只影响 std-widgets 里那几个控件（这里用到的是 ScrollView）。
    // 别的都是自己画的，见 ui/theme.slint。
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("编译 .slint 失败");

    #[cfg(windows)]
    windows_resources();
}

/// 把图标和版本信息编进 exe。
///
/// 不编的话，资源管理器、开始菜单快捷方式、「添加或删除程序」里显示的都是系统默认的
/// 空白图标 —— 窗口标题栏上的火苗是运行时 Slint 设的，那只管窗口本身。
///
/// 资源脚本在这里现生成：版本号跟着 Cargo.toml 走，不在两个地方各写一份。
#[cfg(windows)]
fn windows_resources() {
    use std::path::PathBuf;

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo 会设"));
    let icon = manifest.join("ui").join("icons").join("gouhuo.ico");
    println!("cargo:rerun-if-changed={}", icon.display());

    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo 会设");
    let mut parts = version
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );

    // rc.exe 默认按系统代码页读文件，中文要靠这一行按 UTF-8 读。
    // 常量写成数字，免得去 #include SDK 的头文件（那要 rc.exe 知道 SDK 的 include 路径）：
    //   0x40004 = VOS_NT_WINDOWS32，0x1 = VFT_APP，080404b0 = 简体中文 + Unicode
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080404b0"
    BEGIN
      VALUE "FileDescription", "篝火"
      VALUE "ProductName", "篝火"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "gouhuo.exe"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0804, 1200
  END
END
"#,
        // rc.exe 认正斜杠，省得操心字符串里的反斜杠转义。
        icon = icon.display().to_string().replace('\\', "/"),
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo 会设")).join("gouhuo.rc");
    std::fs::write(&out, rc).expect("写不了资源脚本");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_required()
        .expect("编不了 Windows 资源（要装 Windows SDK，VS BuildTools 带着）");
}
