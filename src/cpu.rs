use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::shared::{Mode, Shared, lower_thread_priority};

/// Length of one on/off slice in Half mode.
const SLICE: Duration = Duration::from_millis(20);
/// How often an idle worker checks whether it has been switched on.
const IDLE_POLL: Duration = Duration::from_millis(50);

/// Spawn one worker per logical core. Returns the number of workers.
pub fn spawn(shared: Arc<Shared>) -> usize {
    let count = thread::available_parallelism().map_or(1, |n| n.get());
    for i in 0..count {
        let shared = shared.clone();
        thread::Builder::new()
            .name(format!("cpu-heater-{i}"))
            .spawn(move || worker(&shared))
            .expect("failed to spawn CPU worker");
    }
    count
}

fn worker(shared: &Shared) {
    lower_thread_priority();
    let mut state = 1.000_1_f64;
    loop {
        match shared.cpu.get() {
            Mode::Off => thread::sleep(IDLE_POLL),
            Mode::Full => burn(&mut state, SLICE),
            Mode::Half => {
                burn(&mut state, SLICE);
                thread::sleep(SLICE);
            }
        }
    }
}

/// Keep the core busy with floating-point work for `duration`.
fn burn(state: &mut f64, duration: Duration) {
    let start = Instant::now();
    let mut x = *state;
    while start.elapsed() < duration {
        for i in 0..20_000 {
            x = (x * 1.000_000_1 + i as f64).sqrt().sin().abs() + 1.0;
        }
        x = black_box(x);
    }
    *state = x;
}
