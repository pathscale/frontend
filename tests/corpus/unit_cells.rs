// Corpus unit: generic cells. A trait with an associated type, an associated const and a
// default method that reads it, a generic struct, a blanket-style impl with a bound, a `where`
// clause, and a nested module that imports from its parent. `Q0` is the per-unit tag; see
// `unit_geometry.rs`.

pub trait CellQ0 {
    type Item;
    const WIDTH: u32;

    fn weight(&self) -> u32;

    fn wide(&self) -> u32 {
        self.weight() * Self::WIDTH
    }
}

pub struct UnitQ0 {
    pub mass: u32,
}

pub struct PairQ0<T> {
    pub left: T,
    pub right: T,
}

impl CellQ0 for UnitQ0 {
    type Item = u32;
    const WIDTH: u32 = 2;

    fn weight(&self) -> u32 {
        self.mass
    }
}

impl<T: CellQ0> CellQ0 for PairQ0<T> {
    type Item = T;
    const WIDTH: u32 = 4;

    fn weight(&self) -> u32 {
        self.left.weight() + self.right.weight()
    }
}

impl<T> PairQ0<T> {
    pub fn new(left: T, right: T) -> PairQ0<T> {
        PairQ0 { left, right }
    }

    pub fn left(&self) -> &T {
        &self.left
    }
}

pub fn heavier_Q0<T>(a: &T, b: &T) -> bool
where
    T: CellQ0,
{
    a.weight() > b.weight()
}

pub mod nested_Q0 {
    use super::{CellQ0, PairQ0, UnitQ0};

    pub fn build(mass: u32) -> PairQ0<UnitQ0> {
        PairQ0::new(UnitQ0 { mass }, UnitQ0 { mass: mass + 1 })
    }

    pub fn deep(mass: u32) -> PairQ0<PairQ0<UnitQ0>> {
        PairQ0::new(build(mass), build(mass + 2))
    }

    pub fn score(mass: u32) -> u32 {
        let d = deep(mass);
        d.wide() + d.left().weight()
    }
}

pub fn contest_Q0(a: u32, b: u32) -> u32 {
    let x = nested_Q0::build(a);
    let y = nested_Q0::build(b);
    if heavier_Q0(&x, &y) { nested_Q0::score(a) } else { nested_Q0::score(b) }
}
