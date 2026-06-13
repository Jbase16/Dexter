# Dexter Placement Controls

Dexter has two placement controls that are intentionally separate from the
voice hotkey.

## Built-in Keyboard Control

- Voice hotkey: `Control` + `Shift` + `Space`
- Placement key: right `Option`

Pressing right `Option` snaps Dexter to the current mouse location. While right
`Option` is still held, hold the primary mouse button and drag to move Dexter
around that screen. Releasing right `Option` saves the new position.

This keeps Dexter from following normal mouse movement between displays. He only
moves when the operator intentionally enters placement mode.

The placement regression covers both halves of that contract: mouse movement
while placement mode is active but the primary button is not held must not move
Dexter, and primary-button movement while placement mode is active must move the
panel by the expected delta.

## Magic Mouse Gesture Control

macOS does not provide a built-in global Magic Mouse gesture binding API that is
specific enough for Dexter to own directly. Use BetterTouchTool for this.

Recommended BetterTouchTool action:

```bash
/Users/jason/Developer/Dex/scripts/dexter-place.sh snap
```

Recommended trigger:

- Magic Mouse gesture: choose an unused single-tap or multi-finger gesture
- Action type: run shell script
- Script path/command: `/Users/jason/Developer/Dex/scripts/dexter-place.sh snap`

That command tells the running Dexter UI to center the orb on the current mouse
position. It does not activate the voice hotkey, does not talk to the Rust
daemon, and does not open a terminal window.

Optional script commands:

```bash
/Users/jason/Developer/Dex/scripts/dexter-place.sh snap
/Users/jason/Developer/Dex/scripts/dexter-place.sh move
/Users/jason/Developer/Dex/scripts/dexter-place.sh start
/Users/jason/Developer/Dex/scripts/dexter-place.sh stop
```

`snap`, `move`, and `center` all center Dexter on the current mouse location.
`start`/`hold`/`drag` and `stop`/`release`/`end` are available for automation
tools that can emit separate press/release events.

The Dexter app menu exposes the same controls:

- `Dexter > Move Dexter to Mouse`
- `Dexter > Start Dexter Placement Drag`
- `Dexter > Stop Dexter Placement Drag`

Regression coverage:

```bash
make live-smoke-hud-placement
make live-smoke-placement-command
```

`live-smoke-hud-placement` verifies the in-app placement mechanics and
click-through geometry. `live-smoke-placement-command` launches the real Swift
app, invokes `scripts/dexter-place.sh snap`, `start`, and `stop`, and verifies
the distributed notifications are received by the running app.
