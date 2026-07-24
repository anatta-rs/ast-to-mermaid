//! Golden-corpus module: a linear forward chain 3+ hops deep, so the
//! impact view has a downstream half to prove it renders.

pub fn entry(x: f64) -> f64 {
    step_one(x)
}

fn step_one(x: f64) -> f64 {
    step_two(x) + 1.0
}

fn step_two(x: f64) -> f64 {
    step_three(x) * 2.0
}

fn step_three(x: f64) -> f64 {
    x.abs()
}
