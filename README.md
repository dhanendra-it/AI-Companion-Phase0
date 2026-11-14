# Aalu

A Windows 11 desktop companion with a floating robot, AI chat, voice calls, screen context, and a daily scheduler.

## Scope and status

- **Included:** Tao native frameless transparent always-on-top robot window, click to open chat, Alt+Space chat, Right Ctrl+Space scheduler, and Right Ctrl+Up robot visibility hotkeys, Wry/WebView2 chat, AI providers, voice calling, screen context, reminders, custom robot window icon, Windows sign-in startup registration, and process-tree RAM measurement script.
- **Windows startup:** when the app launches, it registers itself for the current Windows user so the robot starts at sign-in.
- **Window icon:** the chat and scheduler windows use the custom Aalu robot icon.
- **Close behavior:** closing the chat window drops its Tao window and Wry WebView object; clicking the robot creates a fresh chat window.
- **Not implemented:** packaging/installer and persistent chat history. Robot dragging, hotkeys, and startup behavior still need Windows runtime verification.
- **Verification:** `cargo check --release` passes with the updated shortcuts. The previous full build could not replace `target\release\aalu.exe` (Access denied); close Aalu before rebuilding. Shortcut behavior still needs a live Windows check.
- **Shortcuts:** Alt+Space toggles chat; Right Ctrl+Space toggles the scheduler; Right Ctrl+Up shows or hides the robot.

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

Aalu stays above other windows. Left-click it for chat, middle-click to toggle a voice call, and use mouse button 3 to open or close the scheduler. Press Right Ctrl+Space to toggle the scheduler, Alt+Space to toggle chat, or Right Ctrl+Up to show/hide the robot. Escape closes the focused scheduler or chat window.

If WebView2 cannot be created, confirm the Evergreen Runtime is installed and retry. Cargo's first build downloads Rust crates.

## Manual verification steps (not performed here)

1. Run the app and confirm the robot has no visible window rectangle, stays above ordinary windows, and can be dragged.
2. Click the robot; confirm one chat window opens. Close it, click the robot again, and confirm a fresh chat window opens.
3. Press Right Ctrl+Space to open and close the scheduler; press Alt+Space for chat; press Right Ctrl+Up to hide and show the robot.
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
Aalu/
├── .gitignore
├── Cargo.lock
├── Cargo.toml
├── README.md
├── assets/
│   ├── chat.html
│   └── robot.html
├── scripts/
│   └── measure-ram.ps1
└── src/
    └── main.rs
```
