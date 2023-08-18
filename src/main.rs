use anyhow::Result;
use clap::Parser;
use evdev::{self, uinput};
use std::{io, sync::mpsc, thread, time};

/// Adds inertia to your touchpad
///
/// It doesn't alter the original touchpad events, but creates additional
/// virtual mouse device, which only genertes extra pointer movement events
/// when it has momentum.
#[derive(Parser, Debug)]
#[command(name = "InertPad")]
struct Args {
    /// Pointer inertia drag coefficient (must be between 0.0 and 1.0)
    /// Affects inertial pointer movement deceleration.
    #[arg(long, default_value_t = 0.15)]
    pointer_drag: f64,

    /// Scales velocity from raw touchpad units to virtual mouse position units.
    /// Affects initial inertial pointer movement speed.
    #[arg(long, default_value_t = 0.0075)]
    pointer_factor: f64,

    /// Minimum touchpad pointer speed required to trigger inertial movement.
    /// Increase if a short tap causes unwanted pointer movement.
    /// Decrease if intentional swipes don't trigger inertial movement.
    #[arg(long, default_value_t = 2000.0)]
    pointer_threshold: f64,

    /// Disable pointer momentum
    #[arg(long)]
    pointer_disabled: bool,

    /// Scroll inertia drag coefficient (must be between 0.0 and 1.0)
    /// Affects inertial scroll deceleration.
    #[arg(long, default_value_t = 0.1)]
    scroll_drag: f64,

    /// Scales velocity from raw touchpad units to virtual mouse wheel units.
    /// Affects initial inertial scroll speed.
    #[arg(long, default_value_t = 0.05)]
    scroll_factor: f64,

    /// Minimum touchpad scroll speed required to trigger inertial scroll.
    #[arg(long, default_value_t = 100.0)]
    scroll_threshold: f64,

    /// Invert vertical scroll direction
    #[arg(long)]
    scroll_invert: bool,

    /// Disable scroll momentum
    #[arg(long)]
    scroll_disabled: bool,

    /// Pointer position refresh rate during inertial movement.
    #[arg(long, default_value_t = 60.0)]
    refresh_rate: f64,
}

#[derive(Debug)]
enum MomentumMessage {
    PointerMomentum((f64, f64)),
    PointerStop,
    ScrollMomentum((f64, f64)),
    ScrollStop,
}

/// Emulates mouse device (via uinput) which performs inertial pointer movement
struct VirtualMouse {
    device: uinput::VirtualDevice,
}

impl VirtualMouse {
    fn new() -> Result<Self> {
        use evdev::{AttributeSet, BusType, InputId, Key, RelativeAxisType};
        let device = uinput::VirtualDeviceBuilder::new()?
            .name("InertPad Virtual Mouse")
            .input_id(InputId::new(BusType::BUS_USB, 0x1234, 0x5678, 0))
            .with_keys(&[Key::BTN_LEFT].into_iter().collect::<AttributeSet<_>>())?
            .with_relative_axes(
                &[
                    RelativeAxisType::REL_X,
                    RelativeAxisType::REL_Y,
                    RelativeAxisType::REL_WHEEL_HI_RES,
                    RelativeAxisType::REL_HWHEEL_HI_RES,
                ]
                .into_iter()
                .collect::<AttributeSet<_>>(),
            )?
            .build()?;
        Ok(Self { device })
    }

