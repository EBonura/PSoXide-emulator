//! Bounded capture of the window renderer lifecycle without a window.
//! Optional exact fresh-boot comparison checks lifecycle isolation, not
//! CPU/hardware rendering parity or a passing gameplay journey.
#![allow(dead_code)]

#[path = "../src/card.rs"]
mod card;
#[path = "../src/compare.rs"]
mod compare;
#[path = "../src/img.rs"]
mod img;
#[path = "../src/journey.rs"]
mod journey;
#[path = "../src/machine.rs"]
mod machine;
#[path = "../src/symbols.rs"]
mod symbols;
#[path = "../src/tick_log.rs"]
mod tick_log;

use clap::Parser;
use machine::{Boot, Machine};
use psx_gpu_render::{HwRenderer, ScaleMode};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    disc: PathBuf,
    /// Play another disc first, retaining the same renderer for the target disc.
    #[arg(long, requires = "previous_route")]
    previous_disc: Option<PathBuf>,
    #[arg(long, requires = "previous_disc")]
    previous_route: Option<PathBuf>,
    /// Require target checkpoints to equal boot1 captures from a fresh process.
    #[arg(long, requires = "previous_disc")]
    expect_fresh: Option<PathBuf>,
    #[arg(long)]
    route: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=4))]
    batch: u32,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=4))]
    scale: u32,
    /// Repeat the boot and route on the same persistent renderer.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=2))]
    boots: u32,
    /// Change Window scale before the batch starting at this tick: TICK:SCALE.
    #[arg(long)]
    scale_at: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    name: String,
    title: Option<String>,
    max_ticks: u64,
    #[serde(rename = "step")]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    wait: Option<u64>,
    hold: Option<u64>,
    #[serde(default)]
    press: Vec<String>,
    checkpoint: Option<String>,
    golden: Option<bool>,
}

struct Plan {
    pads: Vec<u16>,
    checkpoints: BTreeMap<u64, Vec<String>>,
}

fn require_exact(actual: &img::Img, expected: &std::path::Path) -> Result<(), String> {
    let reference = img::Img::load_png(expected)?;
    if actual.w != reference.w || actual.h != reference.h || actual.rgba != reference.rgba {
        return Err(format!(
            "lifecycle checkpoint differs from {}",
            expected.display()
        ));
    }
    Ok(())
}

fn plan(route: &Route) -> Result<Plan, String> {
    if route.max_ticks == 0 || route.max_ticks > 60_000 {
        return Err("max_ticks must be 1..60000".into());
    }
    let mut p = Plan {
        pads: Vec::new(),
        checkpoints: BTreeMap::new(),
    };
    for step in &route.steps {
        if step.golden == Some(true) {
            return Err("capture diagnostic cannot evaluate goldens".into());
        }
        let mut mask = 0;
        for name in &step.press {
            let bit = machine::button_mask(name);
            if bit == 0 {
                return Err(format!("unknown button {name}"));
            }
            mask |= bit;
        }
        let held = if mask == 0 { 0 } else { step.hold.unwrap_or(4) };
        let wait = step.wait.unwrap_or(0);
        let total = (p.pads.len() as u64)
            .checked_add(held)
            .and_then(|n| n.checked_add(wait))
            .ok_or("route duration overflow")?;
        if total > route.max_ticks {
            return Err("route exceeds max_ticks".into());
        }
        p.pads.extend(std::iter::repeat_n(mask, held as usize));
        p.pads.extend(std::iter::repeat_n(0, wait as usize));
        if let Some(name) = &step.checkpoint {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(
                    "checkpoint name must contain only letters, digits, underscore or hyphen"
                        .into(),
                );
            }
            p.checkpoints.entry(total).or_default().push(name.clone());
        }
    }
    if p.pads.is_empty() {
        return Err("empty route".into());
    }
    Ok(p)
}

