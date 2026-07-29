//! Terminal input: turn raw-mode stdin (mouse and keyboard) into Bevy messages.
//!
//! # Design
//!
//! crossterm's blocking `event::read()` runs on a BACKGROUND thread (it cannot
//! share the Bevy schedule) and pushes decoded events down an mpsc channel. A Bevy
//! system drains the channel each tick, in [`KittySet::Input`], and:
//!
//! * a mouse click reverse-maps the terminal cell through [`FitBox`] to a viewport
//!   position, and writes a [`KittyClick`] message,
//! * a key writes a [`KittyKey`] message, AND a real
//!   [`KeyboardInput`](bevy::input::keyboard::KeyboardInput) plus
//!   `ButtonInput<KeyCode>` press/release, matching what winit would produce, so
//!   existing keyboard handling works unchanged.
//!
//! Clicks are *not* injected into any Bevy input state, because there is no
//! meaningful cursor: a terminal reports a cell, and turning that into a world
//! position is the app's business. Hence the message. Ordering your handler
//! `.after(KittySet::Input)` makes it land the same frame.
//!
//! Raw mode and mouse reporting are enabled on install and restored on Ctrl-C or
//! exit, so the user's terminal is never left wedged.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;
use std::io::Write as _;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;

use crate::term::{FitBox, TermSize};
use crate::{KittyCamera, KittyConfig, KittySet};

/// A click somewhere in the rendered world.
///
/// `viewport` is in `virtual_size` space with a top-left origin, ready to hand to
/// `Camera::viewport_to_world_2d`. Clicks that land in the letterbox padding are
/// dropped rather than clamped, so a message always refers to a real world point.
#[derive(Message, Debug, Clone, Copy)]
pub struct KittyClick {
    /// Position in `virtual_size` space, top-left origin.
    pub viewport: Vec2,
    /// The 1-based terminal cell that was clicked, for logging.
    pub cell: (u16, u16),
}

/// A key press or release read from the terminal.
///
/// A matching [`KeyboardInput`] is written too, so an app that already handles
/// keyboard input needs nothing extra. This message is for cases that want to know
/// the input came from the terminal.
#[derive(Message, Debug, Clone)]
pub struct KittyKey {
    pub code: KeyCode,
    pub logical: Key,
    pub text: Option<String>,
    pub pressed: bool,
}

/// One decoded terminal input event, on its way from the reader thread.
enum TermEvent {
    /// Left-click at a 1-based terminal (col, row) cell.
    Click { col: u16, row: u16 },
    /// A key press/release, already mapped to Bevy's KeyCode and logical Key.
    Key {
        code: KeyCode,
        logical: Key,
        text: Option<String>,
        pressed: bool,
    },
}

/// The receiver end of the stdin -> ECS channel.
///
/// Wrapped in a `Mutex` because a `Receiver` is `Send` but not `Sync`, and Bevy
/// resources must be `Sync`.
#[derive(Resource)]
struct InputChannel(Mutex<Receiver<TermEvent>>);

/// Register the message types, ALWAYS, whether or not input is actually enabled.
///
/// Kept separate from [`build`] on purpose. A system that reads `KittyClick`
/// panics with "Message not initialized" if the type was never registered, so an
/// app whose input is switched off (or whose stdout is a pipe, so raw mode fails)
/// would crash on a message it will simply never receive. Registration is free;
/// skipping it is a landmine.
pub(crate) fn register_messages(app: &mut App) {
    app.add_message::<KittyClick>();
    app.add_message::<KittyKey>();
}

pub(crate) fn build(app: &mut App) {
    // Enable raw mode and mouse reporting up front. If this fails (e.g. stdout is
    // not a tty) log loudly and skip input rather than crash: rendering to a pipe
    // is a legitimate thing to do.
    if let Err(e) = enable_terminal_input() {
        warn!("[kitty] could not enable terminal input ({e}); running without it");
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel::<TermEvent>();
    spawn_reader(tx);
    app.insert_resource(InputChannel(Mutex::new(rx)));
    app.add_systems(Update, drain_input.in_set(KittySet::Input));
    info!("[kitty] terminal input enabled (mouse + keyboard)");
}

/// Turn on raw mode and SGR mouse reporting, plus a best-effort kitty keyboard
/// enhancement so key releases and repeats are reported where supported.
fn enable_terminal_input() -> std::io::Result<()> {
    use crossterm::event::{
        EnableMouseCapture, KeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
    use crossterm::execute;
    use crossterm::terminal::enable_raw_mode;
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnableMouseCapture)?;
    // Optional: richer key events. Ignore failure, not all terminals do it.
    let _ = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
    );
    Ok(())
}

