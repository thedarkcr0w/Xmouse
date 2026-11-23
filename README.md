<div align="center">

# XMouse

**Play your own Xbox or PlayStation with a mouse and keyboard, from your PC.**

One small Windows app. Pick your console, press Play, and its picture and sound open in a window on your PC while your mouse and keyboard drive a virtual controller.

![GTA VI played with mouse and keyboard through XMouse](docs/gta6-playing.jpg)

</div>

## What it does

- **Xbox and PlayStation.** Xbox works out of the box. PlayStation 5 and 4 work through **PSNBridge**, a free plugin XMouse installs for you with one click.
- **Your console, on your PC.** The game streams over your home network in up to 1080p at 60 fps, with sound, in its own window. Go fullscreen with F11.
- **Mouse and keyboard as a controller.** Every key and mouse button can be any controller button, stick direction or trigger. Mouse movement becomes a smooth, tunable right stick.
- **Speaks your controller's language.** On PlayStation the app shows a DualSense and PlayStation button names (Cross, Circle, L1, R2, Options); on Xbox it shows an Xbox controller and A, B, LB, RT.
- **Frame generation.** Turn on AMD FSR or NVIDIA DLSS frame generation and a 30 fps game plays at 60 (or 60 at 120). XMouse downloads the maker's files for you with one click.
- **A cleaner picture.** Optional clean-up for compression blocks and banding, sharpening, vibrance and contrast, all behind their own switches.
- **Sharper picture on NVIDIA RTX.** RTX Video Super Resolution upscales the stream to your monitor, so 1080p looks crisp on a 1440p or 4K screen.
- **GPU video decoding.** Turn it on and your graphics card decodes the stream in under 2 ms a frame instead of the CPU, for less input lag. It falls back to the CPU by itself if it can't run.
- **Ready-made game profiles.** GTA VI, GTA V, Red Dead Redemption 2, a standard shooter layout, third-person action games and the console menus. Each one is fully editable and works on both consoles.
- **Layers.** One key can switch the whole layout: in GTA, `F` gets you into a car and your keys turn into driving controls, then `F` again gets you back out.
- **Built to feel tight.** Input goes out at 120 Hz. Video is decoded the moment the last packet of a frame lands, and the app never builds up a backlog.
- **Always know your keys.** A see-through legend at the side of the picture lists what every key does in the current layer, and a live controller in the corner lights up as you press.
- **TV Play too.** Prefer the big screen? Keep the picture on your TV and use XMouse only for the controls.

<div align="center">

![GTA VI on foot](docs/gta6-running.jpg)

</div>

## Getting started

**Download** `XMouse.exe` from [Releases](../../releases) and run it. There's nothing to install. The first time, XMouse asks which console you play on.

<div align="center">
<img src="docs/console-picker.png" alt="Which console do you play on? Xbox or PlayStation" width="720">
</div>

### Xbox

1. **On your Xbox**, open *Settings → Devices & connections → Remote features* and tick *Enable remote features*. On older dashboards there is also *Xbox app preferences*: if you see it, allow connections from any device (newer Series X|S dashboards no longer have it, which is fine). Set *Power mode* to *Sleep* so XMouse can wake the console, then run *Test remote play* in the same menu to check your network.
2. **Pick Xbox** and **sign in** with the Microsoft account that's signed in on your Xbox. XMouse shows a short code and a Microsoft sign-in link; enter the code there.
3. **Pick your console and game**, choose *Picture: On this PC*, and press **Play**.
4. **Click the picture** to hand the mouse and keyboard to the game. **F8** gives them back.

XMouse works well on an **Xbox Series S** and great on an **Xbox Series X**: the Series X sends a better-quality stream and responds with less input lag.

### PlayStation

