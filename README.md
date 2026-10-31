# atmos-lcd

A lightweight replacement for Cooler Master MasterCTRL that drives the LCD on the
**MasterLiquid Atmos II LCD** AIO cooler (USB `2516:0222`). It is a single Windows
executable that lives in the system tray, with no services and no background
agents.

Not affiliated with or endorsed by Cooler Master.

## Features

- **Screens**
  - **System:** CPU load, GPU temperature and GPU load.
  - **Clock**
  - **Media:** pictures, animated GIF/WebP and videos, at up to 60 fps.
- **Gallery:** media is converted once and streamed from disk, so memory stays
  low whatever you play. Add files with the button, select one to show it, and
  delete what you no longer need.
- **Offline mode:** drag gallery items onto the cooler's five storage slots. The
  cooler cycles through them on its own while the PC is off or not streaming.
- **Rotation** in 30° steps, for mounting the pump at any angle (hexagonal pump
  heads included). Stored slots are re-exported when the rotation changes.
- **Brightness**, **display while the PC sleeps**, **start with Windows**.
- **Live preview:** the settings window shows what the LCD shows, animations
  included.
- **Small footprint:** about 8 MB of memory while in the tray. CPU and GPU
  readings come straight from Windows, the same sources Task Manager uses.

## Requirements

- Windows 10 2004 or later (64-bit).
- GPU temperature needs a WDDM 2.7+ graphics driver. Load is shown either way.
- Quit MasterCTRL first. Both programs talk to the same USB device.

## Usage

Run `atmos-lcd.exe`. The first launch opens the settings window; later launches
start in the tray. Starting the exe again while it is running opens its window,
and closing the window keeps the app in the tray until you choose 종료 (Quit).

The UI is in Korean.

### Command line

Running the exe with a command performs one task and exits, which helps with
scripting and debugging:

```
atmos-lcd param                         print current device parameters
atmos-lcd brightness <0-100>
atmos-lcd rotate <degrees>              saved to config
atmos-lcd run <screen>                  show a screen in the foreground until Ctrl+C
atmos-lcd preview <screen> <out.jpg>    render a screen to a file without the device
atmos-lcd stats                         print CPU and GPU readings for a few seconds
atmos-lcd gallery                       list gallery items
atmos-lcd import <file>                 add a picture, GIF/WebP or video to the gallery
atmos-lcd bench <id> [fps...]           measure frame transfer speed with a gallery item
atmos-lcd media-upload <name> <type> <file>
atmos-lcd media-delete <name> <type>
atmos-lcd realtime on|off               off: play the device's stored media

screens: clock | system | gallery <id> | file <path>   (file: preview only)
```

### Supported media

- Pictures: PNG, JPEG, BMP.
- Animations: GIF, WebP.
- Videos: MP4, M4V, MOV, WMV, AVI, MKV and WebM, through Windows Media
  Foundation (whatever codecs Windows has). Videos are cut at 60 seconds.

### Where things are stored

| What | Where |
|---|---|
| Settings | `%APPDATA%\atmos-lcd\config.txt` |
| Gallery | `%LOCALAPPDATA%\atmos-lcd\gallery\` |
| Start with Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `AtmosLCD` |

## Building

The app is written in Rust with [Slint](https://slint.dev) for the UI. It
cross-compiles from Linux or WSL with MinGW, which is set as the default target
in `.cargo/config.toml`:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64
cargo build --release
# target/x86_64-pc-windows-gnu/release/atmos-lcd.exe
```

To build natively on Windows with MSVC, remove the `[build]` target in
`.cargo/config.toml` and run `cargo build --release`.

### Patched Slint backend

`patches/i-slint-backend-winit` is Slint 1.18.1's winit backend with two fixes
for the software renderer:

- [`slint-sw-repaint-after-minimize.diff`](patches/slint-sw-repaint-after-minimize.diff):
  the window was left blank after restoring from minimized.
- [`slint-sw-free-buffer-on-hide.diff`](patches/slint-sw-free-buffer-on-hide.diff):
  the window-sized pixel buffer is freed while the window is hidden.

## Protocol

The USB protocol was reverse engineered from MasterCTRL and USB captures. It is
documented in [PROTOCOL.md](PROTOCOL.md), covering the handshake, command
framing, realtime streaming and stored media. `tools/` has the C# probe and the
pcap parser used during the analysis.

## License

MIT, see [LICENSE](LICENSE). Exceptions:

- `patches/i-slint-backend-winit` keeps Slint's own license (GPL-3.0 or the Slint
  Royalty-free License 2.0, see its `LICENSES/` folder). This app uses Slint
  under the Royalty-free License.
- The Pretendard fonts in `ui/fonts` are under the SIL Open Font License 1.1.

## Credits

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)

- [Pretendard](https://github.com/orioncactus/pretendard) font, SIL Open Font
  License 1.1 (`ui/fonts/LICENSE.txt`).
