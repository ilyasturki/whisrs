use std::sync::{Mutex as StdMutex, OnceLock};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use whisrs::InjectorBackend;

static KEYBOARD: OnceLock<StdMutex<Option<Box<dyn xkb_type::KeyInjector>>>> = OnceLock::new();

/// Type text at the cursor using uinput (keyboard injection) or clipboard paste.
pub(crate) fn type_text_at_cursor(
    text: &str,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        // Runtime path: short settle delay. The startup prewarm in
        // `warm_keyboard` is what gives X11 time to attach its keymap;
        // we don't want every error-recovery to stall the user's typing
        // path for 1s.
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let result = keyboard.type_text(text).context("failed to type text");
    if result.is_err() {
        *keyboard_guard = None;
    }
    result?;
    Ok(())
}

/// Send a paste keystroke — Ctrl+V, or Ctrl+Shift+V in terminals — via the
/// **persistent** virtual keyboard (the same device `type_text_at_cursor`
/// uses).
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the paste silently no-ops. The persistent device is already
/// recognized, so its keystrokes land. The combo is raw keycodes (`KEY_V` is
/// `v` in every common layout), so it stays layout-independent.
fn paste_via_keyboard(
    is_terminal: bool,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    use evdev::Key;

    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let combo: &[Key] = if is_terminal {
        &[Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT, Key::KEY_V]
    } else {
        &[Key::KEY_LEFTCTRL, Key::KEY_V]
    };

    let result = keyboard
        .send_combo(combo)
        .context("failed to send paste combo");
    if result.is_err() {
        *keyboard_guard = None;
    }
    result
}

/// Clear the current shell prompt line by sending Ctrl+A ("move to start of
/// line") then Ctrl+K ("kill to end of line") via the **persistent** virtual
/// keyboard (the same device `type_text_at_cursor` and `paste_via_keyboard`
/// use). That readline / zle / fish editing pair empties the line in bash, zsh
/// and fish alike.
///
/// Only ever called for terminals. A terminal's mouse highlight is a visual
/// overlay, not an editable selection, so injecting at the cursor would append
/// to the existing line rather than replace it. Clearing the line first makes
/// the injected text the whole line, which is what a "rewrite my selection"
/// command means at a prompt. In GUI text widgets no clear is needed (or
/// wanted): typing/pasting over a real selection replaces it natively.
///
/// Must NOT use a fresh per-call uinput device: on some compositors (e.g.
/// KWin) keystrokes from a device the compositor hasn't finished enumerating
/// are dropped, so the clear silently no-ops. The persistent device is already
/// recognized, so its keystrokes land.
pub(crate) fn clear_line_via_keyboard(
    key_delay: std::time::Duration,
    backend: InjectorBackend,
) -> Result<()> {
    use evdev::Key;

    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let mut keyboard_guard = keyboard_slot
        .lock()
        .map_err(|_| anyhow::anyhow!("keyboard mutex poisoned"))?;

    if keyboard_guard.is_none() {
        *keyboard_guard = Some(new_keyboard(
            key_delay, /* prewarm = */ false, backend,
        )?);
    }

    let keyboard = keyboard_guard
        .as_mut()
        .expect("keyboard exists after initialization");
    keyboard.set_key_delay(key_delay);

    let result = keyboard
        .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_A])
        .context("failed to send Ctrl+A (move to start of line)")
        .and_then(|()| {
            keyboard
                .send_combo(&[Key::KEY_LEFTCTRL, Key::KEY_K])
                .context("failed to send Ctrl+K (kill to end of line)")
        });
    if result.is_err() {
        *keyboard_guard = None;
    }
    result
}

