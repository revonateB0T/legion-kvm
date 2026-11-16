use super::*;

#[test]
fn keyboard_and_mouse_buttons_keep_press_release_order_without_repeats() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    for (code, value) in [(28, 1), (28, 2), (28, 0), (0x110, 1), (0x110, 0)] {
        decoder.event(1, code, value, 1000, 800, &mut events);
    }
    assert_eq!(
        events,
        vec![
            Event::Key {
                code: 28,
                down: true
            },
            Event::Key {
                code: 28,
                down: false
            },
            Event::Button {
                code: 0x110,
                down: true
            },
            Event::Button {
                code: 0x110,
                down: false
            }
        ]
    );
}

#[test]
fn tablet_coordinates_precede_the_click_and_are_normalized_from_device_ranges() {
    let mut decoder = Decoder {
        absolute: Some([
            Axis {
                value: 0,
                min: 0,
                max: 32767,
            },
            Axis {
                value: 0,
                min: 0,
                max: 32767,
            },
        ]),
        ..Default::default()
    };
    let mut events = Vec::new();
    decoder.event(3, 0, 32767, 1000, 800, &mut events);
    decoder.event(3, 1, -1, 1000, 800, &mut events);
    decoder.event(1, 0x110, 1, 1000, 800, &mut events);
    decoder.event(0, 0, 0, 1000, 800, &mut events);
    assert_eq!(
        events,
        vec![
            Event::Absolute { x: 1.0, y: 0.0 },
            Event::Button {
                code: 0x110,
                down: true
            }
        ]
    );
    assert_eq!(
        Axis {
            value: i32::MAX,
            min: i32::MIN,
            max: i32::MAX
        }
        .normalized(),
        1.0
    );
}

#[test]
fn relative_mouse_emits_unclamped_motion_and_integrates_a_cursor() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();

    // The virtual cursor starts centered; +250/-200 on 1000x800 reaches
    // (0.75, 0.25), so UI hit-testing follows the raw mouse.
    decoder.event(2, 0, 250, 1000, 800, &mut events);
    decoder.event(2, 1, -200, 1000, 800, &mut events);
    decoder.event(0, 0, 0, 1000, 800, &mut events);

    // Beyond the right edge the integrated cursor clamps, but the relative
    // delta must stay unclamped so a locked FPS camera can keep rotating.
    decoder.event(2, 0, 500, 1000, 800, &mut events);
    decoder.event(1, 0x110, 1, 1000, 800, &mut events);
    decoder.event(0, 0, 0, 1000, 800, &mut events);

    assert_eq!(
        events,
        vec![
            Event::Relative {
                dx: 250.0,
                dy: -200.0,
                raw_dx: 250.0,
                raw_dy: -200.0,
            },
            Event::Absolute { x: 0.75, y: 0.25 },
            Event::Relative {
                dx: 500.0,
                dy: 0.0,
                raw_dx: 500.0,
                raw_dy: 0.0,
            },
            Event::Absolute { x: 1.0, y: 0.25 },
            Event::Button {
                code: 0x110,
                down: true
            },
        ]
    );
}

#[test]
fn relative_axes_accumulate_until_syn_or_a_click_flushes_them() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    decoder.event(2, 0, 3, 1024, 1024, &mut events);
    decoder.event(2, 1, 4, 1024, 1024, &mut events);
    decoder.event(2, 0, 5, 1024, 1024, &mut events);
    decoder.event(1, 0x110, 1, 1024, 1024, &mut events);
    assert_eq!(
        events,
        vec![
            Event::Relative {
                dx: 8.0,
                dy: 4.0,
                raw_dx: 8.0,
                raw_dy: 4.0,
            },
            Event::Absolute {
                x: 0.5078125,
                y: 0.50390625
            },
            Event::Button {
                code: 0x110,
                down: true
            },
        ]
    );
}

#[test]
fn dropped_packets_release_keys_and_ignore_the_incomplete_frame() {
    let mut decoder = Decoder::default();
    let mut events = Vec::new();
    decoder.event(1, 28, 1, 1000, 800, &mut events);
    decoder.event(2, 0, 100, 1000, 800, &mut events);
    decoder.event(0, 3, 0, 1000, 800, &mut events);
    decoder.event(1, 30, 1, 1000, 800, &mut events);
    decoder.event(0, 0, 0, 1000, 800, &mut events);
    decoder.event(1, 31, 1, 1000, 800, &mut events);
    assert_eq!(
        events,
        vec![
            Event::Key {
                code: 28,
                down: true
            },
            Event::Reset,
            Event::Key {
                code: 31,
                down: true
            }
        ]
    );
}

#[test]
fn evdev_batches_advertise_relative_support_for_pointer_lock() {
    let mut input = Input {
        devices: Vec::new(),
        started: false,
    };
    assert!(
        input.poll(1000, 800, 0).expect("batch").relative_supported,
        "guest evdev input can always synthesize relative motion"
    );
}
