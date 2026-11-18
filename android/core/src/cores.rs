// Copyright (c) tyetladd | MIT License | https://github.com/tyetladd/wavo-app

//! The fast cores of a phone. A phone's CPU has clusters of different speed (a Pixel 7 Pro: two
//! Cortex-X1, two A78, four A55), and wavo starts a thread for every core the calling thread may
//! run on: a share of the work on an A55 takes several times longer than on an X1. So the engine's
//! thread is kept off the slowest cluster before wavo counts the cores; the threads it starts
//! inherit that.

/// The cores to run on, of each core's top frequency (`None` for a core without one): all but
/// those of the slowest cluster. `None` when the cores don't differ or their speed isn't known,
/// which leaves the thread as it is.
pub fn fast(freqs: &[Option<u64>]) -> Option<Vec<usize>> {
  let slowest = freqs.iter().flatten().min()?;
  if freqs.iter().any(Option::is_none) {
    return None;
  }

  let fast: Vec<usize> = (0..freqs.len()).filter(|&i| freqs[i] > Some(*slowest)).collect();
  (!fast.is_empty()).then_some(fast)
}

/// Keeps the calling thread, and the threads it starts from now on, on the fast cores. Returns how
/// many those are, `None` when nothing changed.
#[cfg(any(target_os = "android", target_os = "linux"))]
pub fn pin() -> Option<usize> {
  let count = std::thread::available_parallelism().ok()?.get();
  let freq = |cpu: usize| {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
    std::fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()
  };
  // Cores may be offline or outside the thread's set, so their numbers go past the count.
  let freqs: Vec<Option<u64>> = (0..count.max(16)).map(freq).collect();
  let known = freqs.iter().rposition(Option::is_some)? + 1;
  let fast = fast(&freqs[..known])?;

  // SAFETY: `set` is a zeroed cpu_set_t, which CPU_SET fills within its size (the cores are below
  // 16), and sched_setaffinity reads it for the calling thread (pid 0).
  let ok = unsafe {
    let mut set: libc::cpu_set_t = std::mem::zeroed();
    for &cpu in &fast {
      libc::CPU_SET(cpu, &mut set);
    }

    libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
  };
  ok.then_some(fast.len())
}

#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub fn pin() -> Option<usize> {
  None
}

#[cfg(test)]
mod tests {
  use super::fast;

  #[test]
  fn skips_the_slowest_cluster() {
    // A Pixel 7 Pro: four A55, two A78, two X1.
    let k = |x: u64| Some(x * 1000);
    let pixel = [k(1803), k(1803), k(1803), k(1803), k(2348), k(2348), k(2850), k(2850)];
    assert_eq!(fast(&pixel), Some(vec![4, 5, 6, 7]));
  }

  #[test]
  fn leaves_equal_or_unknown_cores() {
    assert_eq!(fast(&[Some(3000), Some(3000)]), None);
    assert_eq!(fast(&[Some(3000), None, Some(1000)]), None);
    assert_eq!(fast(&[]), None);
  }
}
