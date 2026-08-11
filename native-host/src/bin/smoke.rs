use std::error::Error;
use std::thread;
use std::time::Duration;

use holodori_native_host::protocol::{
    ACTION_DOWN, ACTION_MOVE, ACTION_UP, CONTACT_FLAG_INSIDE, CONTACT_FLAG_TIP, Contact,
    FRAME_FLAG_LOCKED, TouchFrame,
};
use holodori_native_host::touch::{PROBE_WINDOW_TITLE, TouchInjector, TouchTarget};

fn main() {
    if let Err(error) = run() {
        eprintln!("touch smoke test: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let target = TouchTarget::from_window_title(PROBE_WINDOW_TITLE)?;
    println!(
        "target {},{} {}x{}",
        target.left, target.top, target.width, target.height
    );
    let mut injector = TouchInjector::new(target)?;
    let session_id = 0x534d_4f4b_4554_4f55;
    let mut sequence = 0;

    injector.accept(&frame(session_id, sequence, ACTION_DOWN, 0.04, true))?;
    println!("DOWN accepted");
    sequence += 1;
    for step in 1..=48 {
        injector.accept(&frame(
            session_id,
            sequence,
            ACTION_MOVE,
            0.04 + step as f32 * 0.92 / 48.0,
            true,
        ))?;
        sequence += 1;
        thread::sleep(Duration::from_millis(2));
    }
    injector.accept(&frame(session_id, sequence, ACTION_UP, 0.96, false))?;
    println!("OK: Windows accepted DOWN + 48 UPDATE + UP touch frames");
    Ok(())
}

fn frame(session_id: u64, sequence: u64, action: u8, x: f32, touching: bool) -> TouchFrame {
    TouchFrame {
        session_id,
        sequence,
        phone_event_nanos: 0,
        phone_callback_nanos: 0,
        phone_send_nanos: 0,
        echo_host_send_nanos: 0,
        phone_control_receive_nanos: 0,
        action,
        action_pointer_id: 0,
        flags: FRAME_FLAG_LOCKED,
        contacts: vec![Contact {
            pointer_id: 0,
            flags: CONTACT_FLAG_INSIDE | if touching { CONTACT_FLAG_TIP } else { 0 },
            x,
            y: 0.55,
            pressure: 0.5,
            touch_major: 0.04,
        }],
    }
}