1. **On your PS5 or PS4**, turn on Remote Play (on PS5: *Settings → System → Remote Play*). To let XMouse wake it from rest mode, turn on *Stay connected to the internet* and *Enable turning on from network* under the rest mode power settings.
2. **Click Install PSNBridge** under PlayStation. XMouse downloads the [PSNBridge plugin](https://github.com/thedarkcr0w/xmouse-psnbridge) and installs it. You can also install or update it later under *More → Plugins*.
3. **Pick PlayStation** and enter your **PSN account ID** (the PSNBridge README explains where to find it). It stays on your PC.
4. **Pick your console**, enter the PIN the console shows for linking a device (on PS5: *Settings → System → Remote Play → Link Device*), and press **Play**.

> PlayStation support is new and hasn't been tried on a real console yet. If something doesn't work, please [open an issue](https://github.com/thedarkcr0w/xmouse-psnbridge/issues) and say what happened.

Your PC and console should be on the same home network. A wired connection for either one gives the steadiest picture. You can switch consoles at any time under *More → Console type*.

## Make it yours

<table>
<tr>
<td width="50%"><img src="docs/home.png" alt="Home: console, game and Play"></td>
<td width="50%"><img src="docs/keys.png" alt="Key editor on a drawn keyboard"></td>
</tr>
<tr>
<td><b>Home.</b> Your console, your game, one big Play button.</td>
<td><b>Keys.</b> Click any key on the keyboard and pick what it does. Hold, tap, toggle and turbo modes, plus layers.</td>
</tr>
<tr>
<td width="50%"><img src="docs/sens.png" alt="Sensitivity and aim settings"></td>
<td width="50%"><img src="docs/test-pad.png" alt="Live test pad with an Xbox controller"></td>
</tr>
<tr>
<td><b>Sens &amp; aim.</b> Sensitivity, slower aiming while you hold aim, deadzone, fine aim and smoothing, per game.</td>
<td><b>Test.</b> Try your setup without touching the console. Every press lights up on the controller, and the sticks move with your keys and mouse.</td>
</tr>
<tr>
<td width="50%"><img src="docs/test-pad-ps.png" alt="Live test pad with a DualSense"></td>
<td width="50%"><img src="docs/console-picker.png" alt="Console picker"></td>
</tr>
<tr>
<td><b>DualSense on PlayStation.</b> The test pad, the in-game overlay and every key label follow the console you play on.</td>
<td><b>One app, both consoles.</b> Switch under <i>More → Console type</i>; your game profiles come along.</td>
</tr>
</table>

**Find the deadzone in game.** Press **F6** during play. XMouse nudges the right stick a little at a time; use `[` and `]` until the camera just starts to move, then press F6 again to save it. Then set your sensitivity.

## Keys while you play

| Key | What it does |
| --- | --- |
| **F8** | Give the mouse and keyboard back to Windows |
| **F11** | Fullscreen |
| **F7** | Show or hide the controller and the controls legend |
| **F6** | Deadzone finder |

### GTA VI layout

Button names are Xbox / PlayStation.

| On foot | | In a vehicle | |
| --- | --- | --- | --- |
| `W A S D` | Move | `W` / `S` | Accelerate / brake |
| Mouse | Look and aim | `A` / `D` | Steer |
| `Shift` | Sprint | `Shift` / `Space` | A / X (Cross / Square) |
| `Space` | Jump | `H` | Horn |
| `F` | Enter vehicle | `F` | Exit vehicle |
| Right / left click | Aim / shoot | Mouse back / forward | Drive-by aim / handbrake |
| Mouse back / forward | Weapon wheel / cover | `R` | Cinematic camera |
| `R` | Reload / melee | `C` | Look behind |
| `Ctrl` | Run (holds the left stick in) | `M` | Map |
| `C` | Look behind | Arrow keys | D-pad (radio, headlights) |
| `M` | Map | | |
| Arrow keys | D-pad | | |
| `Alt` | Walk slowly | | |

## Tips

- **The game ignores your keys but the console menus react?** The game is still listening to the controller that started it. Press `Home` (the Xbox or PS button), select the game's tile and press `Shift` (A / Cross) to jump back in with XMouse. Or start the game from XMouse in the first place. XMouse reminds you of this with a short tip and a chime when the picture appears.
- **Less input lag on NVIDIA RTX:** turn on *GPU video decoding* (and *RTX Video upscaling*) under *More → Picture and connection*.
- **One-click play:** start XMouse with `--play` (for example from a desktop shortcut) and it connects straight to your last console in your last mode.
- **No sharper picture on RTX?** Turn on *Super Resolution* under *Graphics → Video* in the NVIDIA app, and check that *RTX Video upscaling* is *On* under *More → Picture and connection* in XMouse.
- **Picture stutters?** Plug the PC or the console into the router with a cable, and close other heavy downloads on the network.
- **Smoother motion:** under *More → Picture and connection → PC Play picture*, install and turn on *Frame generation (FSR)* (any modern graphics card) or *Frame generation (DLSS)* (GeForce RTX 40 or 50, with the player window in front). It adds about half a game frame of input lag, and a 60 fps game needs a 120 Hz screen. The key overlay is hidden while it runs.

## Plugins

Consoles other than Xbox come from plugins: separate programs XMouse starts and talks to. They live in `%APPDATA%\XMouse\config\plugins` (*More → Plugins folder* opens it). **PSNBridge** is the PlayStation plugin; it's open source under AGPL-3.0 in its [own repository](https://github.com/thedarkcr0w/xmouse-psnbridge).

## Build from source

You need [Rust](https://rustup.rs) (MSVC toolchain), the Visual Studio C++ build tools and CMake.

```powershell
cargo build --release
```

The app ends up in `target\release\xmouse.exe`. Run the tests with `cargo test`.

## Support

If XMouse makes your games better and you'd like to say thanks:

- ☕ **Ko-fi:** [ko-fi.com/darkcr0w](https://ko-fi.com/darkcr0w)
- ▶️ **YouTube:** [youtube.com/@thadarkcr0w](https://youtube.com/@thadarkcr0w)

## License

XMouse is licensed under [CC BY-NC-SA 4.0](LICENSE) (Creative Commons Attribution-NonCommercial-ShareAlike 4.0). The PSNBridge plugin has its own license (AGPL-3.0).

- **You can** use it, share it, change it and build on it.
- **You can't** sell it or anything made from it.
- **Anything you make from it** has to give credit and be shared under this same license, so it stays open.

## Not affiliated

XMouse is an independent project for playing your own console at home. It isn't made, endorsed or supported by Microsoft, Sony Interactive Entertainment, NVIDIA, AMD or any game publisher. AMD FSR frame generation and NVIDIA DLSS frame generation are not part of XMouse: their files are downloaded from AMD's and NVIDIA's own GitHub pages, only when you ask, under their makers' licences. AMD, FidelityFX and FSR are trademarks of Advanced Micro Devices, Inc. NVIDIA, GeForce RTX and DLSS are trademarks of NVIDIA Corporation. Xbox is a trademark of Microsoft. PlayStation, PS5, PS4 and DualSense are trademarks of Sony Interactive Entertainment. Game names and screenshots belong to their owners.