/// Restore the terminal to a sane state. Safe to call more than once.
fn restore_terminal() {
    use crossterm::event::{DisableMouseCapture, PopKeyboardEnhancementFlags};
    use crossterm::execute;
    use crossterm::terminal::disable_raw_mode;
    let mut out = std::io::stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableMouseCapture);
    let _ = disable_raw_mode();
    // Show the cursor again (the plugin hid it at startup).
    let mut buf = Vec::new();
    crate::proto::leave_screen(&mut buf);
    let _ = out.write_all(&buf);
    let _ = out.flush();
}

/// Background thread: block on crossterm events, map them, and forward.
///
/// Also handles Ctrl-C, because raw mode swallows the default SIGINT on ^C, so we
/// have to watch for it ourselves or the user cannot quit.
fn spawn_reader(tx: Sender<TermEvent>) {
    std::thread::Builder::new()
        .name("kitty-input".into())
        .spawn(move || {
            use crossterm::event::{
                read, Event, KeyCode as CtKey, KeyEventKind, KeyModifiers, MouseButton,
                MouseEventKind,
            };
            loop {
                let ev = match read() {
                    Ok(e) => e,
                    Err(e) => {
                        // stdin closed or errored: restore and stop reading.
                        // eprintln! rather than the Bevy logger: this thread may
                        // outlive the app, and stderr is the safe channel (stdout
                        // is the graphics stream).
                        eprintln!("[kitty] input reader stopping: {e}");
                        restore_terminal();
                        return;
                    }
                };
                match ev {
                    Event::Mouse(m) => {
                        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                            // crossterm columns/rows are 0-based; the kitty
                            // graphics protocol and FitBox are 1-based.
                            let _ = tx.send(TermEvent::Click {
                                col: m.column + 1,
                                row: m.row + 1,
                            });
                        }
                    }
                    Event::Key(k) => {
                        // Ctrl-C: restore and exit the process.
                        if matches!(k.code, CtKey::Char('c'))
                            && k.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            restore_terminal();
                            std::process::exit(130);
                        }
                        // Release and repeat only arrive if the enhancement flag
                        // took; the default (no kind) is treated as a press.
                        let pressed = !matches!(k.kind, KeyEventKind::Release);
                        if let Some((code, logical, text)) = map_key(k.code) {
                            let _ = tx.send(TermEvent::Key {
                                code,
                                logical,
                                text,
                                pressed,
                            });
                        }
                    }
                    _ => {}
                }
            }
        })
        .expect("failed to spawn kitty input thread");
}

/// Map a crossterm key to (Bevy `KeyCode`, logical `Key`, optional text).
fn map_key(code: crossterm::event::KeyCode) -> Option<(KeyCode, Key, Option<String>)> {
    use crossterm::event::KeyCode as C;
    Some(match code {
        C::Char(c) => {
            let s = c.to_string();
            // `Key::Character` takes bevy_input's `SmolStr`, which is an ALIAS:
            // it is `smol_str::SmolStr` with bevy_input's `smol_str` feature on and
            // plain `String` with it off. Converting via `.into()` compiles under
            // both, so this crate does not break when cargo feature unification
            // flips that in a larger workspace.
            (
                char_to_keycode(c),
                Key::Character(s.clone().into()),
                Some(s),
            )
        }
        C::Enter => (KeyCode::Enter, Key::Enter, None),
        C::Backspace => (KeyCode::Backspace, Key::Backspace, None),
        C::Esc => (KeyCode::Escape, Key::Escape, None),
        C::Left => (KeyCode::ArrowLeft, Key::ArrowLeft, None),
        C::Right => (KeyCode::ArrowRight, Key::ArrowRight, None),
        C::Up => (KeyCode::ArrowUp, Key::ArrowUp, None),
        C::Down => (KeyCode::ArrowDown, Key::ArrowDown, None),
        C::Tab => (KeyCode::Tab, Key::Tab, None),
        C::Delete => (KeyCode::Delete, Key::Delete, None),
        C::Home => (KeyCode::Home, Key::Home, None),
        C::End => (KeyCode::End, Key::End, None),
        C::PageUp => (KeyCode::PageUp, Key::PageUp, None),
        C::PageDown => (KeyCode::PageDown, Key::PageDown, None),
        C::F(n @ 1..=12) => {
            let (code, logical) = f_key(n)?;
            (code, logical, None)
        }
        _ => return None,
    })
}

