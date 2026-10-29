//! Synthetic tick logs: a route of two scenes with a stall between them.

use crate::config::Config;
use crate::log::{Run, TickLog};

const VB: u64 = 571_236;

/// A route of `a_ticks` in scene 1 (flip every `a_every` ticks), then `stall` ticks without a
/// flip, then `b_ticks` in scene 2 (flip every 3 ticks). Columns: scene, sim, faces.
fn synth(a_ticks: u64, a_every: u64, stall: u64, b_ticks: u64) -> String {
    let mut s = String::from(
        "boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls,scene,sim,faces\n",
    );
    let mut tick = 0;
    let mut sim = 0;
    let row = |s: &mut String,
               step: u64,
               flip: bool,
               scene: u64,
               faces: u64,
               tick: &mut u64,
               sim: &mut u64,
               advance_sim: bool| {
        *tick += 1;
        if advance_sim {
            *sim += 1;
        }
        s.push_str(&format!(
            "1,{t},{step},{c},{VB},1000,{f},0,{scene},{sim},{faces}\n",
            t = *tick,
            c = *tick * VB,
            f = u8::from(flip),
            sim = *sim
        ));
    };
    for i in 0..a_ticks {
        row(
            &mut s,
            1,
            i % a_every == 0,
            1,
            100,
            &mut tick,
            &mut sim,
            true,
        );
    }
    for i in 0..stall {
        // The scene word changes at once, the simulation resumes only at the end.
        row(&mut s, 2, false, 2, 0, &mut tick, &mut sim, i + 1 == stall);
    }
    for i in 0..b_ticks {
        row(&mut s, 2, i % 3 == 0, 2, 200, &mut tick, &mut sim, true);
    }
    s
}

fn cfg(ready: bool) -> Config {
    let extra = if ready {
        "ready = \"sim\"\n"
    } else {
        "load_vblanks = 20\n"
    };
    let text = format!(
        "title = \"Test\"\n[scene]\nword = \"scene\"\n{extra}[scene.names]\n1 = \"alpha\"\n2 = \"beta\"\n[[counter]]\nword = \"faces\"\nlabel = \"Faces\"\n"
    );
    toml::from_str(&text).expect("config")
}

#[test]
fn stall_mode_finds_the_load_and_excludes_it() {
    let log = TickLog::parse(&synth(200, 2, 40, 90)).expect("parse");
    let run = Run::measure(&log, &cfg(false), Default::default()).expect("measure");
    assert_eq!(run.loads.len(), 1);
    assert_eq!(run.loads[0].from, "alpha");
    assert_eq!(run.loads[0].to, "beta");
    assert!(run.loads[0].seconds > 0.6);
    let rows = run.summaries();
    assert_eq!(rows[0].name, "alpha");
    assert!(
        (rows[0].mean_fps - 29.67).abs() < 0.2,
        "alpha mean {}",
        rows[0].mean_fps
    );
    assert!(
        (rows[1].mean_fps - 19.78).abs() < 0.2,
        "beta mean {}",
        rows[1].mean_fps
    );
    assert_eq!(rows[1].pct_over_2, 100.0);
    assert_eq!(rows[1].pct_over_3, 0.0);
    assert!(run.play_frames().all(|f| f.vblanks < 20));
}

#[test]
fn ready_mode_ends_the_load_at_the_first_sim_tick() {
    let log = TickLog::parse(&synth(200, 2, 40, 90)).expect("parse");
    let run = Run::measure(&log, &cfg(true), Default::default()).expect("measure");
    assert_eq!(run.loads.len(), 1);
    // 40 stall ticks, the sim moves on the last of them.
    assert!(
        (39..=41).contains(&run.loads[0].vblanks),
        "load {} vblanks",
        run.loads[0].vblanks
    );
}

#[test]
fn rejects_a_log_that_is_not_a_tick_log() {
    assert!(TickLog::parse("a,b,c\n1,2,3\n").is_err());
    assert!(TickLog::parse("").is_err());
}

#[test]
fn counter_fit_explains_a_counter_driven_route() {
    // Frame cost follows the faces counter exactly, so the fit explains nearly everything.
    let mut s = String::from(
        "boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls,scene,sim,faces\n",
    );
    let mut tick = 0u64;
    for k in 0..600u64 {
        let faces = 100 + (k % 7) * 40;
        let vb = 2 + faces / 120;
        for i in 0..vb {
            tick += 1;
            s.push_str(&format!(
                "1,{tick},1,{},{VB},1000,{},0,1,{tick},{faces}\n",
                tick * VB,
                u8::from(i + 1 == vb)
            ));
        }
    }
    let log = TickLog::parse(&s).expect("parse");
    let run = Run::measure(&log, &cfg(false), Default::default()).expect("measure");
    let fit = crate::stats::Fit::of(&run).expect("fit");
    assert!(fit.r2 > 0.8, "r2 {}", fit.r2);
}