/// Inject `text` at the cursor, choosing keystrokes or clipboard paste.
///
/// With `paste = false` (default) this types via the virtual keyboard. With
/// `paste = true` it sets the clipboard, sends Ctrl+V (Ctrl+Shift+V for
/// terminals), then restores the previous clipboard — layout-independent
/// injection for compositors that lack the Wayland virtual-keyboard protocol
/// (see [`whisrs::InputConfig::paste`]). Runs in a blocking context (callers
/// wrap it in `spawn_blocking`), so the sleeps/restore use std threads.
///
/// `ClipboardBackend` is text-only, so a clipboard holding non-text content
/// (an image, a file list, ...) can't be captured and round-tripped at all.
/// If the pre-paste read fails, this falls back to typing instead of pasting
/// — overwriting the clipboard via `set_text` first and only then discovering
/// there's nothing valid to restore would destroy that content permanently
/// (see #69), which skipping the *restore* alone can't undo since the damage
/// already happened at `set_text`.
///
/// The restore is otherwise skipped, rather than clobbering the clipboard,
/// when the clipboard no longer holds the text we set — something else (the
/// user, another app) copied over it during the paste, and restoring would
/// race that copy and discard it.
pub(crate) fn inject_text(
    text: &str,
    is_terminal: bool,
    key_delay: std::time::Duration,
    backend: InjectorBackend,
    paste: bool,
) -> Result<()> {
    if !paste {
        return type_text_at_cursor(text, key_delay, backend);
    }

    let clipboard = xkb_type::default_clipboard();
    let saved = match clipboard.get_text() {
        Ok(s) => s,
        Err(e) => {
            debug!("clipboard unreadable as text, typing instead of pasting: {e:#}");
            return type_text_at_cursor(text, key_delay, backend);
        }
    };
    clipboard
        .set_text(text)
        .context("failed to set clipboard for paste injection")?;

    // Let the clipboard settle before the paste keystroke.
    std::thread::sleep(std::time::Duration::from_millis(50));

    let paste_result = paste_via_keyboard(is_terminal, key_delay, backend);

    // Restore the user's clipboard after the paste has landed, regardless of
    // whether the keystroke succeeded — but only if it's safe to (see doc
    // comment above).
    let pasted_text = text.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let clipboard = xkb_type::default_clipboard();
        match clipboard.get_text() {
            Ok(current) if current == pasted_text => {
                if let Err(e) = clipboard.set_text(&saved) {
                    warn!("failed to restore clipboard: {e}");
                }
            }
            Ok(_) => {
                debug!("clipboard changed during paste injection; skipping restore");
            }
            Err(e) => {
                warn!("failed to read clipboard before restore, skipping restore: {e}");
            }
        }
    });

    paste_result
}

pub(crate) fn warm_keyboard(key_delay: std::time::Duration, backend: InjectorBackend) {
    let keyboard_slot = KEYBOARD.get_or_init(|| StdMutex::new(None));
    let Ok(mut keyboard_guard) = keyboard_slot.lock() else {
        warn!("failed to initialize virtual keyboard: keyboard mutex poisoned");
        return;
    };

    if keyboard_guard.is_some() {
        return;
    }

    // Startup path: long settle delay so X11 has time to process
    // MappingNotify and attach the device keymap before the first key.
    match new_keyboard(key_delay, /* prewarm = */ true, backend) {
        Ok(kb) => {
            *keyboard_guard = Some(kb);
            info!("virtual keyboard initialized");
        }
        Err(e) => {
            warn!("failed to initialize virtual keyboard: {e:#}");
        }
    }
}

/// Build the uinput (evdev) keyboard, mapping the common permission failure
/// to an actionable message. `prewarm` adds a settle delay so X11 attaches
/// the device keymap before the first keystroke.
fn new_uinput_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    let result = if prewarm {
        xkb_type::Keyboard::new_prewarm(key_delay)
    } else {
        xkb_type::Keyboard::new(key_delay)
    };
    match result {
        Ok(kb) => Ok(Box::new(kb)),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("Permission denied") || msg.contains("permission") {
                anyhow::bail!(
                    "Cannot open /dev/uinput — permission denied.\n\
                     Fix: sudo usermod -aG input $USER"
                );
            }
            Err(e.context("failed to create virtual keyboard"))
        }
    }
}