/// Physical and logical key for a function key number.
///
/// `Key` has no `F(n)` variant in Bevy 0.19: F1 to F12 are separate variants, so
/// the mapping has to be spelled out.
fn f_key(n: u8) -> Option<(KeyCode, Key)> {
    Some(match n {
        1 => (KeyCode::F1, Key::F1),
        2 => (KeyCode::F2, Key::F2),
        3 => (KeyCode::F3, Key::F3),
        4 => (KeyCode::F4, Key::F4),
        5 => (KeyCode::F5, Key::F5),
        6 => (KeyCode::F6, Key::F6),
        7 => (KeyCode::F7, Key::F7),
        8 => (KeyCode::F8, Key::F8),
        9 => (KeyCode::F9, Key::F9),
        10 => (KeyCode::F10, Key::F10),
        11 => (KeyCode::F11, Key::F11),
        12 => (KeyCode::F12, Key::F12),
        _ => return None,
    })
}

/// Best-effort physical-key guess for a typed character.
///
/// A terminal reports characters, not scan codes, so this cannot be exact for a
/// non-US layout. Text-entry code should read the logical `Key`/`text`, which
/// always carries the real character; this only needs to be right for keys used as
/// controls.
fn char_to_keycode(c: char) -> KeyCode {
    match c.to_ascii_lowercase() {
        'a' => KeyCode::KeyA,
        'b' => KeyCode::KeyB,
        'c' => KeyCode::KeyC,
        'd' => KeyCode::KeyD,
        'e' => KeyCode::KeyE,
        'f' => KeyCode::KeyF,
        'g' => KeyCode::KeyG,
        'h' => KeyCode::KeyH,
        'i' => KeyCode::KeyI,
        'j' => KeyCode::KeyJ,
        'k' => KeyCode::KeyK,
        'l' => KeyCode::KeyL,
        'm' => KeyCode::KeyM,
        'n' => KeyCode::KeyN,
        'o' => KeyCode::KeyO,
        'p' => KeyCode::KeyP,
        'q' => KeyCode::KeyQ,
        'r' => KeyCode::KeyR,
        's' => KeyCode::KeyS,
        't' => KeyCode::KeyT,
        'u' => KeyCode::KeyU,
        'v' => KeyCode::KeyV,
        'w' => KeyCode::KeyW,
        'x' => KeyCode::KeyX,
        'y' => KeyCode::KeyY,
        'z' => KeyCode::KeyZ,
        ' ' => KeyCode::Space,
        '0' => KeyCode::Digit0,
        '1' => KeyCode::Digit1,
        '2' => KeyCode::Digit2,
        '3' => KeyCode::Digit3,
        '4' => KeyCode::Digit4,
        '5' => KeyCode::Digit5,
        '6' => KeyCode::Digit6,
        '7' => KeyCode::Digit7,
        '8' => KeyCode::Digit8,
        '9' => KeyCode::Digit9,
        '-' => KeyCode::Minus,
        '=' => KeyCode::Equal,
        ',' => KeyCode::Comma,
        '.' => KeyCode::Period,
        '/' => KeyCode::Slash,
        ';' => KeyCode::Semicolon,
        '\'' => KeyCode::Quote,
        '[' => KeyCode::BracketLeft,
        ']' => KeyCode::BracketRight,
        '\\' => KeyCode::Backslash,
        '`' => KeyCode::Backquote,
        _ => KeyCode::Space,
    }
}

