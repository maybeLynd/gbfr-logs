# gbfr-logs

[![GitHub Release](https://img.shields.io/github/v/release/false-spring/gbfr-logs)](https://github.com/false-spring/gbfr-logs/releases)
[![GitHub Downloads](https://img.shields.io/github/downloads/false-spring/gbfr-logs/total)](https://github.com/false-spring/gbfr-logs/releases)
[![Discord](https://img.shields.io/discord/1218833963032776774?style=flat&label=discord&color=7289da)](https://discord.gg/GR4r9zrqJj)
[![GitHub License](https://img.shields.io/github/license/false-spring/gbfr-logs)](./LICENSE)

<a href="https://www.buymeacoffee.com/false.spring" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/default-orange.png" alt="Buy Me A Coffee" height="41" width="174"></a>

Overlay DPS parser/meter for Granblue Fantasy: Relink, based initially on the reverse engineering work from [naoouo/GBFR-ACT](https://github.com/nyaoouo/GBFR-ACT).

## How to install

- Go to [Releases](https://github.com/false-spring/gbfr-logs/releases)
- Download the latest .msi installer and run it.
- Open GBFR Logs after the game is already running.

## Screenshots

### DPS Overlay

<img width="715" height="71" alt="meter" src="https://github.com/user-attachments/assets/89e2514c-9b46-4f68-a764-7ad7d4d9819b" />


### Skill Tracking (with skill grouping)

<img width="711" height="262" alt="skill-tracking" src="https://github.com/user-attachments/assets/b200e8e9-850d-448b-9c2e-0fd054c9509d" />


### Historical Logs (with filtering)

![Logs](./docs/screenshots/log-history.png)

### DPS Charts

![Charts](./docs/screenshots/charting.png)

### SBA Tracking

![SBA Tracking](./docs/screenshots/sba-tracking.png)

### Equipment Tracking

<img width="593" height="698" alt="equipment-tracking" src="https://github.com/user-attachments/assets/7808f62d-efb4-42a3-8d30-a322f40a9462" />


### Master Traits
<img width="710" height="539" alt="master-traits" src="https://github.com/user-attachments/assets/c01f08e6-c8ef-433e-890b-20cd8685e590" />

### Multi-language Support

![Simplified Chinese](./docs/screenshots/simplified-chinese.png)

## Settings / Customization

![Settings](./docs/screenshots/settings.png)

## Frequently Asked Questions

> Q: I closed the meter, but it's still running?

When you close the windows, GBFR Logs continues to run in your task tray in the bottom right of your desktop.

This task tray functionality is meant to give you more options for customizing:

- This lets you close the logs window, but be able to reopen it again later.
- You can toggle clickthrough of the overlay as well.

> Q: The meter isn't updating or displaying anything.

Try running the program after the game has been launched. Be sure to run the program as admin.

> Q: The application is not working / launching.

GBFR Logs uses your built-in Microsoft Edge Webview2 Runtime to run the application. This keeps the app relatively small as we don't have to package in a browser.

However, you may have an out-of-date or missing "Webview2 Runtime":

- Install the latest one from Microsoft: https://developer.microsoft.com/en-us/microsoft-edge/webview2/?form=MA13LH#download (Evergreen Bootstrapper should work here)

> Q: Is this safe? My antivirus is marking the installation as a virus / malware.

As always, this is up to you to trust GBFR Logs. The program can trigger false positive flags. There are reasons why it can give such alerts:

- GBFR Logs does code DLL injection into the running game process which can look like a virus-like program.
- GBFR Logs reads game memory and modifies game code at runtime in order to receive parser data.
- I recommend adding an exception / whitelisting for the installation folder so that your anti-virus does not delete it while your game is running, but you may not need to do so if you haven't ran into this issue.

See [how to add an exclusion to Windows Defender](https://support.microsoft.com/en-us/windows/add-an-exclusion-to-windows-security-811816c0-4dfd-af4a-47e4-c301afe13b26).

> Q: How do I update?

Launching the application will automatically check for new updates!

Same as with installing, you can download the [latest release](https://github.com/false-spring/gbfr-logs/releases) and run the installer again and it will update over your old installation.

> Q: How do I uninstall?

You can uninstall GBFR Logs the normal way through the Control Panel or by running the uninstall script in the folder where you installed it to. You may also want to remove these folders.

- `%AppData%\gbfr-logs`

> Q: How do I add/edit my language?

Read [src-tauri/lang/README.md](./src-tauri/lang/README.md) for more information on how to add/edit language support!

> Q: My issue isn't listed here, or I have a suggestion.

Feel free to create a [new GitHub issue](https://github.com/false-spring/gbfr-logs/issues) or [join the Discord server](https://discord.gg/GR4r9zrqJj).

## For Developers

- Install nightly Rust ([rustup.rs](https://rustup.rs/)) + [Node.js](https://nodejs.org/en/download).
- Install NPM dependencies with `npm install`
- `npm run tauri dev`

## Under the hood

This project is split up into a few subprojects:

- `src-hook/` - Library that is injected into the game that broadcasts essential damage events.
- `src-tauri/` - The Tauri Rust backend that communicates with the hooked process and does parsing.
- `protocol/` - Defines the message protocol used by hook + back-end.
- `src/` - The JS front-end used by the Tauri web app

## Credits

This project would not have been possible without the following folks:

- [nyaoouo/GBFR-ACT](https://github.com/nyaoouo/GBFR-ACT) for the original reverse engineering work.
- [Harkain](https://github.com/Harkains) for their work on formatting and translating skills to friendly English names.
- [false-spring](https://github.com/false-spring/gbfr-logs) for their work on the original repo for gbfr-logs from which expansion support was derived.

## Disclaimer

Please keep in mind that this tool is meant to improve the experience that Cygames has provided us and is not meant to cause them or anyone other players damage. GBFR Logs modifies your running game client and is not guaranteed to work after game patches, in which case you may experience instability or crashes.
