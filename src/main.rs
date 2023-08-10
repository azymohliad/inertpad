use std::{
    fs,
    io,
    thread,
    time,
    sync::mpsc,
    os::unix::prelude::{FileTypeExt, OpenOptionsExt},
};
use evdev;
use input_linux as inx;
use nix::libc::O_NONBLOCK;

enum Message {
    StartMovement(f64, f64),
    StopMovement,
}


struct VirtualPointer {
    uinput: inx::UInputHandle<fs::File>,
}

impl VirtualPointer {
    fn new() -> io::Result<Self> {
        let uinput_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NONBLOCK)
            .open("/dev/uinput")?;
        let uinput = inx::UInputHandle::new(uinput_file);
        uinput.set_evbit(inx::EventKind::Key)?;
        uinput.set_keybit(inx::Key::ButtonLeft)?;
        uinput.set_evbit(inx::EventKind::Relative)?;
        uinput.set_relbit(inx::RelativeAxis::X)?;
        uinput.set_relbit(inx::RelativeAxis::Y)?;

        let input_id = inx::InputId {
            bustype: input_linux::sys::BUS_USB,
            vendor: 0x1234,
            product: 0x5678,
            version: 0,
        };
        let device_name = b"InertiaPad Virtual Mouse";
        uinput.create(&input_id, device_name, 0, &[])?;
        Ok(Self { uinput })
    }

    fn set_position(&self, x: i32, y: i32) -> io::Result<()>{
        const ZERO: inx::EventTime = inx::EventTime::new(0, 0);
        let events = [
            *inx::InputEvent::from(inx::RelativeEvent::new(ZERO, inx::RelativeAxis::X, x)).as_raw(),
            *inx::InputEvent::from(inx::RelativeEvent::new(ZERO, inx::RelativeAxis::Y, y)).as_raw(),
            *inx::InputEvent::from(inx::SynchronizeEvent::new(ZERO, inx::SynchronizeKind::Report, 0)).as_raw(),
        ];
        self.uinput.write(&events)?;
        Ok(())
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        if let Err(error) = self.uinput.dev_destroy() {
            eprintln!("Failed to destroy virtual pointer device: {}", error);
        }
    }
}


fn get_touchpad() -> Option<evdev::Device> {
    for entry in fs::read_dir("/dev/input/").ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_char_device()
        && entry.file_name().to_str().unwrap().starts_with("event") {
            let device = evdev::Device::open(entry.path()).ok()?;
            let keys = device.supported_keys()?;
            if keys.contains(evdev::Key::BTN_TOOL_FINGER)
            && keys.contains(evdev::Key::BTN_TOUCH) {
                return Some(device);
            }
        }
    }
    None
}


fn capture_touchpad_input(
    mut touchpad: evdev::Device,
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

    while let Ok(events) = touchpad.fetch_events() {
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
                        if timestamp.duration_since(multitouch_timestamp).unwrap_or(multitouch_timeout) < multitouch_timeout {
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
    vpointer: VirtualPointer,
    receiver: mpsc::Receiver<Message>,
    drag: f64,
    scale: f64,
) {
    let min_speed = 1f64;
    let period = time::Duration::from_millis(15);
    // let drag = 1.0 - (1.0 - drag) / period.as_secs_f64();
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
    let speed_threshold = 1000.0;
    let (sender, receiver) = mpsc::channel();

    match get_touchpad() {
        None => eprintln!("Touchpad not found!"),
        Some(touchpad) => {
            match VirtualPointer::new() {
                Err(e) => eprintln!("Failed to create virtual pointer device: {}", e),
                Ok(vpointer) => {
                    thread::spawn(move || {
                        capture_touchpad_input(touchpad, sender, speed_threshold);
                    });
                    emulate_mouse_output(vpointer, receiver, drag, scale);
                }
            }
        }
    }
}