/// Drain queued terminal events each tick and turn them into messages.
// A Bevy system's arguments ARE its dependency injection, so the argument-count
// lint does not apply: splitting this would mean two systems racing one channel.
#[allow(clippy::too_many_arguments)]
fn drain_input(
    chan: Res<InputChannel>,
    config: Res<KittyConfig>,
    mut clicks: MessageWriter<KittyClick>,
    mut kitty_keys: MessageWriter<KittyKey>,
    mut key_events: MessageWriter<KeyboardInput>,
    mut keys: ResMut<ButtonInput<KeyCode>>,
    camera_q: Query<Entity, With<KittyCamera>>,
    mut term: Local<Option<TermSize>>,
    mut ticks: Local<u64>,
) {
    *ticks += 1;
    // Re-query the terminal size occasionally so mouse mapping tracks resizes.
    if term.is_none() || ticks.is_multiple_of(120) {
        *term = Some(TermSize::query(config.terminal_size));
    }
    let fit = FitBox::compute(&term.unwrap(), config.virtual_size);
    // KeyboardInput needs a "window" entity. Headless there is none, so reuse the
    // camera entity as a stand-in; handlers that only read the key ignore it.
    let window_stub = camera_q.single().unwrap_or(Entity::PLACEHOLDER);

    let Ok(rx) = chan.0.lock() else {
        error!("[kitty] input channel mutex poisoned; terminal input is dead this tick");
        return;
    };
    while let Ok(ev) = rx.try_recv() {
        match ev {
            TermEvent::Click { col, row } => {
                // Reverse the FitBox: cell -> terminal px -> viewport.
                match fit.cell_to_viewport(col, row) {
                    Some(vp) => {
                        info!(
                            "[kitty] click cell ({col},{row}) -> viewport ({:.0},{:.0})",
                            vp.x, vp.y
                        );
                        clicks.write(KittyClick {
                            viewport: vp,
                            cell: (col, row),
                        });
                    }
                    None => {
                        debug!("[kitty] click cell ({col},{row}) is in the letterbox - ignored");
                    }
                }
            }
            TermEvent::Key {
                code,
                logical,
                text,
                pressed,
            } => {
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                if pressed {
                    keys.press(code);
                } else {
                    keys.release(code);
                }
                key_events.write(KeyboardInput {
                    key_code: code,
                    logical_key: logical.clone(),
                    state,
                    // Same alias story as `Key::Character` in `map_key`: `.into()`
                    // works whether bevy_input's `SmolStr` is smol_str or String.
                    text: text.clone().map(Into::into),
                    repeat: false,
                    window: window_stub,
                });
                kitty_keys.write(KittyKey {
                    code,
                    logical,
                    text,
                    pressed,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode as C;

    #[test]
    fn typed_characters_carry_their_text() {
        // Text entry reads the logical key and `text`; losing them makes typing
        // silently do nothing.
        let (code, logical, text) = map_key(C::Char('q')).unwrap();
        assert_eq!(code, KeyCode::KeyQ);
        assert_eq!(text.as_deref(), Some("q"));
        assert!(matches!(logical, Key::Character(_)));
    }

    #[test]
    fn uppercase_maps_to_the_same_physical_key_but_keeps_its_case() {
        let (code, _, text) = map_key(C::Char('Q')).unwrap();
        assert_eq!(code, KeyCode::KeyQ, "physical key is layout-level");
        assert_eq!(
            text.as_deref(),
            Some("Q"),
            "but the text must stay uppercase"
        );
    }

    #[test]
    fn control_keys_have_no_text() {
        // A control key with text would insert a stray character into a text field.
        for k in [C::Enter, C::Esc, C::Backspace, C::Tab, C::Up] {
            let (_, _, text) = map_key(k).unwrap();
            assert!(text.is_none(), "{k:?} should carry no text");
        }
    }

    #[test]
    fn unmapped_keys_are_dropped_rather_than_guessed() {
        assert!(map_key(C::Insert).is_none());
        assert!(map_key(C::F(13)).is_none());
    }

    #[test]
    fn punctuation_gets_a_real_keycode_not_space() {
        // Falling back to Space for punctuation would make '-' act as a jump key
        // in a game that binds Space.
        for c in ['-', '=', ',', '.', '/', ';', '[', ']', '\\', '`', '\''] {
            assert_ne!(
                char_to_keycode(c),
                KeyCode::Space,
                "{c:?} should not fall back to Space"
            );
        }
        // Something genuinely unmappable still falls back, deliberately.
        assert_eq!(char_to_keycode('\u{263A}'), KeyCode::Space);
    }

    #[test]
    fn function_keys_map_in_order() {
        assert_eq!(f_key(1).unwrap().0, KeyCode::F1);
        assert_eq!(f_key(12).unwrap().0, KeyCode::F12);
        // Out of range must be None, not silently F12: `map_key` relies on this.
        assert!(f_key(0).is_none());
        assert!(f_key(13).is_none());
    }
}
