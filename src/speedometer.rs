#![allow(dead_code)]

use std::collections::VecDeque;

// Speedometer trait
pub trait Speedometer {
    /// Get the current speed in messages per second.
    fn get_speed(&self) -> f64;
    /// Add a measurement to the speedometer.
    /// The duration is in milliseconds and the number of messages is the number of messages processed in that duration.
    fn add_measurement(&mut self, duration: u128, msgs: u128);
}

pub struct InstantSpeedometer {
    speed: f64,
}
impl InstantSpeedometer {
    pub fn new() -> Self {
        Self { speed: 0.0 }
    }
}
impl Default for InstantSpeedometer {
    fn default() -> Self {
        Self::new()
    }
}
impl InstantSpeedometer {
    /// Get the current speed in messages per second.
    pub fn get_speed(&self) -> f64 {
        self.speed
    }
}
impl Speedometer for InstantSpeedometer {
    fn get_speed(&self) -> f64 {
        self.speed
    }

    fn add_measurement(&mut self, duration: u128, msgs: u128) {
        self.speed = msgs as f64 * 1000.0 / duration as f64;
    }
}

struct RingbufferMeasurement {
    duration: u128,
    msgs: u128,
}
pub struct RingbufferSpeedometer {
    measurements: VecDeque<RingbufferMeasurement>,
}
impl Default for RingbufferSpeedometer {
    fn default() -> Self {
        Self::new(1024)
    }
}
impl RingbufferSpeedometer {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be greater than 0");
        assert!(capacity.is_power_of_two(), "Capacity must be a power of 2");
        let ring = VecDeque::with_capacity(capacity);
        assert!(ring.capacity() == capacity, "This is unexpected");
        Self { measurements: ring }
    }
}
impl Speedometer for RingbufferSpeedometer {
    fn get_speed(&self) -> f64 {
        if self.measurements.is_empty() {
            return 0.0;
        }
        let (time, msgs) = self
            .measurements
            .iter()
            .fold((0_u128, 0_u128), |state, elem| {
                (state.0 + elem.duration, state.1 + elem.msgs)
            });
        msgs as f64 * 1000.0 / (time as f64)
    }

    fn add_measurement(&mut self, duration: u128, msgs: u128) {
        if self.measurements.len() == self.measurements.capacity() {
            let _ = self.measurements.pop_front();
        }
        self.measurements
            .push_back(RingbufferMeasurement { duration, msgs });
    }
}

pub struct SmootherSpeedometer {
    speed: f64,
    smooth_factor: f64,
}
impl Default for SmootherSpeedometer {
    fn default() -> Self {
        Self::new(0.1)
    }
}
impl SmootherSpeedometer {
    pub fn new(smooth_factor: f64) -> Self {
        Self {
            speed: 0.0,
            smooth_factor,
        }
    }
}
impl Speedometer for SmootherSpeedometer {
    fn get_speed(&self) -> f64 {
        self.speed
    }

    fn add_measurement(&mut self, duration: u128, msgs: u128) {
        let new_speed = msgs as f64 * 1000.0 / duration as f64;
        self.speed = self.smooth_factor * new_speed + (1.0 - self.smooth_factor) * self.speed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ringbuffer_speedometer() {
        let mut speedometer = RingbufferSpeedometer::new(4);
        assert_eq!(speedometer.get_speed(), 0.0);
        speedometer.add_measurement(100, 10);
        assert_eq!(speedometer.get_speed(), 100.0);
        speedometer.add_measurement(150, 30);
        assert_eq!(speedometer.get_speed(), 160.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 32.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 17.777777777777778);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 9.523809523809524);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.0);
    }

    #[test]
    fn test_instant_speedometer() {
        let mut speedometer = InstantSpeedometer::new();
        assert_eq!(speedometer.get_speed(), 0.0);
        speedometer.add_measurement(100, 10);
        assert_eq!(speedometer.get_speed(), 100.0);
        speedometer.add_measurement(150, 30);
        assert_eq!(speedometer.get_speed(), 200.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.0);
    }

    #[test]
    fn test_smoother_speedometer() {
        let mut speedometer = SmootherSpeedometer::new(0.5);
        speedometer.add_measurement(100, 10);
        assert_eq!(speedometer.get_speed(), 50.0);
        speedometer.add_measurement(150, 30);
        assert_eq!(speedometer.get_speed(), 125.0);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 62.5);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 31.25);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 15.625);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 7.8125);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 3.90625);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 1.953125);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.9765625);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.48828125);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.244140625);
        speedometer.add_measurement(1000, 0);
        assert_eq!(speedometer.get_speed(), 0.1220703125);
    }
}
