//! Канонический POLER-цикл (каузальный 2D-осциллятор внимания)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PolerParams {
    pub eta: f64,
    pub gamma: f64,
    pub mix: f64,
    pub delta: f64,
    pub dissipator: f64,
}

impl Default for PolerParams {
    fn default() -> Self {
        Self {
            eta: 0.01,
            gamma: 0.1,
            mix: 0.1,
            delta: 1e-10,
            dissipator: 0.02,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub p: f64,
    pub q: f64,
}

impl Vec2 {
    pub fn new(p: f64, q: f64) -> Self {
        Self { p, q }
    }

    fn dot(self, o: Vec2) -> f64 {
        self.p * o.p + self.q * o.q
    }

    fn norm_sq(self) -> f64 {
        self.dot(self)
    }
}

#[derive(Debug, Clone, Copy)]
struct Mat2 {
    m: [[f64; 2]; 2],
}

impl Mat2 {
    fn apply(&self, v: Vec2) -> Vec2 {
        Vec2::new(
            self.m[0][0] * v.p + self.m[0][1] * v.q,
            self.m[1][0] * v.p + self.m[1][1] * v.q,
        )
    }

    fn skew_part(a: &Mat2) -> Self {
        Mat2 {
            m: [
                [0.0, a.m[0][1] - a.m[1][0]],
                [a.m[1][0] - a.m[0][1], 0.0],
            ],
        }
    }
}

pub fn cordic_inv_sqrt(x: f64) -> f64 {
    if x <= 0.0 {
        return 1.0;
    }
    let bits = x.to_bits();
    let guess = f64::from_bits(0x5FE6EB50C7B537A9_u64.wrapping_sub(bits >> 1));
    let y1 = guess * (1.5 - 0.5 * x * guess * guess);
    let y2 = y1 * (1.5 - 0.5 * x * y1 * y1);
    y2 * (1.5 - 0.5 * x * y2 * y2)
}

pub struct PolerCycle {
    params: PolerParams,
    x: Vec2,
    osc_prev: f64,
    omega: f64,
}

impl PolerCycle {
    pub fn new(params: PolerParams) -> Self {
        Self {
            params,
            x: Vec2::new(0.0, 0.0),
            osc_prev: 0.0,
            omega: 0.0,
        }
    }

    fn projector(jc: Vec2, delta: f64) -> Mat2 {
        let jjt = jc.dot(jc) + delta;
        Mat2 {
            m: [
                [1.0 - jc.p * jc.p / jjt, -jc.p * jc.q / jjt],
                [-jc.q * jc.p / jjt, 1.0 - jc.q * jc.q / jjt],
            ],
        }
    }

    pub fn step(&mut self, o_t: f64, allowed: bool) -> Vec2 {
        let params = self.params;
        let obs = o_t.tanh();
        let target = Vec2::new(obs, 0.0);

        let d = params.dissipator;
        let dissip = Mat2 {
            m: [[d * d, 0.0], [0.0, d * d]],
        };

        let delta_obs = obs - self.osc_prev;
        self.osc_prev = obs;
        self.omega = 0.9 * self.omega + 0.1 * (delta_obs * 10.0).clamp(-1.0, 1.0);
        let a = Mat2 {
            m: [[0.0, -self.omega], [self.omega, 0.0]],
        };
        let resonance = Mat2::skew_part(&a);

        let grad_f = Vec2::new(2.0 * (self.x.p - target.p), 2.0 * (self.x.q - target.q));
        let dx = dissip.apply(self.x);
        let jx = resonance.apply(self.x);
        let force = Vec2::new(
            dx.p + params.gamma * jx.p + grad_f.p,
            dx.q + params.gamma * jx.q + grad_f.q,
        );

        let (projected, _used_projection) = if allowed {
            (force, false)
        } else {
            let norm = force.norm_sq().sqrt().max(1e-12);
            let jc = Vec2::new(force.p / norm, force.q / norm);
            (Self::projector(jc, params.delta).apply(force), true)
        };

        let p_new = Vec2::new(
            self.x.p - params.eta * projected.p,
            self.x.q - params.eta * projected.q,
        );

        let r2 = p_new.norm_sq();
        let scale = if r2 > 1e-12 {
            cordic_inv_sqrt(r2)
        } else {
            1.0
        };
        let p_norm = Vec2::new(p_new.p * scale, p_new.q * scale);

        self.x = Vec2::new(
            (1.0 - params.mix) * p_new.p + params.mix * p_norm.p,
            (1.0 - params.mix) * p_new.q + params.mix * p_norm.q,
        );

        self.x
    }

    #[inline]
    pub fn state(&self) -> Vec2 {
        self.x
    }
}
