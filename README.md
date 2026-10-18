<img src="icon.png" width="96" align="right" alt="">

# ChickenWM Notify

Notifications: toasts in a screen corner (Windows app notifications, tray balloons, `chickenctl notify`), a notification center with history, do-not-disturb with quiet hours and per-app rules.

Part of [ChickenWM](https://github.com/despale-code/chickenwm), a Hyprland-style tiling desktop for
Windows. Each program lives in its own repository; the ChickenWM repository ties them together
and has the full documentation and the installer.

This project is vibecoded: written with an AI assistant.

## Building

Windows 10 or 11 (64-bit) and Rust 1.85 or newer:

```
cargo build --release
```

It depends on [chickenwm-core](https://github.com/despale-code/chickenwm-core), which Cargo fetches from GitHub.

## License

MIT, see [LICENSE](LICENSE).
