//! Small OS helpers: notifications, shells, window focus, autostart, setup and doctor.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

/// Run a command line through the platform shell.
pub fn shell(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        no_console(&mut c);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Windows: start a console program without flashing a console window.
#[allow(unused_variables)]
pub fn no_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|d| d.join(program).is_file() || d.join(format!("{program}.exe")).is_file())
}

/// Best effort desktop notification. Never blocks, never fails loudly.
#[allow(unused_variables)]
pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "linux")]
    {
        if which("notify-send") {
            let _ = Command::new("notify-send")
                .args([
                    "-a",
                    "hotbar",
                    "-t",
                    "1500",
                    "-h",
                    "string:x-canonical-private-synchronous:hotbar",
                ])
                .arg(title)
                .arg(body)
                .spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(body),
            esc(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).spawn();
    }
    #[cfg(windows)]
    {
        // No toast without an installed app identity; the log keeps a record.
        crate::log!("{title}: {body}");
    }
}

/// Handle of the focused window (Windows), so the picker can hand focus back.
pub fn foreground_window() -> isize {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as isize
    }
    #[cfg(not(windows))]
    0
}

#[allow(unused_variables)]
pub fn focus_window(hwnd: isize) {
    #[cfg(windows)]
    unsafe {
        if hwnd != 0 {
            windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd as _);
        }
    }
}

/// Windows: pull a window to the front even when another app has focus.
#[allow(unused_variables)]
pub fn force_foreground(title: &str) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, FindWindowW, GetForegroundWindow, GetWindowThreadProcessId,
            SetForegroundWindow,
        };
        let t: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let hwnd = FindWindowW(std::ptr::null(), t.as_ptr());
        if hwnd.is_null() {
            return;
        }
        let fg = GetForegroundWindow();
        if fg == hwnd {
            return;
        }
        let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        let me = GetCurrentThreadId();
        AttachThreadInput(me, fg_thread, 1);
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        AttachThreadInput(me, fg_thread, 0);
    }
}

fn exe() -> Result<PathBuf> {
    std::env::current_exe().context("could not find the hotbar executable")
}

pub fn autostart(on: bool) -> Result<String> {
    #[cfg(windows)]
    {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        if on {
            let mut path = exe()?;
            let quiet = path.with_file_name("hotbarw.exe");
            if quiet.is_file() {
                path = quiet;
            }
            let value = format!("\"{}\" daemon", path.display());
            let ok = Command::new("reg")
                .args([
                    "add", key, "/v", "hotbar", "/t", "REG_SZ", "/d", &value, "/f",
                ])
                .status()?
                .success();
            if !ok {
                anyhow::bail!("reg add failed");
            }
            Ok(format!("hotbar will start at login ({value})"))
        } else {
            let _ = Command::new("reg")
                .args(["delete", key, "/v", "hotbar", "/f"])
                .status();
            Ok("hotbar will no longer start at login".into())
        }
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".config"));
        let unit_dir = base.join("systemd").join("user");
        let unit = unit_dir.join("hotbar.service");
        if on {
            std::fs::create_dir_all(&unit_dir)?;
            let text = include_str!("../packaging/hotbar.service").replace(
                "ExecStart=hotbar daemon",
                &format!("ExecStart={} daemon", exe()?.display()),
            );
            std::fs::write(&unit, text)?;
            let ok = Command::new("systemctl")
                .args(["--user", "enable", "--now", "hotbar.service"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                Ok(format!("enabled {}", unit.display()))
            } else {
                Ok(format!(
                    "wrote {}, but systemctl failed. Without systemd, add `exec-once = hotbar daemon` (Hyprland) or `exec hotbar daemon` (sway, i3) to your config.",
                    unit.display()
                ))
            }
        } else {
            let _ = Command::new("systemctl")
                .args(["--user", "disable", "--now", "hotbar.service"])
                .status();
            let _ = std::fs::remove_file(&unit);
            Ok("autostart removed".into())
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let dir = PathBuf::from(home).join("Library").join("LaunchAgents");
        let plist = dir.join("dev.hotbar.plist");
        if on {
            std::fs::create_dir_all(&dir)?;
            let text = include_str!("../packaging/dev.hotbar.plist")
                .replace("/usr/local/bin/hotbar", &exe()?.display().to_string());
            std::fs::write(&plist, text)?;
            let _ = Command::new("launchctl")
                .arg("load")
                .arg("-w")
                .arg(&plist)
                .status();
            Ok(format!("installed {}", plist.display()))
        } else {
            let _ = Command::new("launchctl")
                .arg("unload")
                .arg("-w")
                .arg(&plist)
                .status();
            let _ = std::fs::remove_file(&plist);
            Ok("autostart removed".into())
        }
    }
}

