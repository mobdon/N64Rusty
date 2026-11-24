//! Whole runs with no window: replay, record and the debug port.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::thread;

use n64rusty_host::pad::button;
use n64rusty_host::{run_headless, run_windowed, Guest, Pad, Pads, RunConfig, Session, Tick};

/// Stands in for a game: remembers what it was given.
#[derive(Default)]
struct Log(Vec<(u64, Pads)>);

impl Guest for Log {
    fn frame(&mut self, frame: u64, pads: &Pads) {
        self.0.push((frame, *pads));
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("n64rusty-host-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn replay_drives_the_game_and_records_identically() {
    let script = temp("in.txt");
    std::fs::write(
        &script,
        "0 1 - 0 0\n10 1 A 80 0\n12 1 - 0 0\n15 3 START 0 -80\n",
    )
    .unwrap();
    let record = temp("out.txt");
    let cfg = RunConfig::parse(
        &format!("frames = 20\nreplay = {:?}\nrecord = {:?}", script, record),
        "".as_ref(),
    )
    .unwrap();

    let log = run_headless(Log::default(), &cfg).unwrap();
    assert_eq!(log.0.len(), 20);
    assert_eq!(log.0[9].1[0], Pad::default());
    assert_eq!(
        log.0[11].1[0],
        Pad {
            buttons: button::A,
            x: 80,
            y: 0
        }
    );
    assert_eq!(log.0[12].1[0], Pad::default());
    assert_eq!(log.0[19].1[2].buttons, button::START);

    // Replaying the recording gives the same frames.
    let cfg2 = RunConfig {
        replay: Some(record),
        record: None,
        ..cfg
    };
    assert_eq!(run_headless(Log::default(), &cfg2).unwrap().0, log.0);
}

#[test]
fn debug_port_steps_one_frame_at_a_time() {
    let cfg = RunConfig::parse("frames = 100\ndebug_port = \"127.0.0.1:0\"", "".as_ref()).unwrap();
    let mut session = Session::new(Log::default(), &cfg).unwrap();
    let addr = session.debug_addr().unwrap();

    let client = thread::spawn(move || {
        let stream = TcpStream::connect(addr).unwrap();
        let mut out = stream.try_clone().unwrap();
        let mut lines = BufReader::new(stream).lines();
        let mut ask = |cmd: &[u8]| {
            out.write_all(cmd).unwrap();
            out.write_all(b"\n").unwrap();
            lines.next().unwrap().unwrap()
        };
        assert_eq!(ask(b"frame"), "ok 0");
        assert_eq!(ask(b"step"), "ok 1");
        assert_eq!(ask(b"pad 2 B+L -5 7"), "ok");
        assert_eq!(ask(b"step 3"), "ok 4");
        assert_eq!(ask(b"release 2"), "ok");
        assert_eq!(ask(b"step"), "ok 5");
        assert!(ask(b"pad 9 - 0 0").starts_with("err"));
        assert!(ask(b"jump").starts_with("err"));
        assert!(ask(b"st\xffep").starts_with("err"));
        assert_eq!(ask(b"frame"), "ok 5"); // the connection survived
        assert_eq!(ask(b"quit"), "ok 5");
    });

    // A paused session blocks on the port, so this loop runs only as fast as
    // the client steps it.
    while session.tick(&Pads::default(), true) != Tick::Done {}
    client.join().unwrap();

    let log = session.finish().unwrap().0;
    assert_eq!(log.len(), 5);
    assert_eq!(log[0].1[1], Pad::default());
    for (_, pads) in &log[1..4] {
        assert_eq!(
            pads[1],
            Pad {
                buttons: button::B | button::L,
                x: -5,
                y: 7
            }
        );
    }
    assert_eq!(log[4].1[1], Pad::default());
}

#[test]
fn headless_config_never_opens_a_window() {
    // A window would need an event loop; a headless run must not make one.
    let cfg = RunConfig::parse("headless = true\nframes = 3", "".as_ref()).unwrap();
    struct NoWindow(u64);
    impl Guest for NoWindow {
        fn frame(&mut self, _: u64, _: &Pads) {
            self.0 += 1;
        }
        fn attach_window(&mut self, _: std::sync::Arc<winit::window::Window>) {
            panic!("a headless run opened a window");
        }
    }
    let guest = run_windowed(NoWindow(0), &cfg).unwrap();
    assert_eq!(guest.0, 3);
}