    fn set_position(&mut self, x: i32, y: i32) -> io::Result<()> {
        use evdev::{EventType, InputEvent, RelativeAxisType, Synchronization};
        let events = [
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, x),
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, y),
            InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0),
        ];
        self.device.emit(&events)?;
        Ok(())
    }

    fn scroll_vertical(&mut self, value: i32) -> io::Result<()> {
        use evdev::{EventType, InputEvent, RelativeAxisType, Synchronization};
        let events = [
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_WHEEL_HI_RES.0, value),
            InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0),
        ];
        self.device.emit(&events)?;
        Ok(())
    }

    fn scroll_horizontal(&mut self, value: i32) -> io::Result<()> {
        use evdev::{EventType, InputEvent, RelativeAxisType, Synchronization};
        let events = [
            InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_HWHEEL_HI_RES.0, value),
            InputEvent::new(EventType::SYNCHRONIZATION, Synchronization::SYN_REPORT.0, 0),
        ];
        self.device.emit(&events)?;
        Ok(())
    }

    fn run_emulation(
        &mut self,
        receiver: mpsc::Receiver<MomentumMessage>,
        pointer_drag: f64,
        pointer_scaler: f64,
        scroll_drag: f64,
        scroll_scaler: f64,
        scroll_invert: bool,
        refresh_rate: f64,
    ) {
        let period = time::Duration::from_secs_f64(refresh_rate.recip());
        let mut pointer_moving = false;
        let mut scrolling = false;
        let mut pvel = (0f64, 0f64); // Pointer velocity
        let mut svel = (0f64, 0f64); // Scroll velocity
        let pointer_deceleration = 1.0 - pointer_drag.clamp(0.0, 1.0);
        let scroll_deceleration = 1.0 - scroll_drag.clamp(0.0, 1.0);

        loop {
            if pointer_moving || scrolling {
                match receiver.recv_timeout(period) {
                    Ok(MomentumMessage::PointerStop) => {
                        log::debug!("Emulation: stop pointer");
                        pointer_moving = false;
                        pvel = (0.0, 0.0);
                    }
                    Ok(MomentumMessage::ScrollStop) => {
                        log::debug!("Emulation: stop scrolling");
                        scrolling = false;
                        svel = (0.0, 0.0);
                    }
                    Ok(event) => {
                        log::warn!("Emulation: unexpected event: {:?}", event);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if pointer_moving {
                            let x = (pvel.0 * pointer_scaler) as i32;
                            let y = (pvel.1 * pointer_scaler) as i32;
                            if x == 0 && y == 0 {
                                pointer_moving = false;
                                pvel = (0.0, 0.0);
                            } else {
                                pvel = (pvel.0 * pointer_deceleration, pvel.1 * pointer_deceleration);
                                log::trace!("Emulation: relative position = ({}, {})", x, y);
                                self.set_position(x, y).unwrap();
                            }
                        }
                        if scrolling {
                            let x = (svel.0 * scroll_scaler) as i32;
                            let y = (svel.1 * scroll_scaler) as i32;
                            if x == 0 && y == 0 {
                                scrolling = false;
                                svel = (0.0, 0.0);
                            } else {
                                svel = (svel.0 * scroll_deceleration, svel.1 * scroll_deceleration);

                                if x.abs() <= y.abs() {
                                    let y = if scroll_invert { -y } else { y };
                                    log::trace!("Emulation: scroll vertical = {}", y);
                                    self.scroll_vertical(y).unwrap();
                                } else {
                                    log::trace!("Emulation: scroll horizontal = {}", -x);
                                    self.scroll_horizontal(-x).unwrap();
                                }
                            }
                        }
                    }
                    Err(error) => {
                        log::error!("Emulation: {}", error);
                    }
                }
            } else {
                match receiver.recv() {
                    Ok(MomentumMessage::PointerMomentum((vx, vy))) => {
                        log::debug!("Emulation: movement velocity = ({:.02}, {:.02})", vx, vy);
                        pointer_moving = true;
                        pvel = (vx, vy);
                    }
                    Ok(MomentumMessage::ScrollMomentum((vx, vy))) => {
                        log::debug!("Emulation: scroll velocity = ({:.02}, {:.02})", vx, vy);
                        scrolling = true;
                        svel = (vx, vy);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::error!("Emulation: {}", error);
                    }
                }
            }
        }
    }
}

/// Captures raw evdev touchpad events and forwards
struct Touchpad {
    device: evdev::Device,
}

impl Touchpad {
    fn default() -> Option<Self> {
        for (_path, device) in evdev::enumerate() {
            if let Some(keys) = device.supported_keys() {
                if keys.contains(evdev::Key::BTN_TOOL_FINGER)
                    && keys.contains(evdev::Key::BTN_TOUCH)
                {
                    return Some(Self { device });
                }
            }
        }
        None
    }

