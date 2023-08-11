use std::{io, sync::mpsc, thread, time};
use anyhow::Result;
use clap::Parser;
use evdev::{self, uinput};


/// Adds inertia to your touchpad
///
/// It doesn't alter the original touchpad events, but creates additional
/// virtual mouse device, which only genertes extra pointer movement events
/// when it has momentum.
#[derive(Parser, Debug)]
#[command(name = "InertPad")]
struct Args {
    /// Inertia drag coefficient (must be between 0.0 and 1.0)
    /// Affects inertial movement deceleration.
    #[arg(long, default_value_t = 0.15)]
    drag: f64,

    /// Scales velocity from raw touchpad units to virtual mouse units.
    /// Affects initial inertial movement speed.
    #[arg(long, default_value_t = 0.0075)]
    speed_factor: f64,

    /// Minimum touchpad pointer speed required to trigger inertial movement.
    /// Increase if a short tap causes unwanted pointer movement.
    /// Decrease if intentional swipes don't trigger inertial movement.
    #[arg(long, default_value_t = 1000.0)]
    speed_threshold: f64,

    /// Pointer position refresh rate during inertial movement.
    #[arg(long, default_value_t = 60.0)]
    refresh_rate: f64,

    /// Prevents inertial movement from multitouch by ignoring swipes
    /// for a specified number of milliseconds after multitouch release.
    #[arg(long, default_value_t = 500)]
    multitouch_cooldown: u64,
}


enum Message {
    StartMovement(f64, f64),
    StopMovement,
}


struct VirtualPointer {
    device: uinput::VirtualDevice,
}

impl VirtualPointer {
    fn new() -> Result<Self> {
        let device = uinput::VirtualDeviceBuilder::new()?
            .name("InertPad Virtual Mouse")
            .input_id(evdev::InputId::new(evdev::BusType::BUS_USB, 0x1234, 0x5678, 0))
            .with_keys(
                &[
                    evdev::Key::BTN_LEFT
                ].into_iter().collect::<evdev::AttributeSet<_>>())?
            .with_relative_axes(
                &[
                    evdev::RelativeAxisType::REL_X,
                    evdev::RelativeAxisType::REL_Y,
                ].into_iter().collect::<evdev::AttributeSet<_>>())?
            .build()?;
        Ok(Self { device })
    }

    fn set_position(&mut self, x: i32, y: i32) -> io::Result<()>{
        let events = [
            evdev::InputEvent::new(evdev::EventType::RELATIVE, evdev::RelativeAxisType::REL_X.0, x),
            evdev::InputEvent::new(evdev::EventType::RELATIVE, evdev::RelativeAxisType::REL_Y.0, y),
            evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, evdev::Synchronization::SYN_REPORT.0, 0),
        ];
        self.device.emit(&events)?;
        Ok(())
    }
}


struct Touchpad {
    device: evdev::Device,
}

impl Touchpad {
    fn default() -> Option<Self> {
        for (_path, device) in evdev::enumerate() {
            if let Some(keys) = device.supported_keys() {
                if keys.contains(evdev::Key::BTN_TOOL_FINGER)
                && keys.contains(evdev::Key::BTN_TOUCH) {
                    return Some(Self { device });
                }
            }
        }
        None
    }
}