fn run(a: Args) -> Result<(), String> {
    let route: Route = toml::from_str(&fs::read_to_string(&a.route).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let p = plan(&route)?;
    let mut scale_events = BTreeMap::new();
    for event in &a.scale_at {
        let (tick, scale) = event.split_once(':').ok_or("scale-at needs TICK:SCALE")?;
        let tick: u64 = tick.parse().map_err(|_| "invalid scale tick")?;
        let scale: u32 = scale.parse().map_err(|_| "invalid scale")?;
        if tick >= p.pads.len() as u64
            || !(1..=4).contains(&scale)
            || scale_events.insert(tick, scale).is_some()
        {
            return Err("invalid or duplicate scale event".into());
        }
    }
    fs::create_dir(&a.out).map_err(|e| format!("fresh output directory required: {e}"))?;
    let mut log = fs::File::create(a.out.join("lifecycle.log")).map_err(|e| e.to_string())?;
    writeln!(
        log,
        "capture only; route={} batch={} initial_scale={} boots={} filter=nearest",
        route.name, a.batch, a.scale, a.boots
    )
    .map_err(|e| e.to_string())?;
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok_or("no hardware adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("gui-lifecycle"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .map_err(|e| e.to_string())?;
    let mut hw = HwRenderer::new_headless(device.clone(), queue);
    hw.set_texture_filter(0);
    let mut last_fallback = false;
    let previous = a
        .previous_route
        .as_ref()
        .map(|path| {
            let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
            let route: Route = toml::from_str(&text).map_err(|e| e.to_string())?;
            plan(&route)
        })
        .transpose()?;
    let mut boots = Vec::new();
    if let (Some(disc), Some(plan)) = (&a.previous_disc, &previous) {
        boots.push((disc, plan));
    }
    boots.extend((0..a.boots).map(|_| (&a.disc, &p)));
    for (boot_index, (disc, p)) in boots.into_iter().enumerate() {
        let generation = boot_index + 1;
        writeln!(log, "boot={generation} disc={}", disc.display()).map_err(|e| e.to_string())?;
        let mut m = Machine::boot(&Boot {
            disc,
            pad: journey::PadMode::Analog,
            pad2: journey::Pad2Mode::None,
            memcard: journey::MemcardMode::Fresh,
            cmd_log: true,
            track_display: true,
        })?;
        hw.restore_environment(&m.bus.gpu);
        let mut desired_scale = a.scale;
        let mut first_batch = true;
        while m.ticks < p.pads.len() as u64 {
            let start = m.ticks;
            if let Some(scale) = scale_events.get(&start) {
                desired_scale = *scale;
            }
            let next_checkpoint = p
                .checkpoints
                .range((start + 1)..)
                .next()
                .map(|(t, _)| *t)
                .unwrap_or(u64::MAX);
            let next_scale = scale_events
                .range((start + 1)..)
                .next()
                .map(|(t, _)| *t)
                .unwrap_or(u64::MAX);
            let limit = (start + u64::from(a.batch))
                .min(p.pads.len() as u64)
                .min(next_checkpoint)
                .min(next_scale);
            // A redraw samples its controller once. Split at input changes
            // without shifting the fixed reference route's press/release ticks.
            let mask = p.pads[start as usize];
            let end = ((start + 1)..limit)
                .find(|&tick| p.pads[tick as usize] != mask)
                .unwrap_or(limit);
            let before = m.bus.gpu.vram.words().to_vec();
            let env_before = hw.debug_env();
            m.set_pad(1, mask, Machine::centre(), Machine::centre());
            for _ in start..end {
                m.tick()?;
            }
            let commands = m.bus.gpu.drain_completed_cmd_log();
            let area = m.bus.gpu.display_area();
            let dims = (u32::from(area.width).max(1), u32::from(area.height).max(1));
            let scale = HwRenderer::scale_for(
                ScaleMode::Window,
                (dims.0 * desired_scale, dims.1 * desired_scale),
                dims,
            );
            let resized = hw.set_internal_scale(scale, None);
            let fallback = area.bpp24 || m.bus.gpu.field_rendering_active();
            let leaving_fallback = last_fallback && !fallback;
            let resync = first_batch || resized || leaving_fallback;
            if resync {
                hw.sync_target_from_vram(&before);
            }
            hw.render_frame(&m.bus.gpu, &commands, &before);
            writeln!(log, "generation={generation} ticks={start}..{end} batch={} commands={} scale={scale} filter=nearest resized={resized} bpp24={} field={} leaving_fallback={leaving_fallback} resync={resync} env_before={env_before:?} env_after={:?}",
                end-start, commands.len(), area.bpp24, m.bus.gpu.field_rendering_active(), hw.debug_env()).map_err(|e| e.to_string())?;
            first_batch = false;
            last_fallback = fallback;
            if let Some(names) = p.checkpoints.get(&end) {
                device.poll(wgpu::Maintain::Wait);
                let cpu = m.display_image();
                for name in names {
                    let stem = format!("boot{generation}-{end}-{name}");
                    cpu.save_png(&a.out.join(format!("{stem}-cpu.png")))?;
                    let reference = a.expect_fresh.as_ref().filter(|_| boot_index > 0);
                    if let Some(dir) = reference {
                        require_exact(&cpu, &dir.join(format!("boot1-{end}-{name}-cpu.png")))?;
                    }
                    if fallback {
                        writeln!(
                            log,
                            "checkpoint={stem} hardware=unavailable bpp24_or_field=true"
                        )
                        .map_err(|e| e.to_string())?;
                    } else {
                        let (w, h, rgba) = hw.read_subrect_rgba8(
                            u32::from(area.x) * scale,
                            u32::from(area.y) * scale,
                            u32::from(area.width) * scale,
                            u32::from(area.height) * scale,
                        );
                        let hardware = img::Img::from_rgba(w, h, rgba);
                        hardware.save_png(&a.out.join(format!("{stem}-hw{scale}.png")))?;
                        if let Some(dir) = reference {
                            require_exact(
                                &hardware,
                                &dir.join(format!("boot1-{end}-{name}-hw{scale}.png")),
                            )?;
                        }
                    }
                }
                log.flush().map_err(|e| e.to_string())?;
            }
            if end % 32 == 0 {
                device.poll(wgpu::Maintain::Wait);
            } else {
                device.poll(wgpu::Maintain::Poll);
            }
        }
    }
    writeln!(
        log,
        "complete; fresh_boot_comparison={}; no CPU/hardware parity verdict",
        a.expect_fresh.is_some()
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    if let Err(e) = run(Args::parse()) {
        eprintln!("gui-lifecycle: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_preserves_press_release_and_checkpoint_ticks() {
        let route: Route = toml::from_str("name='test'\nmax_ticks=20\n[[step]]\nwait=3\n[[step]]\npress=['cross']\nhold=4\nwait=2\ncheckpoint='end'\ngolden=false\n").unwrap();
        let p = plan(&route).unwrap();
        assert_eq!(
            p.pads,
            [
                vec![0; 3],
                vec![machine::button_mask("cross"); 4],
                vec![0; 2]
            ]
            .concat()
        );
        assert_eq!(p.checkpoints[&9], vec!["end"]);
    }
    #[test]
    fn rejects_unsupported_actions_and_overlong_routes() {
        assert!(
            toml::from_str::<Route>("name='x'\nmax_ticks=3\n[[step]]\nwait_until={}\n").is_err()
        );
        let r: Route = toml::from_str("name='x'\nmax_ticks=3\n[[step]]\nwait=4\n").unwrap();
        assert!(plan(&r).is_err());
    }
}