/// Construct the configured keyboard-injection backend.
///
/// `Auto` prefers the layout-independent Wayland virtual keyboard when a
/// Wayland session is detected, falling back to uinput when the compositor
/// lacks `zwp_virtual_keyboard_v1`. `prewarm` only affects the uinput path
/// (the Wayland backend ships its own keymap, so no settle delay is needed).
fn new_keyboard(
    key_delay: std::time::Duration,
    prewarm: bool,
    backend: InjectorBackend,
) -> Result<Box<dyn xkb_type::KeyInjector>> {
    match backend {
        InjectorBackend::Uinput => new_uinput_keyboard(key_delay, prewarm),
        InjectorBackend::WaylandVk => {
            let kb = xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay)?;
            info!("using wayland virtual-keyboard injection backend");
            Ok(Box::new(kb))
        }
        InjectorBackend::Auto => {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                match xkb_type::wayland_vk::WaylandVkKeyboard::new(key_delay) {
                    Ok(kb) => {
                        info!("using wayland virtual-keyboard injection backend");
                        return Ok(Box::new(kb));
                    }
                    Err(e) => {
                        warn!(
                            "wayland virtual-keyboard unavailable, falling back to uinput: {e:#}"
                        );
                    }
                }
            }
            new_uinput_keyboard(key_delay, prewarm)
        }
    }
}

/// Whole-identifier terminal matches, lowercased. Covers both the bare X11
/// WM_CLASS class form (`Alacritty`, `st-256color`) and the reverse-DNS
/// Wayland app_id form (`org.gnome.Terminal`).
///
/// Matching is exact, never substring: an unanchored `contains` on `"st"`
/// false-positived on `steam`, `Postman` and `systemsettings`, and command
/// mode then wiped the whole GUI text field with Ctrl+A/Ctrl+K (#70).
///
/// Provenance notes for the entries we could not capture at runtime:
/// - `waveterm`, `tabby` — source-derived only, no runtime capture.
/// - `dev.warp.warp` — unverified, from secondary sources only.
/// - `org.xfce.terminal`, `org.contourterminal.contour` — speculative and
///   defensive; the strings we did verify are `xfce4-terminal` and `contour`.
/// - st sets its class from `opt_class ? opt_class : termname`, and shipped
///   `config.def.h` sets `termname = "st-256color"` — so vanilla st reports
///   `st-256color`, and bare `st` needs a patch or `-c st`. The enumerated
///   `st-*` entries cover the common termnames (`st-mono` is defensive; it is
///   absent from local ncurses terminfo). A build with a custom `termname` or
///   `-c` class will not match and needs a user-level override.
const TERMINAL_CLASSES: &[&str] = &[
    "alacritty",
    "blackbox-terminal",
    "contour",
    "cool-retro-term",
    "deepin-terminal",
    "foot",
    "footclient",
    "ghostty",
    "ghostty-debug",
    "gnome-terminal",
    "gnome-terminal-server",
    "guake",
    // `hyper` is a generic word; safe only because matching is whole-string.
    "hyper",
    "kgx",
    "kitty",
    "koi8rxterm",
    "konsole",
    "lxterminal",
    "mate-terminal",
    "mlterm",
    "ptyxis",
    "qterminal",
    "rio",
    "roxterm",
    "rxvt",
    "rxvt-unicode",
    "sakura",
    "st",
    "st-16color",
    "st-256color",
    "st-direct",
    "st-mono",
    "tabby",
    "terminator",
    "terminology",
    "termite",
    "tilix",
    "urxvt",
    "urxvtc",
    "uxterm",
    "waveterm",
    "wezterm",
    "xfce4-terminal",
    "xterm",
    "yakuake",
    // reverse-DNS app_ids
    "com.gexperts.tilix",
    "com.mitchellh.ghostty",
    "com.mitchellh.ghostty-debug",
    "com.raggesilver.blackbox",
    "dev.warp.warp",
    // full entry, not a leaf: `terminal` is an excluded generic leaf
    "io.elementary.terminal",
    "org.contourterminal.contour",
    "org.gnome.console",
    "org.gnome.console.devel",
    "org.gnome.ptyxis",
    "org.gnome.terminal",
    "org.kde.konsole",
    "org.kde.yakuake",
    "org.wezfurlong.wezterm",
    "org.xfce.terminal",
];