#[test]
fn compare_lines_runs_up_by_step_and_writes_a_page() {
    let dir = std::env::temp_dir().join(format!("frametime-test-{}", std::process::id()));
    let c = cfg(false);
    let a = Run::measure(
        &TickLog::parse(&synth(200, 2, 40, 90)).expect("a"),
        &c,
        Default::default(),
    )
    .expect("a");
    // The second run loads twice as long and plays scene 1 slower.
    let b = Run::measure(
        &TickLog::parse(&synth(200, 3, 80, 90)).expect("b"),
        &c,
        Default::default(),
    )
    .expect("b");
    let line = crate::compare::write(&a, &b, &c, ("before", "after"), &dir).expect("compare");
    assert!(line.contains("whole route"));
    let html = std::fs::read_to_string(dir.join("index.html")).expect("page");
    assert!(html.contains("Test Frame Time"));
    assert!(html.contains("\"aligned\":true"));
    let csv = std::fs::read_to_string(dir.join("data/compare.csv")).expect("csv");
    assert!(csv.lines().any(|l| l.starts_with("alpha,")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_stalls_in_a_row_are_two_loads() {
    // Flip, 40 quiet ticks, flip, 35 quiet ticks, then steady play: both gaps are loads.
    let mut s = String::from(
        "boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls,scene,sim,faces\n",
    );
    let mut tick = 0u64;
    let mut push = |s: &mut String, flip: bool| {
        tick += 1;
        s.push_str(&format!(
            "1,{tick},1,{},{VB},1000,{},0,1,{tick},100\n",
            tick * VB,
            u8::from(flip)
        ));
    };
    for i in 0..30 {
        push(&mut s, i % 2 == 0);
    }
    for _ in 0..39 {
        push(&mut s, false);
    }
    push(&mut s, true);
    for _ in 0..34 {
        push(&mut s, false);
    }
    push(&mut s, true);
    for i in 0..60 {
        push(&mut s, i % 2 == 0);
    }
    let log = TickLog::parse(&s).expect("parse");
    let run = Run::measure(&log, &cfg(false), Default::default()).expect("measure");
    assert_eq!(run.loads.len(), 2);
    assert!(run.play_frames().all(|f| f.vblanks < 20));
}

/// Title ticks (mode 0), play (mode 1), a load (mode 2), play again; scene word follows.
fn moded(segment_scene: u64) -> String {
    let mut s =
        String::from("boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls,scene,mode\n");
    let mut tick = 0u64;
    let mut push = |s: &mut String, flip: bool, scene: u64, mode: u64| {
        tick += 1;
        s.push_str(&format!(
            "1,{tick},1,{},{VB},1000,{},0,{scene},{mode}\n",
            tick * VB,
            u8::from(flip)
        ));
    };
    for i in 0..50 {
        push(&mut s, i % 3 == 0, 0, 0);
    }
    for i in 0..100 {
        push(&mut s, i % 2 == 0, segment_scene, 1);
    }
    for i in 0..30 {
        push(&mut s, i == 29, segment_scene + 1, 2);
    }
    for i in 0..100 {
        push(&mut s, i % 3 == 0, segment_scene + 1, 1);
    }
    s
}

fn cfg_mode() -> Config {
    toml::from_str(
        "title = \"Test\"\n[scene]\nword = \"scene\"\nmode = \"mode\"\n[scene.names]\n1 = \"alpha\"\n2 = \"beta\"\n3 = \"gamma\"\n",
    )
    .expect("config")
}

#[test]
fn mode_word_separates_menus_play_and_loads() {
    let log = TickLog::parse(&moded(1)).expect("parse");
    let run = Run::measure(&log, &cfg_mode(), Default::default()).expect("measure");
    assert_eq!(run.loads.len(), 1);
    assert_eq!(run.loads[0].to, "beta");
    assert_eq!(run.loads[0].vblanks, 30);
    // The 50 title ticks are not game time: the clock starts at the first play tick.
    assert!((3.7..4.0).contains(&run.total_s), "total {}", run.total_s);
    assert!(run.play_frames().all(|f| f.vblanks <= 3));
    assert_eq!(run.summaries().len(), 3);
}

#[test]
fn segments_join_into_one_route() {
    let a = TickLog::parse(&moded(1)).expect("a");
    let b = TickLog::parse(&moded(2)).expect("b");
    let (log, offsets) = TickLog::join(vec![a, b]).expect("join");
    assert_eq!(offsets, vec![0, 1]);
    let run = Run::measure(&log, &cfg_mode(), Default::default()).expect("measure");
    assert_eq!(run.loads.len(), 2);
    // The second segment's title ticks are left out as well.
    assert!((7.5..8.0).contains(&run.total_s), "total {}", run.total_s);
}

#[test]
fn telemetry_columns_add_an_update_render_split() {
    let mut s = String::from(
        "boot,tick,step,bus_cycles,cycle_delta,cpu_delta,flipped,polls,scene,tel_events,tel_update,tel_render,tel_present\n",
    );
    for t in 1..=400u64 {
        // Two vblanks a frame: 300k cycles of update and 700k of render land in the flip tick.
        let flip = t % 2 == 0;
        let (u, r) = if flip { (300_000, 700_000) } else { (0, 0) };
        s.push_str(&format!(
            "1,{t},1,{},{VB},1000,{},0,1,4,{u},{r},0\n",
            t * VB,
            u8::from(flip)
        ));
    }
    let log = TickLog::parse(&s).expect("parse");
    let c: Config = toml::from_str(
        "title = \"Test\"\n[scene]\nword = \"scene\"\n[scene.names]\n1 = \"alpha\"\n",
    )
    .expect("config");
    let run = Run::measure(&log, &c, Default::default()).expect("measure");
    assert!(run.has_stages);
    let f = run.play_frames().next().expect("a frame");
    assert_eq!(f.stages, Some([300_000, 700_000]));
    let dir = std::env::temp_dir().join(format!("frametime-stages-{}", std::process::id()));
    crate::report::write(&run, &c, &[], &dir).expect("write");
    let html = std::fs::read_to_string(dir.join("index.html")).expect("page");
    assert!(html.contains("guest update stage"));
    assert!(html.contains("Update (ms, mean)"));
    let _ = std::fs::remove_dir_all(&dir);
}
