use std::time::{Duration, Instant};

use inlet_media::Playback;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn three(t0: Instant) -> Playback {
    Playback::new(vec![ms(100), ms(200), ms(100)], t0)
}

#[test]
fn advances_with_time_and_loops() {
    let t0 = Instant::now();
    let mut p = three(t0);
    assert_eq!(p.frame_at(t0), 0);
    assert_eq!(p.frame_at(t0 + ms(99)), 0);
    assert_eq!(p.frame_at(t0 + ms(100)), 1);
    assert_eq!(p.frame_at(t0 + ms(299)), 1);
    assert_eq!(p.frame_at(t0 + ms(300)), 2);
    assert_eq!(p.frame_at(t0 + ms(400)), 0); // looped
    assert_eq!(p.frame_at(t0 + ms(510)), 1);
}

#[test]
fn a_long_sleep_does_not_spin() {
    let t0 = Instant::now();
    let mut p = three(t0);
    // A laptop that slept for a week: must answer at once, with the right frame.
    let later = t0 + Duration::from_secs(7 * 24 * 3600) + ms(150);
    let frame = p.frame_at(later);
    // 7 days is a whole number of 400 ms cycles, so 150 ms into the cycle is frame 1.
    assert_eq!(frame, 1);
}

#[test]
fn pausing_holds_the_frame_and_resuming_continues_from_there() {
    let t0 = Instant::now();
    let mut p = three(t0);
    p.pause(t0 + ms(150)); // 50 ms into frame 1
    assert!(!p.is_playing());
    assert_eq!(p.frame_at(t0 + Duration::from_secs(3600)), 1);
    assert_eq!(p.time_to_next_frame(t0 + Duration::from_secs(3600)), None);

    let t1 = t0 + Duration::from_secs(3600);
    p.resume(t1);
    assert!(p.is_playing());
    assert_eq!(p.frame_at(t1 + ms(100)), 1); // 150 ms into the 200 ms frame
    assert_eq!(p.frame_at(t1 + ms(150)), 2);
}

#[test]
fn reduced_motion_shows_the_first_frame_and_stays() {
    let t0 = Instant::now();
    let mut p = three(t0);
    assert_eq!(p.frame_at(t0 + ms(150)), 1);
    p.set_reduced_motion(true, t0 + ms(150));
    assert_eq!(p.frame_at(t0 + ms(150)), 0);
    assert_eq!(p.frame_at(t0 + Duration::from_secs(10)), 0);
    assert!(!p.is_playing());
    p.set_reduced_motion(false, t0 + Duration::from_secs(10));
    assert!(p.is_playing());
    assert_eq!(p.frame_at(t0 + Duration::from_secs(10) + ms(120)), 1);
}

#[test]
fn a_still_image_never_moves() {
    let t0 = Instant::now();
    let mut p = Playback::new(vec![ms(100)], t0);
    assert!(!p.is_playing());
    assert_eq!(p.frame_at(t0 + Duration::from_secs(60)), 0);
    assert_eq!(p.time_to_next_frame(t0), None);
    let mut empty = Playback::new(vec![], t0);
    assert_eq!(empty.frame_at(t0 + ms(5)), 0);
}

#[test]
fn tells_when_to_repaint() {
    let t0 = Instant::now();
    let mut p = three(t0);
    assert_eq!(p.time_to_next_frame(t0), Some(ms(100)));
    assert_eq!(p.time_to_next_frame(t0 + ms(40)), Some(ms(60)));
    p.frame_at(t0 + ms(130));
    assert_eq!(p.time_to_next_frame(t0 + ms(130)), Some(ms(170)));
}

#[test]
fn time_going_backwards_is_harmless() {
    let t0 = Instant::now() + Duration::from_secs(10);
    let mut p = three(t0);
    p.frame_at(t0 + ms(150));
    assert_eq!(p.frame_at(t0), 1);
}
