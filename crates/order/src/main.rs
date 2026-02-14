use rander::{ratatui, tui::OrderTui};

fn main() -> anyhow::Result<()> {
    configure_console_encoding_best_effort();

    let mut tui = OrderTui::default();
    ratatui::run(|terminal| tui.run(terminal))?;
    Ok(())
}

/// 启动阶段优先修正 Windows 控制台编码，避免中文输入输出乱码。
fn configure_console_encoding_best_effort() {
    #[cfg(windows)]
    {
        if let Err(error) = windows_console::configure_utf8_console() {
            eprintln!("Windows 控制台 UTF-8 初始化失败：{error}");
            windows_console::print_fallback_tips();
        }
    }
}

#[cfg(windows)]
mod windows_console {
    use std::env;

    use anyhow::anyhow;
    use windows_sys::Win32::System::Console::{
        GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
    };

    const UTF8_CODE_PAGE: u32 = 65001;

    /// 将控制台输入/输出 code page 同时切换到 UTF-8（65001）。
    ///
    /// 这里选择在入口尽早处理，原因是后续 TUI 渲染与用户输入都依赖同一控制台会话，
    /// 若只修输出不修输入，中文输入仍可能在特定 shell 里出现异常。
    pub fn configure_utf8_console() -> anyhow::Result<()> {
        let before_input = unsafe { GetConsoleCP() };
        let before_output = unsafe { GetConsoleOutputCP() };

        if before_input != UTF8_CODE_PAGE {
            let ok = unsafe { SetConsoleCP(UTF8_CODE_PAGE) };
            if ok == 0 {
                return Err(anyhow!(
                    "设置控制台输入编码失败（from={} to={}）",
                    before_input,
                    UTF8_CODE_PAGE
                ));
            }
        }

        if before_output != UTF8_CODE_PAGE {
            let ok = unsafe { SetConsoleOutputCP(UTF8_CODE_PAGE) };
            if ok == 0 {
                return Err(anyhow!(
                    "设置控制台输出编码失败（from={} to={}）",
                    before_output,
                    UTF8_CODE_PAGE
                ));
            }
        }

        let after_input = unsafe { GetConsoleCP() };
        let after_output = unsafe { GetConsoleOutputCP() };
        if after_input != UTF8_CODE_PAGE || after_output != UTF8_CODE_PAGE {
            return Err(anyhow!(
                "控制台编码未生效（input={} output={}）",
                after_input,
                after_output
            ));
        }

        // 只在发生过切换时提示，避免每次启动重复刷屏。
        if before_input != UTF8_CODE_PAGE || before_output != UTF8_CODE_PAGE {
            eprintln!(
                "已将 Windows 控制台编码切换为 UTF-8（input: {} -> {}, output: {} -> {}）。",
                before_input, after_input, before_output, after_output
            );
            print_shell_tips();
        }

        Ok(())
    }

    /// 提供 PowerShell / Windows Terminal 的兼容建议。
    ///
    /// 这些建议不影响程序主流程，但能减少“编码已切换仍显示方块字/乱码”的排障成本。
    fn print_shell_tips() {
        if env::var_os("WT_SESSION").is_some() {
            eprintln!(
                "Windows Terminal 提示：请使用支持中文的等宽字体（如 Cascadia Mono PL / Sarasa Mono SC）。"
            );
        }

        if env::var_os("PSModulePath").is_some() {
            eprintln!(
                "PowerShell 提示：可在配置文件加入 [Console]::InputEncoding=[Text.UTF8Encoding]::UTF8 与 [Console]::OutputEncoding=[Text.UTF8Encoding]::UTF8。"
            );
        }

        eprintln!("兼容提示：若手动设置了 LANG/LC_ALL，请确保其值包含 UTF-8。");
        eprintln!("兼容提示：仍有乱码时可先执行 chcp 65001，再重启当前终端会话。");
    }

    /// 当自动设置失败时，给出最小可执行排障路径。
    pub fn print_fallback_tips() {
        print_shell_tips();
    }
}
