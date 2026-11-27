//! A small Rust fixture.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// A limit.
pub const MAX: usize = 10;

static NAME: &str = "coqo";

pub mod shapes {
    pub trait Shape {
        fn area(&self) -> f64;
    }

    #[derive(Debug)]
    pub struct Circle {
        pub radius: f64,
    }

    impl Shape for Circle {
        fn area(&self) -> f64 {
            3.14 * self.radius * self.radius
        }
    }
}

pub enum Color {
    Red,
    Green(u8),
}

pub type Map = HashMap<String, Arc<str>>;

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Red => write!(f, "red"),
            Color::Green(n) => write!(f, "green {n}"),
        }
    }
}

/*
 * A block comment
 * over three lines.
 */
fn main() {
    let x = square!(3);
    println!("{x} {NAME} {MAX}");
}
