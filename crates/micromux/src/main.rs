#![allow(warnings)]
use color_eyre::eyre;

fn main() -> eyre::Result<()> {
    micromux_tui::render()
}