fn capture_touchpad_input(
    mut touchpad: Touchpad,
    sender: mpsc::Sender<Message>,
    speed_threshold: f64,
    multitouch_cooldown: u64)
{
    let (mut vx, mut vy) = (0f64, 0f64);
    let (mut x, mut y) = (0, 0);
    let (mut prev_x, mut prev_y)  = (0, 0);
    let mut timestamp = time::SystemTime::UNIX_EPOCH;
    let mut prev_timestamp = time::SystemTime::UNIX_EPOCH;
    let mut multitouch_timestamp = time::SystemTime::UNIX_EPOCH;
    let multitouch_cooldown = time::Duration::from_millis(multitouch_cooldown);

    while let Ok(events) = touchpad.device.fetch_events() {
        for event in events {
            timestamp = event.timestamp();
            log::trace!("Touchpad event: {:?} = {}", event.kind(), event.value());
            match event.kind() {
                evdev::InputEventKind::AbsAxis(axis) => match axis {
                    evdev::AbsoluteAxisType::ABS_X => x = event.value(),
                    evdev::AbsoluteAxisType::ABS_Y => y = event.value(),
                    _ => (),
                }
                evdev::InputEventKind::Key(key) => match key {
                    evdev::Key::BTN_TOOL_FINGER => if event.value() == 1 {
                        let _ = sender.send(Message::StopMovement);
                    } else {
                        // Filter out multi-touch lift-off
                        if timestamp.duration_since(multitouch_timestamp).unwrap_or_default() < multitouch_cooldown {
                            continue;
                        }
                        let speed = (vx * vx + vy * vy).sqrt();
                        if speed >= speed_threshold {
                            let _ = sender.send(Message::StartMovement(vx, vy));
                        }
                    },
                    evdev::Key::BTN_TOOL_DOUBLETAP |
                    evdev::Key::BTN_TOOL_TRIPLETAP |
                    evdev::Key::BTN_TOOL_QUADTAP |
                    evdev::Key::BTN_TOOL_QUINTTAP => if event.value() == 1 {
                        let _ = sender.send(Message::StopMovement);
                    } else {
                        multitouch_timestamp = timestamp;
                    }
                    _ => {}
                }
                _ => {}
            }
        }
        if x != prev_x || y != prev_y {
            let dx = (x - prev_x) as f64;
            let dy = (y - prev_y) as f64;
            let dt = timestamp.duration_since(prev_timestamp).unwrap().as_secs_f64();
            (vx, vy) = (dx / dt, dy / dt);
            (prev_x, prev_y) = (x, y);
            prev_timestamp = timestamp;
            log::trace!("Velocity: ({:.02}, {:.02})", vx, vy);
        }
    }
}


fn emulate_mouse_output(
    mut vpointer: VirtualPointer,
    receiver: mpsc::Receiver<Message>,
    drag: f64,
    speed_factor: f64,
    refresh_rate: f64
) {
    let period = time::Duration::from_secs_f64(refresh_rate.recip());
    let mut is_moving = false;
    let (mut vx, mut vy) = (0f64, 0f64);
    let deceleration_factor = 1.0 - drag.clamp(0.0, 1.0);

    loop {
        if is_moving {
            if let Ok(Message::StopMovement) = receiver.recv_timeout(period) {
                log::debug!("Emulation: stop movement");
                is_moving = false;
                (vx, vy) = (0.0, 0.0);
            } else {
                let (x, y) = ((vx * speed_factor) as i32, (vy * speed_factor) as i32);
                if x == 0 && y == 0 {
                    is_moving = false;
                    (vx, vy) = (0.0, 0.0);
                } else {
                    (vx, vy) = (vx * deceleration_factor, vy * deceleration_factor);
                    log::trace!("Emulation: relative position = ({}, {})", x, y);
                    vpointer.set_position(x, y).unwrap();
                }
            }
        } else {
            if let Ok(Message::StartMovement(x, y)) = receiver.recv() {
                log::debug!("Emulation: start movement, velocity = ({:.02}, {:.02})", x, y);
                is_moving = true;
                (vx, vy) = (x, y);
            }
        }
    }
}


fn main() {
    env_logger::Builder::new()
        .filter_module("inertpad", log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let args = Args::parse();
    let (sender, receiver) = mpsc::channel();

    match Touchpad::default() {
        None => log::error!("Touchpad not found!"),
        Some(touchpad) => {
            log::info!("Found touchpad: {}", touchpad.device.name().unwrap_or_default());
            match VirtualPointer::new() {
                Err(e) => log::error!("Failed to create virtual pointer device: {}", e),
                Ok(vpointer) => {
                    log::info!("Virtual pointer device is created");
                    thread::spawn(move || {
                        capture_touchpad_input(touchpad, sender, args.speed_threshold, args.multitouch_cooldown);
                    });
                    emulate_mouse_output(vpointer, receiver, args.drag, args.speed_factor, args.refresh_rate);
                }
            }
        }
    }
}

