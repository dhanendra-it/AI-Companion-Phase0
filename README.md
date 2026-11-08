# AI Companion — Phase 0

A small Windows 11 desktop prototype: a cute floating robot and a separate chat window. The chat is a local UI mockup only; it is not connected to an AI service.

## Scope and status

- **Included:** Tao native frameless transparent always-on-top robot window, click to open chat, Alt+Space to open/focus chat, Tao chat window containing a Wry/WebView2 view, and process-tree RAM measurement script.
- **Close behavior:** closing the chat window drops its Tao window and Wry WebView object; clicking the robot creates a fresh chat window.
- **Not implemented:** AI/API integration, persistence, streaming, voice, OCR, screen reading, packaging/installer, startup behavior, settings, and a global registered hotkey. Robot dragging is handled by the embedded view sending pointer deltas to the native window; it still needs Windows verification. Alt+Space is detected by polling key state, so it only works while this app's event loop is active and may conflict with Windows or another app.
- **Unverified:** This project was not built or run on Windows in this preparation. Build, window transparency, WebView2 lifetime, hotkey timing, and RAM measurements need verification on the target Windows 11 machine. No RAM values are asserted here.
- **Brief source:** The uploaded “AI Companion – Project Brief.pdf” was not available among the current project files or the referenced chat attachments. This ZIP follows the explicit Phase 0 requirements in the request and confirmed Alt+Space preference.

## Requirements

- Windows 11 x64
- Rust stable with the MSVC target and Visual Studio C++ Build Tools / Windows SDK
- Microsoft Edge WebView2 Runtime (Evergreen)
- Internet for the first Cargo dependency download

The referenced conversation reported Rust 1.99.1 (`x86_64-pc-windows-msvc`) and WebView2 Runtime 157.0.4341.42 on the user's machine. Those reports are context, not a verification of this ZIP.

## Build and run (PowerShell)

Extract the folder anywhere. In PowerShell, change to the extracted `AI-Companion-Phase0` directory, then run:

```powershell
cargo build --release
cargo run --release
```

The robot appears near the upper-left of the primary screen. Click it to open the chat. Close chat with its window close button; the chat window/WebView is dropped, and another robot click opens a new one. Alt+Space opens or focuses chat while the app is running. Close the robot window to exit.

If WebView2 cannot be created, confirm the Evergreen Runtime is installed and retry. Cargo's first build downloads Rust crates.

## Manual verification steps (not performed here)

1. Run the app and confirm the robot has no visible window rectangle, stays above ordinary windows, and can be dragged.
2. Click the robot; confirm one chat window opens. Close it, click the robot again, and confirm a fresh chat window opens.
3. With the app active, press Alt+Space and confirm chat opens/focuses. Check whether another application or Windows consumes this chord.
4. Measure the three states below. Keep the robot running throughout; allow the app to settle before each capture.

## RAM measurement (Windows PowerShell 7)

Open a second PowerShell window in this folder. The script samples the app and its discoverable descendant processes (including WebView2 children) and writes raw bytes and MiB to `ram-measurements.csv`; it prints average and peak working set and private bytes. Values are collected from the machine at run time and are not estimates.

1. With only the robot visible and no chat open:

   ```powershell
   .\scripts\measure-ram.ps1 -State robot-only
   ```

2. Open chat, wait about 10 seconds, then:

   ```powershell
   .\scripts\measure-ram.ps1 -State chat-open
   ```

3. Close chat, wait about 10 seconds, then:

   ```powershell
   .\scripts\measure-ram.ps1 -State chat-closed
   ```

Each command takes ten one-second-spaced samples by default. Use `-Samples 30 -IntervalSeconds 2` for a longer capture. To save elsewhere, pass `-OutputPath`. If multiple app processes exist, pass the intended root process ID with `-RootPid`.

The process tree is reconstructed from Windows process parent IDs at each sample; a descendant that exits before the snapshot is absent, and a process reparented by Windows may no longer be counted. The script reports rather than fabricates any readings.

## Project files

```text
AI-Companion-Phase0/
├── Cargo.toml
├── build.rs
├── README.md
├── assets/
│   ├── chat.html
│   └── robot.html
├── scripts/
│   └── measure-ram.ps1
└── src/
    └── main.rs
```