    fn run_capture(
        &mut self,
        sender: mpsc::Sender<MomentumMessage>,
        pointer_enabled: bool,
        scroll_enabled: bool,
        pointer_threshold: f64,
        scroll_threshold: f64,
    ) {
        use evdev::{AbsoluteAxisType, InputEventKind, Key};
        const MT_TIMEOUT: time::Duration = time::Duration::from_millis(500);
        let mut touch_count = 0;
        let mut vel = (0f64, 0f64);
        let mut pos = (0, 0);
        let mut prev_pos = (0, 0);
        let mut mt_slot = 0;
        let mut mt_pos = [(0, 0); 2];
        let mut timestamp = time::SystemTime::UNIX_EPOCH;
        let mut prev_timestamp = time::SystemTime::UNIX_EPOCH;
        let mut dt_timestamp = time::SystemTime::UNIX_EPOCH;
        let mut mt_timestamp = time::SystemTime::UNIX_EPOCH;

        while let Ok(events) = self.device.fetch_events() {
            for event in events {
                timestamp = event.timestamp();
                log::trace!("Touchpad event: {:?} = {}", event.kind(), event.value());
                match event.kind() {
                    InputEventKind::AbsAxis(axis) => match axis {
                        AbsoluteAxisType::ABS_X => pos.0 = event.value(),
                        AbsoluteAxisType::ABS_Y => pos.1 = event.value(),
                        AbsoluteAxisType::ABS_MT_POSITION_X => {
                            if mt_slot < mt_pos.len() {
                                mt_pos[mt_slot].0 = event.value();
                            }
                        }
                        AbsoluteAxisType::ABS_MT_POSITION_Y => {
                            if mt_slot < mt_pos.len() {
                                mt_pos[mt_slot].1 = event.value();
                            }
                        }
                        AbsoluteAxisType::ABS_MT_SLOT => mt_slot = event.value() as usize,
                        _ => {}
                    },
                    InputEventKind::Key(key) => match key {
                        Key::BTN_TOUCH => {
                            if event.value() == 1 {
                                vel = (0.0, 0.0);
                                prev_pos = pos;
                                if pointer_enabled {
                                    let _ = sender.send(MomentumMessage::PointerStop);
                                }
                                if scroll_enabled {
                                    let _ = sender.send(MomentumMessage::ScrollStop);
                                }
                            } else {
                                let speed = (vel.0 * vel.0 + vel.1 * vel.1).sqrt();

                                if touch_count == 2
                                    && speed >= scroll_threshold
                                    && timestamp > mt_timestamp + MT_TIMEOUT
                                    && scroll_enabled
                                {
                                    let _ = sender.send(MomentumMessage::ScrollMomentum(vel));
                                } else if touch_count == 1
                                    && speed >= pointer_threshold
                                    && timestamp > dt_timestamp + MT_TIMEOUT
                                    && timestamp > mt_timestamp + MT_TIMEOUT
                                    && pointer_enabled
                                {
                                    let _ = sender.send(MomentumMessage::PointerMomentum(vel));
                                }
                                touch_count = 0;
                            }
                        }
                        Key::BTN_TOOL_FINGER
                        | Key::BTN_TOOL_DOUBLETAP
                        | Key::BTN_TOOL_TRIPLETAP
                        | Key::BTN_TOOL_QUADTAP
                        | Key::BTN_TOOL_QUINTTAP => {
                            if event.value() == 1 {
                                vel = (0.0, 0.0);
                                prev_pos = pos;
                                touch_count = match key {
                                    Key::BTN_TOOL_FINGER => 1,
                                    Key::BTN_TOOL_DOUBLETAP => 2,
                                    Key::BTN_TOOL_TRIPLETAP => 3,
                                    Key::BTN_TOOL_QUADTAP => 4,
                                    Key::BTN_TOOL_QUINTTAP => 5,
                                    _ => unreachable!(),
                                };
                            } else {
                                match key {
                                    Key::BTN_TOOL_FINGER => (),
                                    Key::BTN_TOOL_DOUBLETAP => dt_timestamp = timestamp,
                                    _ => mt_timestamp = timestamp,
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if touch_count == 2 {
                let x = (mt_pos[0].0 + mt_pos[1].0) / 2;
                let y = (mt_pos[0].1 + mt_pos[1].1) / 2;
                pos = (x, y)
            }
            if touch_count != 0 && pos != prev_pos {
                let dx = (pos.0 - prev_pos.0) as f64;
                let dy = (pos.1 - prev_pos.1) as f64;
                let dt = timestamp
                    .duration_since(prev_timestamp)
                    .unwrap()
                    .as_secs_f64();
                vel = (dx / dt, dy / dt);
                prev_pos = pos;
                prev_timestamp = timestamp;
                log::trace!("Velocity: ({:.02}, {:.02})", vel.0, vel.1);
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
        Some(mut touchpad) => {
            log::info!(
                "Found touchpad: {}",
                touchpad.device.name().unwrap_or_default()
            );
            match VirtualMouse::new() {
                Err(e) => log::error!("Failed to create virtual mouse device: {}", e),
                Ok(mut vmouse) => {
                    log::info!("Virtual mouse device is created");
                    thread::spawn(move || {
                        touchpad.run_capture(
                            sender,
                            !args.pointer_disabled,
                            !args.scroll_disabled,
                            args.pointer_threshold,
                            args.scroll_threshold,
                        );
                    });
                    vmouse.run_emulation(
                        receiver,
                        args.pointer_drag,
                        args.pointer_factor,
                        args.scroll_drag,
                        args.scroll_factor,
                        args.scroll_invert,
                        args.refresh_rate,
                    );
                }
            }
        }
    }
}
