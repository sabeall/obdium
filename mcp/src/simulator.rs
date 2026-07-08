//! A coherent vehicle simulator for the MCP server.
//!
//! Unlike demo mode (which replays *random* recorded PID responses, so the
//! numbers jump around incoherently), this produces a physically plausible,
//! smoothly evolving drive cycle: the engine warms up, the car idles then
//! accelerates, cruises, decelerates and idles again on a loop, and every
//! sensor is derived from that shared state so the readings agree with each
//! other (RPM tracks speed and gear, MAF tracks RPM and load, MAP tracks
//! throttle, and so on).
//!
//! It can also inject a fault scenario (vacuum leak, misfire, overheat) that
//! skews the relevant live values *and* reports matching trouble codes with a
//! lit check-engine light — useful for exercising a troubleshooting flow
//! without a car.

use std::time::Instant;

use obdium::diagnostics::{TroubleCode, TroubleCodeCategory};

/// One full idle → accelerate → cruise → decelerate → idle loop, in seconds.
const CYCLE: f32 = 140.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scenario {
    Healthy,
    VacuumLeak,
    Misfire,
    Overheat,
}

impl Scenario {
    /// Parse a user-supplied scenario name. Returns `None` for unknown values
    /// so the caller can report the valid options.
    pub fn parse(s: &str) -> Option<Scenario> {
        match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
            "" | "healthy" | "none" | "ok" | "normal" => Some(Scenario::Healthy),
            "vacuum_leak" | "lean" | "vacuum" => Some(Scenario::VacuumLeak),
            "misfire" | "rough_idle" => Some(Scenario::Misfire),
            "overheat" | "overheating" | "hot" => Some(Scenario::Overheat),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Scenario::Healthy => "healthy",
            Scenario::VacuumLeak => "vacuum_leak",
            Scenario::Misfire => "misfire",
            Scenario::Overheat => "overheat",
        }
    }

    pub const VALID: &'static str = "healthy, vacuum_leak, misfire, overheat";
}

/// A single coherent snapshot of every simulated sensor.
pub struct Frame {
    pub rpm: f32,
    pub speed: f32,
    pub load: f32,
    pub coolant: f32,
    pub intake_air: f32,
    pub ambient: f32,
    pub map: f32,
    pub maf: f32,
    pub throttle: f32,
    pub timing: f32,
    pub stft1: f32,
    pub ltft1: f32,
    pub voltage: f32,
    pub fuel_level: f32,
    pub runtime: f32,
}

pub struct Simulator {
    start: Instant,
    scenario: Scenario,
    ambient: f32,
}

impl Simulator {
    pub fn new(scenario: Scenario) -> Self {
        Self {
            start: Instant::now(),
            scenario,
            ambient: 22.0,
        }
    }

    pub fn scenario(&self) -> Scenario {
        self.scenario
    }

    /// Clearing codes turns the fault (and the check-engine light) off, exactly
    /// like clearing them on a real ECU.
    pub fn clear_codes(&mut self) {
        self.scenario = Scenario::Healthy;
    }

    pub fn elapsed(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }

    /// The current snapshot, based on wall-clock time since `connect`.
    pub fn frame(&self) -> Frame {
        self.frame_at(self.elapsed())
    }

