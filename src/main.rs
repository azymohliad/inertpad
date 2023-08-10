use std::{
    fs,
    io,
    sync::mpsc,
    thread,
    time,
    os::unix::prelude::FileTypeExt,
};
use anyhow::Result;
use evdev::{self, uinput};


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
        for entry in fs::read_dir("/dev/input/").ok()? {
            if let Ok(entry) = entry {
                if let Ok(touchpad) = Self::from_devinput_entry(entry) {
                    return Some(touchpad);
                }
            }
        }
        None
    }

    fn from_devinput_entry(entry: fs::DirEntry) -> Result<Self> {
        if entry.file_type()?.is_char_device()
        && entry.file_name().to_str().unwrap().starts_with("event") {
            let device = evdev::Device::open(entry.path())?;
            if let Some(keys) = device.supported_keys() {
                if keys.contains(evdev::Key::BTN_TOOL_FINGER)
                && keys.contains(evdev::Key::BTN_TOUCH) {
                    return Ok(Self { device });
                }
            }
        }
        anyhow::bail!("Not a touchpad")
    }
}


fn capture_touchpad_input(
    mut touchpad: Touchpad,
    sender: mpsc::Sender<Message>,
    speed_threshold: f64)
{
    let (mut vx, mut vy) = (0f64, 0f64);
    let (mut x, mut y) = (0, 0);
    let (mut prev_x, mut prev_y)  = (0, 0);
    let mut timestamp = time::SystemTime::UNIX_EPOCH;
    let mut prev_timestamp = time::SystemTime::UNIX_EPOCH;
    let mut multitouch_timestamp = time::SystemTime::UNIX_EPOCH;
    let multitouch_timeout = time::Duration::from_millis(500);

    while let Ok(events) = touchpad.device.fetch_events() {
        for event in events {
            timestamp = event.timestamp();
            log::trace!("Touchpad event: {:?}", event.kind());
            match event.kind() {
                evdev::InputEventKind::AbsAxis(axis) => match axis {
                    evdev::AbsoluteAxisType::ABS_X => x = event.value(),
                    evdev::AbsoluteAxisType::ABS_Y => y = event.value(),
                    _ => (),
                }
                evdev::InputEventKind::Key(key) => match key {
                    evdev::Key::BTN_TOOL_FINGER => if event.value() == 1 {
                        log::debug!("Finger Down");
                        let _ = sender.send(Message::StopMovement);
                    } else {
                        if timestamp.duration_since(multitouch_timestamp).unwrap_or_default() < multitouch_timeout {
                            continue;
                        }
                        let speed = (vx * vx + vy * vy).sqrt();
                        if speed >= speed_threshold {
                            log::debug!("Finger Up, velocity = ({:.02}, {:.02})", vx, vy);
                            let _ = sender.send(Message::StartMovement(vx, vy));
                        } else {
                            log::debug!("Finger Up");
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
        let dx = (x - prev_x) as f64;
        let dy = (y - prev_y) as f64;
        let dt = timestamp.duration_since(prev_timestamp).unwrap().as_secs_f64();
        (vx, vy) = (dx / dt, dy / dt);
        (prev_x, prev_y) = (x, y);
        prev_timestamp = timestamp;
    }
}


fn emulate_mouse_output(
    mut vpointer: VirtualPointer,
    receiver: mpsc::Receiver<Message>,
    drag: f64,
    scale: f64,
) {
    let min_speed = 1f64;
    let period = time::Duration::from_millis(15);
    let mut is_moving = false;
    let (mut vx, mut vy) = (0f64, 0f64);

    loop {
        if is_moving {
            if let Ok(Message::StopMovement) = receiver.recv_timeout(period) {
                is_moving = false;
                (vx, vy) = (0.0, 0.0);
            } else {
                let (x, y) = ((vx * scale) as i32, (vy * scale) as i32);
                (vx, vy) = (vx * drag, vy * drag);
                let speed = (vx * vx + vy * vy).sqrt();
                if speed < min_speed {
                    is_moving = false;
                    (vx, vy) = (0.0, 0.0);
                } else {
                    log::trace!("Emulation: relative position = ({}, {})", x, y);
                    vpointer.set_position(x, y).unwrap();
                }
            }
        } else {
            if let Ok(Message::StartMovement(x, y)) = receiver.recv() {
                is_moving = true;
                (vx, vy) = (x, y);
            }
        }
    }
}


fn main() {
    env_logger::init();

    let scale = 0.01;
    let drag = 0.8;
    let speed_threshold = 200.0;
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
                        capture_touchpad_input(touchpad, sender, speed_threshold);
                    });
                    emulate_mouse_output(vpointer, receiver, drag, scale);
                }
            }
        }
    }
}

