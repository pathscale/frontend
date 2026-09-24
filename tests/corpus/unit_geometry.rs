// Corpus unit: geometry. Structs, a tuple struct, an enum with every variant shape, a trait
// with default methods, inherent and trait impls, a generic function with a bound, a `while`
// loop and `&mut self`. Every name carries `Q0`, which `examples/parallel_timing.rs` replaces
// with a per-file, per-unit tag, so one file can hold many copies and no two files are alike.

pub struct PointQ0 {
    pub x: u32,
    pub y: u32,
}

pub struct SizeQ0(pub u32, pub u32);

pub enum ShapeQ0 {
    Dot(PointQ0),
    Line(PointQ0, PointQ0),
    Rect { corner: PointQ0, size: SizeQ0 },
    Empty,
}

pub trait MeasureQ0 {
    fn size(&self) -> u32;

    fn twice(&self) -> u32 {
        let a = self.size();
        a + a
    }

    fn at_least(&self, floor: u32) -> u32 {
        let s = self.size();
        if s < floor { floor } else { s }
    }
}

impl PointQ0 {
    pub fn new(x: u32, y: u32) -> PointQ0 {
        PointQ0 { x, y }
    }

    pub fn origin() -> PointQ0 {
        PointQ0::new(0, 0)
    }

    pub fn flip(&self) -> PointQ0 {
        PointQ0::new(self.y, self.x)
    }

    pub fn shift(&mut self, dx: u32, dy: u32) {
        self.x = self.x + dx;
        self.y = self.y + dy;
    }

    pub fn manhattan(&self, other: &PointQ0) -> u32 {
        let dx = if self.x > other.x { self.x - other.x } else { other.x - self.x };
        let dy = if self.y > other.y { self.y - other.y } else { other.y - self.y };
        dx + dy
    }
}

impl MeasureQ0 for PointQ0 {
    fn size(&self) -> u32 {
        self.x + self.y
    }
}

impl MeasureQ0 for SizeQ0 {
    fn size(&self) -> u32 {
        self.0 * self.1
    }
}

impl MeasureQ0 for ShapeQ0 {
    fn size(&self) -> u32 {
        match self {
            ShapeQ0::Dot(p) => p.size(),
            ShapeQ0::Line(a, b) => a.manhattan(b),
            ShapeQ0::Rect { corner, size } => corner.size() + size.size(),
            ShapeQ0::Empty => 0,
        }
    }
}

pub fn larger_Q0<T: MeasureQ0>(a: &T, b: &T) -> u32 {
    let x = a.size();
    let y = b.size();
    if x > y { x } else { y }
}

pub fn walk_Q0(steps: u32) -> PointQ0 {
    let mut p = PointQ0::origin();
    let mut i: u32 = 0;
    while i < steps {
        if i * 2 < steps {
            p.shift(1, 0);
        } else {
            p.shift(0, 1);
        }
        i = i + 1;
    }
    p
}

pub fn shapes_Q0() -> u32 {
    let dot = ShapeQ0::Dot(PointQ0::new(1, 2));
    let line = ShapeQ0::Line(PointQ0::origin(), walk_Q0(9));
    let rect = ShapeQ0::Rect { corner: PointQ0::new(3, 4).flip(), size: SizeQ0(2, 5) };
    let empty = ShapeQ0::Empty;
    larger_Q0(&dot, &line) + rect.twice() + empty.at_least(4)
}