    /// Compute a frame at an arbitrary elapsed time. Split out so tests can
    /// drive the model deterministically.
    pub fn frame_at(&self, t: f32) -> Frame {
        let tc = t.rem_euclid(CYCLE);
        let speed = speed_profile(tc);

        // Acceleration (km/h per second) via a small central difference; the
        // cycle boundaries are all at idle (speed 0) so wrap-around is smooth.
        let dt = 0.5;
        let accel = (speed_profile((tc + dt).rem_euclid(CYCLE))
            - speed_profile((tc - dt).rem_euclid(CYCLE)))
            / (2.0 * dt);

        let idling = speed < 2.0;

        // RPM: idle wobble when stopped; otherwise a banded "gearbox" that
        // shifts (drops revs) as speed climbs, plus a lift under acceleration.
        let mut rpm = if idling {
            800.0 + 30.0 * (t * 1.7).sin()
        } else {
            let gear_rpm = 1200.0 + (speed.rem_euclid(25.0)) * 72.0;
            gear_rpm + accel * 110.0
        };

        // Throttle follows driver demand.
        let mut throttle = if idling {
            12.0 + 2.0 * (t * 0.9).sin()
        } else if accel > 1.0 {
            30.0 + accel * 9.0
        } else if accel < -1.0 {
            5.0 // closed throttle on the overrun
        } else {
            18.0 + 4.0 * (t * 0.3).sin()
        };

        // Load and the air path all follow from throttle / RPM.
        let mut load = throttle * 0.85 + 12.0 + 6.0 * (t * 0.5).sin();
        let map = 18.0 + throttle * 0.85;
        let mut maf = (rpm / 1000.0) * (load / 100.0) * 11.0;
        let timing = 30.0 - load * 0.15 + 3.0 * (t * 0.7).sin();

        // Closed-loop fuel trims: small oscillation around a stable long-term
        // value on a healthy engine.
        let mut stft1 = 2.5 * (t * 0.8).sin() + 1.0 * (t * 0.23).sin();
        let mut ltft1 = -1.5 + 1.0 * (t / 40.0).sin();

        let voltage = 14.3 + 0.18 * (t * 0.6).sin();

        // Coolant warms asymptotically toward operating temperature.
        let mut coolant =
            self.ambient + (92.0 - self.ambient) * (1.0 - (-t / 120.0).exp()) + 1.5 * (t * 0.05).sin();
        let intake_air = self.ambient + (coolant - self.ambient) * 0.08 + 3.0;
        let fuel_level = (68.0 - t * 0.02).max(2.0);

        // --- fault scenarios -------------------------------------------------
        match self.scenario {
            Scenario::Healthy => {}
            Scenario::VacuumLeak => {
                // Unmetered air leans the mixture; the ECU trims fuel up hard,
                // worst at idle where the leak is a bigger fraction of airflow.
                let idle_weight = if idling { 1.0 } else { 0.35 };
                stft1 += 9.0 * idle_weight;
                ltft1 += 24.0 * idle_weight;
                if idling {
                    rpm += 120.0; // slightly high, hunting idle
                }
            }
            Scenario::Misfire => {
                // Rough running: RPM jitter and erratic short-term fuel trim.
                let rough = if idling { 1.0 } else { 0.3 };
                rpm += 140.0 * (t * 9.0).sin() * rough;
                stft1 += 7.0 * (t * 5.0).sin();
                maf *= 0.97;
            }
            Scenario::Overheat => {
                // Climbs past the normal thermostat range toward the red.
                coolant = self.ambient + (119.0 - self.ambient) * (1.0 - (-t / 90.0).exp());
                load += 6.0;
            }
        }

        // Clamp to physically sane ranges.
        rpm = rpm.clamp(600.0, 6500.0);
        throttle = throttle.clamp(6.0, 96.0);
        load = load.clamp(12.0, 99.0);
        maf = maf.max(1.4);

        Frame {
            rpm,
            speed,
            load,
            coolant,
            intake_air,
            ambient: self.ambient,
            map: map.clamp(12.0, 102.0),
            maf,
            throttle,
            timing: timing.clamp(-10.0, 45.0),
            stft1,
            ltft1,
            voltage,
            fuel_level,
            runtime: t,
        }
    }

    pub fn check_engine_light(&self) -> bool {
        self.scenario != Scenario::Healthy
    }

    /// Trouble codes matching the active scenario, with descriptions looked up
    /// from OBDium's code database (same source the real reads use).
    pub fn trouble_codes(&self) -> Vec<TroubleCode> {
        let codes: &[&str] = match self.scenario {
            Scenario::Healthy => &[],
            Scenario::VacuumLeak => &["P0171"],
            Scenario::Misfire => &["P0300", "P0301"],
            Scenario::Overheat => &["P0217"],
        };
        codes
            .iter()
            .map(|dtc| TroubleCode::new(TroubleCodeCategory::Powertrain, dtc.to_string(), false))
            .collect()
    }
}

