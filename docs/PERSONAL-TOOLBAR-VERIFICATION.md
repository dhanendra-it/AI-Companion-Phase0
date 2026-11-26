# Personal Toolbar verification

The toolbar is hosted in the Windows WebView2 window and uses Windows Shell/clipboard APIs. Run these checks on a Windows 10/11 machine with WebView2 Runtime installed.

## Automated checks

The `.github/workflows/windows-check.yml` workflow runs:

- `cargo check --locked --release`
- `cargo test --locked`

A green workflow confirms the Rust project compiles and its Rust unit tests pass. It does not exercise interactive WebView2 behavior.

## Manual smoke test

1. Start Aalu and open the Personal Toolbar using the configured mouse gesture while the pointer is over the robot.
2. Open the toolbar, close it with the center button, reopen it, and verify the layout and saved items remain available.
3. Open **Settings**. Toggle **Show in SS** both ways, close/reopen the toolbar, and confirm the selected state persists.
4. From Settings, open **Window Manager**. Verify **Show / hide Aalu**, **Open chat**, and **Open scheduler** each perform the matching action.
5. Open **Apps** from the ring's pinned app slots. Add a Windows application or `.lnk` file with Browse file / app, save it, launch it, and verify it opens. Right-click the pinned item and confirm removal.
6. Open **Shortcuts**. Add a desktop application path or `.lnk` file, launch it, and verify it opens.
7. Open **Folders**. Browse for a folder, save it, open it, and verify Explorer displays the selected folder.
8. Open **Text Snippets**. Add text containing punctuation, line breaks, and non-English characters. Activate it, paste into Notepad, and verify the exact text was copied.
9. Open **Bookmarks**. Add a full URL and a bare domain. Activate both and verify they open in the default browser.
10. In each add dialog, test Cancel, empty name/target validation, file-picker cancellation, and a deliberately invalid target. Confirm the toolbar remains usable and reports errors rather than silently failing.
11. Add enough items to create multiple pages. Scroll through the pages, activate an item on a later page, and verify right-click removal removes the correct item.
12. Restart Aalu and verify all saved items and Settings persist.

## Known boundaries

- Shortcuts in the current toolbar are pinned launch targets (applications, Windows shortcuts, folders, or URLs); they do not register new global keyboard combinations.
- Native picker, shell launching, clipboard access, focus behavior, and screenshot-exclusion behavior require the Windows manual checks above. They cannot be validated by a source-only GitHub edit.
