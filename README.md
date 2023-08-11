# InertPad

An experiment that adds inertia to touchpad on Linux.

It listens to raw (`evdev`) touchpad events, and creates additional virtual mouse device (via `uinput`) which kicks in to drive the pointer only during inertial movement. It doens't alter nor inhibits the original touchpad events.

## Build

1. Install [Rust](https://www.rust-lang.org/tools/install)
2. Build:
```
cargo build --release
```

## Usage

It requires root access. It needs read-access to `/dev/input/evdev*` for reading raw touchpad events, and write-access to `/dev/uinput` to create a virtual mouse device. For the former adding a user to `input` group is sufficient, but the latter requires root access anyway.

```
sudo ./target/release/inertpad
```

It can be configured with the following command-line arguments:

- `--drag <DRAG>` - Inertia drag coefficient (must be between 0.0 and 1.0) Affects inertial movement deceleration. Default: 0.15.
- `--speed-factor <SPEED_FACTOR>` - Scales velocity from raw touchpad units to virtual mouse units. Affects initial inertial movement speed. Default: 0.0075.
- `--speed-threshold <SPEED_THRESHOLD>` - Minimum touchpad pointer speed required to trigger inertial movement. Increase if a short tap causes unwanted pointer movement. Decrease if intentional swipes don't trigger inertial movement. Default: 1000.
- `--refresh-rate <REFRESH_RATE>` - Pointer position refresh rate during inertial movement. Default: 60.
- `--multitouch-cooldown <MULTITOUCH_COOLDOWN>` - Prevents inertial movement from multitouch by ignoring swipes for a specified number of milliseconds after multitouch release. Default: 500.