/// Vehicle speed (km/h) as a function of position within the drive cycle.
fn speed_profile(tc: f32) -> f32 {
    if tc < 20.0 {
        0.0 // idle
    } else if tc < 45.0 {
        smoothstep((tc - 20.0) / 25.0) * 105.0 // accelerate to ~105
    } else if tc < 100.0 {
        100.0 + 6.0 * ((tc - 45.0) / 9.0).sin() // cruise ~100 with gentle undulation
    } else if tc < 122.0 {
        (1.0 - smoothstep((tc - 100.0) / 22.0)) * 100.0 // decelerate to 0
    } else {
        0.0 // idle
    }
}

fn smoothstep(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    p * p * (3.0 - 2.0 * p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_parsing_is_forgiving() {
        assert_eq!(Scenario::parse("Vacuum-Leak"), Some(Scenario::VacuumLeak));
        assert_eq!(Scenario::parse(" MISFIRE "), Some(Scenario::Misfire));
        assert_eq!(Scenario::parse(""), Some(Scenario::Healthy));
        assert_eq!(Scenario::parse("nonsense"), None);
    }

    #[test]
    fn readings_stay_in_physical_ranges_across_a_cycle() {
        let sim = Simulator::new(Scenario::Healthy);
        let mut t = 0.0;
        while t < CYCLE * 2.0 {
            let f = sim.frame_at(t);
            assert!(f.rpm >= 600.0 && f.rpm <= 6500.0, "rpm {} at t={t}", f.rpm);
            assert!(f.speed >= 0.0 && f.speed <= 115.0, "speed {} at t={t}", f.speed);
            assert!(f.throttle >= 6.0 && f.throttle <= 96.0);
            assert!(f.load >= 12.0 && f.load <= 99.0);
            assert!(f.maf >= 1.4);
            assert!(f.coolant > 15.0 && f.coolant < 110.0, "coolant {}", f.coolant);
            t += 1.0;
        }
    }

    #[test]
    fn engine_warms_up_over_time() {
        let sim = Simulator::new(Scenario::Healthy);
        let cold = sim.frame_at(2.0).coolant;
        let warm = sim.frame_at(300.0).coolant;
        assert!(warm > cold + 30.0, "cold {cold} warm {warm}");
        assert!(warm > 80.0, "should reach operating temp, got {warm}");
    }

    #[test]
    fn idle_then_moving_is_coherent() {
        let sim = Simulator::new(Scenario::Healthy);
        // t=5s is inside the initial idle window; t=70s is mid-cruise.
        let idle = sim.frame_at(5.0);
        let cruise = sim.frame_at(70.0);
        assert!(idle.speed < 2.0, "should be idling, speed {}", idle.speed);
        assert!(cruise.speed > 60.0, "should be cruising, speed {}", cruise.speed);
        assert!(cruise.rpm > idle.rpm, "revs should rise with speed");
        assert!(cruise.maf > idle.maf, "airflow should rise with load");
    }

    #[test]
    fn vacuum_leak_drives_fuel_trims_positive_and_sets_a_code() {
        let sim = Simulator::new(Scenario::VacuumLeak);
        let f = sim.frame_at(5.0); // idle, where the leak bites hardest
        assert!(f.ltft1 > 10.0, "expected large positive LTFT, got {}", f.ltft1);
        assert!(sim.check_engine_light());
        let codes = sim.trouble_codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].dtc, "P0171");
    }

    #[test]
    fn overheat_pushes_coolant_into_the_red() {
        let sim = Simulator::new(Scenario::Overheat);
        let f = sim.frame_at(300.0);
        assert!(f.coolant > 110.0, "coolant should be overheating, got {}", f.coolant);
        assert!(sim.check_engine_light());
    }

    #[test]
    fn healthy_has_no_codes_and_no_mil() {
        let sim = Simulator::new(Scenario::Healthy);
        assert!(!sim.check_engine_light());
        assert!(sim.trouble_codes().is_empty());
    }

    #[test]
    fn clearing_codes_turns_the_light_off() {
        let mut sim = Simulator::new(Scenario::Misfire);
        assert!(sim.check_engine_light());
        sim.clear_codes();
        assert!(!sim.check_engine_light());
        assert!(sim.trouble_codes().is_empty());
    }
}