/// One time setup for key chords.
pub fn setup(yes: bool) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = yes;
        println!(
            "Nothing to set up on Windows. Run `hotbar autostart on` to start hotbar at login."
        );
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = yes;
        println!("hotbar needs two permissions in System Settings > Privacy & Security:");
        println!("  Accessibility      (to send the paste keystroke)");
        println!("  Input Monitoring   (to see Cmd+V followed by a digit)");
        println!("Add the program that runs hotbar (your terminal, or the hotbar binary itself).");
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent")
            .status();
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let user = std::env::var("USER").context("USER is not set")?;
        let rule = include_str!("../packaging/99-hotbar-uinput.rules");
        let steps: Vec<(String, Vec<String>)> = vec![
            (
                "add you to the input group".into(),
                vec!["usermod".into(), "-aG".into(), "input".into(), user],
            ),
            (
                "allow the input group to use /dev/uinput".into(),
                vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "printf '%s' '{}' > /etc/udev/rules.d/99-hotbar-uinput.rules",
                        rule.trim_end()
                    ),
                ],
            ),
            (
                "load uinput at boot".into(),
                vec![
                    "sh".into(),
                    "-c".into(),
                    "echo uinput > /etc/modules-load.d/hotbar.conf".into(),
                ],
            ),
            (
                "load uinput now".into(),
                vec!["modprobe".into(), "uinput".into()],
            ),
            (
                "apply the udev rule".into(),
                vec![
                    "sh".into(),
                    "-c".into(),
                    "udevadm control --reload-rules && udevadm trigger".into(),
                ],
            ),
        ];
        println!("hotbar reads the keyboard through /dev/input and types through /dev/uinput.");
        println!("These commands need root:\n");
        for (why, cmd) in &steps {
            println!("  sudo {}    # {why}", cmd.join(" "));
        }
        if !yes {
            print!("\nRun them now with sudo? [y/N] ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !answer.trim().eq_ignore_ascii_case("y") {
                println!("Nothing changed.");
                return Ok(());
            }
        }
        for (why, cmd) in &steps {
            let ok = Command::new("sudo")
                .args(cmd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                anyhow::bail!("failed to {why}");
            }
        }
        println!("\nDone. Log out and back in so the new group applies, then run `hotbar doctor`.");
        Ok(())
    }
}

/// Print what works and what does not.
pub fn doctor() -> Result<()> {
    let ok = |b: bool| if b { "ok  " } else { "FAIL" };
    println!("hotbar {}", crate::VERSION);
    println!("config      {}", crate::config::config_path().display());
    println!("data        {}", crate::config::data_dir().display());
    match crate::config::Config::load() {
        Ok(_) => println!("[ok  ] config parses"),
        Err(e) => println!("[FAIL] config: {e:#}"),
    }
    let running = crate::ipc::daemon_running();
    println!("[{}] daemon running", if running { "ok  " } else { "no  " });

    #[cfg(target_os = "linux")]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        println!(
            "session     {} {}",
            if wayland {
                "wayland"
            } else if x11 {
                "x11"
            } else {
                "none"
            },
            desktop
        );
        if wayland {
            let dc = crate::clip::wayland_data_control_available();
            println!("[{}] wayland data-control protocol", ok(dc));
            if !dc {
                println!("       Your compositor does not offer data-control (GNOME/Mutter is the usual one).");
                println!(
                    "       hotbar will use XWayland, which only sees copies made in X11 apps."
                );
            }
        }
        let uinput = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_ok();
        println!("[{}] /dev/uinput writable", ok(uinput));
        let kbds = crate::input::linux::keyboards().len();
        println!("[{}] keyboards readable: {kbds}", ok(kbds > 0));
        if !uinput || kbds == 0 {
            println!("       Run `hotbar setup`, then log out and back in.");
        }
        println!("[{}] notify-send (optional)", ok(which("notify-send")));
    }
    #[cfg(target_os = "macos")]
    {
        let (ax, listen) = crate::input::macos_permissions();
        println!("[{}] Accessibility permission", ok(ax));
        println!("[{}] Input Monitoring permission", ok(listen));
        if !(ax && listen) {
            println!("       Run `hotbar setup` to open the right System Settings pages.");
        }
    }
    #[cfg(windows)]
    {
        let _ = ok;
        println!("[ok  ] Windows needs no extra permissions");
        println!("       Win+V opens hotbar's picker while the daemon runs (set keys.picker = \"\" to keep the Windows one)");
    }
    Ok(())
}