/// Distinctive leaf names, matched after stripping a reverse-DNS prefix, so
/// repackaged/forked app_ids (e.g. `io.example.Ghostty`) still resolve.
///
/// Deliberately EXCLUDES generic leaves — `terminal`, `console`, `warp`,
/// `wave`, `rio`, `st`, `foot`, `tabby`, `blackbox`, `contour`. The exclusion
/// applies only to the dotted/leaf form; several of these still match as whole
/// identifiers at stage 1 (`st`, `foot`, `rio`, `tabby`, `contour`). One
/// collision is demonstrated: `app.drey.Warp` (leaf `warp`) is GNOME's Magic
/// Wormhole client, not Warp Terminal. The rest are precautionary, not against
/// a known clash: they are generic enough that any vendor could ship a
/// non-terminal whose app_id ends in `.Contour`, `.BlackBox`, `.Terminal` or
/// `.Console` — a payment terminal, a serial console, a web console.
const TERMINAL_LEAF_CLASSES: &[&str] = &[
    "alacritty",
    "cool-retro-term",
    "ghostty",
    "ghostty-debug",
    "guake",
    "kitty",
    "konsole",
    "ptyxis",
    "qterminal",
    "tilix",
    "waveterm",
    "wezterm",
    "yakuake",
];

/// Check if a window class corresponds to a terminal emulator.
///
/// A false positive here is destructive (command mode clears the line), while a
/// false negative merely degrades to plain injection — so both stages match on
/// whole identifiers or whole dot-segments, never on substrings.
pub(crate) fn is_terminal_class(class: &str) -> bool {
    let lower = class.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    // Stage 1: whole-identifier exact match.
    if TERMINAL_CLASSES.contains(&lower.as_str()) {
        return true;
    }
    // Stage 2: exact match on the last dot-segment, so repackaged reverse-DNS
    // app_ids still resolve. Only applies to dotted identifiers; a bare class
    // must appear in TERMINAL_CLASSES verbatim.
    if let Some((_, leaf)) = lower.rsplit_once('.') {
        if TERMINAL_LEAF_CLASSES.contains(&leaf) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-world strings, in the casing a compositor hands them to us: bare
    /// X11 WM_CLASS classes and reverse-DNS Wayland app_ids.
    #[test]
    fn terminal_classes_match() {
        for class in [
            "st",
            "st-256color",
            "st-direct",
            "gnome-terminal",
            "gnome-terminal-server",
            "org.gnome.Terminal",
            "org.gnome.Console",
            "org.gnome.Console.Devel",
            "kgx",
            "footclient",
            "foot",
            "konsole",
            "org.kde.konsole",
            "org.kde.yakuake",
            "org.wezfurlong.wezterm",
            "org.xfce.terminal",
            "org.contourterminal.contour",
            "com.mitchellh.ghostty",
            "com.mitchellh.ghostty-debug",
            "com.raggesilver.blackbox",
            "dev.warp.warp",
            "ghostty",
            "blackbox-terminal",
            "contour",
            "guake",
            "kitty",
            "rio",
            "rxvt",
            "sakura",
            "tabby",
            "termite",
            "tilix",
            "urxvtc",
            "waveterm",
            "wezterm",
            "xterm",
            "yakuake",
            "qterminal",
            "Alacritty",
            "cool-retro-term",
            "xfce4-terminal",
            "urxvt",
            "rxvt-unicode",
            "URxvt",
            "io.example.Ghostty",
            "Terminator",
            "Com.gexperts.Tilix",
            // added after the #70 review
            "uxterm",
            "koi8rxterm",
            "lxterminal",
            "roxterm",
            "ptyxis",
            "org.gnome.Ptyxis",
            "mate-terminal",
            "deepin-terminal",
            "io.elementary.terminal",
            "terminology",
            "mlterm",
            "hyper",
        ] {
            assert!(
                is_terminal_class(class),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }

    /// No entry may rot into dead weight: stage 1 must match every one of its
    /// own strings.
    #[test]
    fn every_terminal_class_entry_matches() {
        for class in TERMINAL_CLASSES {
            assert!(
                is_terminal_class(class),
                "{class} is in TERMINAL_CLASSES but is_terminal_class says it is not a terminal"
            );
        }
    }

    /// Same for stage 2, exercised through a reverse-DNS prefix.
    #[test]
    fn every_terminal_leaf_class_entry_matches() {
        for leaf in TERMINAL_LEAF_CLASSES {
            let class = format!("io.example.{leaf}");
            assert!(
                is_terminal_class(&class),
                "{class} should match via TERMINAL_LEAF_CLASSES but does not"
            );
            // Stage 2 only fires on dotted identifiers, so the bare class form
            // is matched solely by TERMINAL_CLASSES. A leaf-only entry would
            // leave bare `{leaf}` unmatched — a silent false negative.
            assert!(
                TERMINAL_CLASSES.contains(leaf),
                "{leaf} is in TERMINAL_LEAF_CLASSES but not TERMINAL_CLASSES, so the bare class form `{leaf}` would not match"
            );
        }
    }

    /// The generics the leaf set deliberately omits. Adding any of these to
    /// `TERMINAL_LEAF_CLASSES` is the most destructive edit possible here, so
    /// pin every one of them false.
    #[test]
    fn excluded_generic_leaves_do_not_match() {
        for class in [
            "io.example.Terminal",
            "com.foo.Console",
            "x.y.st",
            "a.b.foot",
            "x.y.tabby",
            "a.b.rio",
            "com.mapbox.contour",
            "com.example.BlackBox",
            "app.drey.Warp",
            "com.example.Wave",
        ] {
            assert!(
                !is_terminal_class(class),
                "{class} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// The `<base>-<suffix>` rule is enumerated, not open-ended: only the st
    /// `termname` values we list and the ghostty debug build match.
    #[test]
    fn hyphen_suffixes_are_enumerated_not_open_ended() {
        for class in [
            "st-link",
            "st-lite",
            "st-jerry",
            "st-",
            "st--",
            "ghostty-foo",
        ] {
            assert!(
                !is_terminal_class(class),
                "{class} is not a known terminal class but is_terminal_class says it is"
            );
        }
        for class in [
            "st-256color",
            "st-direct",
            "st-16color",
            "st-mono",
            "ghostty-debug",
            "com.mitchellh.ghostty-debug",
        ] {
            assert!(
                is_terminal_class(class),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }

    /// Stage 2 needs an actual dot, so the leaf set is not a second bare-string
    /// list: only `blackbox-terminal` is a real class, plain `blackbox` is not.
    #[test]
    fn leaf_set_requires_a_dot() {
        assert!(!is_terminal_class("blackbox"));
        assert!(is_terminal_class("blackbox-terminal"));
        assert!(is_terminal_class("io.example.Ghostty"));
    }

    /// The destructive direction: anything matched here gets Ctrl+A/Ctrl+K sent
    /// into it by command mode, which empties a GUI text field.
    #[test]
    fn non_terminal_classes_do_not_match() {
        for class in [
            "steam",
            "Steam",
            "Postman",
            "systemsettings",
            "gnome-system-monitor",
            "com.obsproject.Studio",
            "libreoffice-startcenter",
            "org.gnome.Settings",
            "obsidian",
            "code",
            "firefox",
            "Gnome-terminal-preferences",
            "org.gnome.Terminal.Preferences",
            "jconsole",
            "foot-server",
            "kitty-open",
            "assistant",
            "linguist",
            "lstopo",
            "xfce4-terminal-emulator",
            "Stremio",
            "standardnotes",
            "",
            "   ",
        ] {
            assert!(
                !is_terminal_class(class),
                "{class:?} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// Issue #70, first half: the old `lower.contains("st")` matched these.
    #[test]
    fn repro_st_substring_matches_non_terminals_issue70() {
        for class in [
            "steam",
            "com.obsproject.Studio",
            "systemsettings",
            "Postman",
            "libreoffice-startcenter",
        ] {
            assert!(
                !is_terminal_class(class),
                "{class} is not a terminal but is_terminal_class says it is"
            );
        }
    }

    /// Issue #70, second half: the old list held only bare X11 classes, so
    /// every reverse-DNS Wayland app_id was missed.
    #[test]
    fn repro_wayland_gnome_terminal_is_missed_issue70() {
        for class in [
            "org.gnome.Terminal",
            "org.gnome.Console",
            "kgx",
            "rio",
            "qterminal",
        ] {
            assert!(
                is_terminal_class(class),
                "{class} is a terminal but is_terminal_class says it is not"
            );
        }
    }
}
